//! `tree_find` and `tree_diff`: low-token observation verbs over the
//! bounded, password-redacted canonical tree representation (M3.5).
//!
//! Both verbs capture a fresh bounded snapshot through the same
//! cancellation/deadline machinery `get_tree` uses, then answer from the
//! canonical line rendering ([`crate::daemon::canon`]):
//!
//! - `tree_find` filters nodes by optional `window_id`, `role`, `name` /
//!   `text` (substring by default, `match: "exact"` for equality) and
//!   `state`, returning each match's ref, fused bounds and ±`context`
//!   canonical lines inside its window block.
//! - `tree_diff` runs a line-oriented LCS between a retained
//!   `from_snapshot` (default: the workspace's latest) and the fresh
//!   capture, emitting compact `added`/`removed`/`changed` hunks and the
//!   new `snapshot_id`. An unknown or evicted `from_snapshot` fails with
//!   `SNAPSHOT_EVICTED` and a resync hint.
//!
//! Both are read-only (allowed on `readOnly` workspaces) and preserve
//! `ROLE_PASSWORD_TEXT` redaction unconditionally: redacted nodes only
//! ever carry the `[REDACTED]` sentinel in snapshot state.

use std::{sync::Arc, time::Duration};

use serde_json::{Map, Value, json};

use crate::{
    WindowId,
    daemon::{
        a11y::{self, Limits},
        canon::{self, FindQuery},
        methods::tree::{bounded_param, cancel_reason, cancelled},
        protocol::{ErrorResponse, code},
        registry::Workspace,
        router::{Call, MethodDef, Scope},
        watched_pids,
    },
};

/// `tree_find` — search the fused tree by role/name/text/state with
/// context lines (read-only).
pub const TREE_FIND: MethodDef = MethodDef {
    name: "tree_find",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .clone()
                .expect("workspace scope resolves a workspace");
            tree_find(ws, call).await
        })
    },
};

/// `tree_diff` — LCS diff of a retained baseline snapshot against the
/// live tree (read-only).
pub const TREE_DIFF: MethodDef = MethodDef {
    name: "tree_diff",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .clone()
                .expect("workspace scope resolves a workspace");
            tree_diff(ws, call).await
        })
    },
};

/// Default `tree_find` context: canonical lines before/after each match.
const DEFAULT_CONTEXT: usize = 2;

/// The capture+window-filter half every observation verb shares: list
/// windows, narrow to `window_filter` (erroring `UNKNOWN_WINDOW`), then
/// run `a11y::capture` under disconnect/`client_pid`/teardown
/// cancellation and the deadline envelope. Returns the capture and the
/// compositor seq the window list was read at.
async fn observe_capture(
    ws: &Arc<Workspace>,
    call: &Call,
    verb: &'static str,
    window_filter: Option<WindowId>,
    limits: &Limits,
) -> Result<(a11y::Capture, u64), ErrorResponse> {
    let reply = ws.call(|handle| handle.windows()).await?;
    let mut windows = reply.value;
    if let Some(id) = window_filter {
        if !windows.iter().any(|w| w.id == id) {
            return Err(ErrorResponse::with_data(
                code::UNKNOWN_WINDOW,
                format!("unknown window id: {id:?}"),
                json!({"window_id": id.0}),
            ));
        }
        windows.retain(|w| w.id == id);
    }
    let client_pid = call
        .params
        .get("client_pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32);
    let pids = watched_pids(client_pid, call.peer_pid);
    let capture = tokio::select! {
        reason = cancel_reason(&call.cancel, ws, &pids) => {
            return Err(cancelled(verb, reason));
        }
        capture = a11y::capture(ws.a11y_address(), &windows, limits, None) => {
            capture.map_err(|err| match err {
                a11y::CaptureError::Deadline => ErrorResponse::with_data(
                    code::TIMEOUT,
                    format!("{verb} exceeded its deadline: an AT-SPI2 peer stopped answering"),
                    json!({"verb": verb, "deadline_ms": limits.deadline.as_millis() as u64}),
                ),
                // Observe verbs never pass a subtree scope, so the
                // stale-root path cannot trigger.
                a11y::CaptureError::StaleRoot { .. } => ErrorResponse::new(
                    code::INTERNAL,
                    format!("{verb} reported a stale root with no subtree scope"),
                ),
            })?
        }
    };
    Ok((capture, reply.seq))
}

/// The optional `window`/`window_id` scope parameter.
fn window_filter(params: &Value) -> Option<WindowId> {
    params
        .get("window")
        .or_else(|| params.get("window_id"))
        .and_then(Value::as_u64)
        .map(WindowId)
}

/// `max_depth`/`max_nodes`/`deadline_ms` overrides on top of
/// `Limits::default()` — the same contract `get_tree` exposes.
fn observe_limits(params: &Value) -> Result<Limits, ErrorResponse> {
    let mut limits = Limits::default();
    if let Some(depth) = bounded_param(params, "max_depth")? {
        limits.max_depth = depth;
    }
    if let Some(nodes) = bounded_param(params, "max_nodes")? {
        limits.max_nodes = nodes;
    }
    if let Some(ms) = params
        .get("deadline_ms")
        .or_else(|| params.get("timeout_ms"))
        .and_then(Value::as_u64)
    {
        limits.deadline = Duration::from_millis(ms.max(1));
    }
    Ok(limits)
}

/// `state` accepts one state name or an array of names.
fn state_filter(params: &Value) -> Result<Vec<String>, ErrorResponse> {
    match params.get("state").or_else(|| params.get("states")) {
        None => Ok(Vec::new()),
        Some(Value::String(state)) => Ok(vec![state.clone()]),
        Some(Value::Array(states)) => states
            .iter()
            .map(|state| {
                state
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ErrorResponse::invalid_params("state entries must be strings"))
            })
            .collect(),
        Some(_) => Err(ErrorResponse::invalid_params(
            "state must be a state name or an array of state names",
        )),
    }
}

async fn tree_find(ws: Arc<Workspace>, call: Call) -> Result<Value, ErrorResponse> {
    let params = &call.params;
    let query = FindQuery {
        role: params
            .get("role")
            .and_then(Value::as_str)
            .map(str::to_string),
        name: params
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        text: params
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        exact: match params.get("match").and_then(Value::as_str) {
            None | Some("substring") => false,
            Some("exact") => true,
            Some(other) => {
                return Err(ErrorResponse::invalid_params(format!(
                    "match must be \"substring\" or \"exact\", got {other:?}"
                )));
            }
        },
        states: state_filter(params)?,
    };
    if !query.has_criteria() {
        return Err(ErrorResponse::invalid_params(
            "tree_find needs at least one filter: role, name, text or state",
        ));
    }
    let context = match params
        .get("context")
        .or_else(|| params.get("context_lines"))
        .and_then(Value::as_u64)
    {
        None => DEFAULT_CONTEXT,
        Some(n) => n as usize,
    };
    let limits = observe_limits(params)?;
    let (capture, seq) =
        observe_capture(&ws, &call, "tree_find", window_filter(params), &limits).await?;
    let snapshot = ws.record_tree_snapshot(capture.windows);
    let lines = canon::canonical_lines(&snapshot);

    let matches: Vec<Value> = canon::find_matches(&lines, &query)
        .into_iter()
        .map(|index| {
            let line = &lines[index];
            let node = line.node.as_ref().expect("match lines carry nodes");
            let (before, matched, after) = canon::context_lines(&lines, index, context);
            let mut entry = Map::new();
            entry.insert("ref".to_string(), json!(node.node_ref));
            entry.insert("role".to_string(), json!(node.role));
            entry.insert("name".to_string(), json!(node.name));
            entry.insert("states".to_string(), json!(node.states));
            if let Some(bounds) = node.bounds {
                entry.insert(
                    "bounds".to_string(),
                    serde_json::to_value(bounds).unwrap_or_default(),
                );
            }
            if let Some(text) = &node.text {
                entry.insert("text".to_string(), json!(text));
            }
            if let Some(value) = node.value {
                entry.insert(
                    "value".to_string(),
                    serde_json::to_value(value).unwrap_or_default(),
                );
            }
            if let Some(value_text) = &node.value_text {
                entry.insert("value_text".to_string(), json!(value_text));
            }
            if node.redacted {
                entry.insert("redacted".to_string(), json!(true));
            }
            entry.insert("window_id".to_string(), json!(line.window_id.0));
            entry.insert(
                "context".to_string(),
                json!({"before": before, "match": matched, "after": after}),
            );
            Value::Object(entry)
        })
        .collect();

    Ok(json!({
        "workspace": ws.id(),
        "snapshot_id": snapshot.id,
        "a11y": capture.bus,
        "limits": {
            "max_depth": limits.max_depth,
            "max_nodes": limits.max_nodes,
            "deadline_ms": limits.deadline.as_millis() as u64,
        },
        "truncation": capture.truncation,
        "query": {
            "role": query.role,
            "name": query.name,
            "text": query.text,
            "match": if query.exact { "exact" } else { "substring" },
            "state": query.states,
            "context": context,
            "window_id": window_filter(params).map(|id| id.0),
        },
        "match_count": matches.len(),
        "matches": matches,
        "seq": seq,
    }))
}

// -- tree_diff --------------------------------------------------------------------

/// The structured failure for an unusable `from_snapshot`: the id is
/// unknown, was evicted from the bounded history ring, or no baseline
/// exists at all — each with a `get_tree` resync hint (spec story 20).
fn snapshot_gone(id: Option<u64>) -> ErrorResponse {
    let detail = match id {
        Some(id) => {
            format!(
                "from_snapshot {id} is unknown or has been evicted from the bounded snapshot history"
            )
        }
        None => "no prior snapshot exists to diff against".to_string(),
    };
    ErrorResponse::with_data(
        code::SNAPSHOT_EVICTED,
        format!("{detail}; run get_tree to capture a fresh baseline"),
        json!({
            "from_snapshot": id,
            "hint": "run get_tree to capture a fresh baseline, then pass its snapshot_id as from_snapshot",
        }),
    )
}

async fn tree_diff(ws: Arc<Workspace>, call: Call) -> Result<Value, ErrorResponse> {
    let params = &call.params;
    let from = params
        .get("from_snapshot")
        .or_else(|| params.get("from"))
        .or_else(|| params.get("snapshot"))
        .and_then(Value::as_u64);
    // Resolve the baseline before capturing: an omitted `from_snapshot`
    // means the workspace's immediately preceding snapshot, which the
    // fresh capture must not overwrite first.
    let baseline = match from {
        Some(id) => ws
            .tree_snapshot(id)
            .ok_or_else(|| snapshot_gone(Some(id)))?,
        None => ws
            .latest_tree_snapshot()
            .ok_or_else(|| snapshot_gone(None))?,
    };
    let limits = observe_limits(params)?;
    let (capture, seq) = observe_capture(&ws, &call, "tree_diff", None, &limits).await?;
    let snapshot = ws.record_tree_snapshot(capture.windows);

    let text_of = |snapshot: &a11y::TreeSnapshot| -> Vec<String> {
        canon::canonical_lines(snapshot)
            .into_iter()
            .map(|line| line.text)
            .collect()
    };
    let old_lines = text_of(&baseline);
    let new_lines = text_of(&snapshot);
    let diff = canon::lcs_diff(&old_lines, &new_lines);

    Ok(json!({
        "workspace": ws.id(),
        "from_snapshot": baseline.id,
        "snapshot_id": snapshot.id,
        "a11y": capture.bus,
        "limits": {
            "max_depth": limits.max_depth,
            "max_nodes": limits.max_nodes,
            "deadline_ms": limits.deadline.as_millis() as u64,
        },
        "truncation": capture.truncation,
        "changed": diff.added > 0 || diff.removed > 0,
        "added_lines": diff.added,
        "removed_lines": diff.removed,
        "unchanged_lines": diff.unchanged,
        "hunks": diff.hunks.iter().map(|hunk| json!({
            "kind": hunk.kind.name(),
            "old_start": hunk.old_start,
            "new_start": hunk.new_start,
            "removed": hunk.removed,
            "added": hunk.added,
        })).collect::<Vec<_>>(),
        "seq": seq,
    }))
}
