//! `get_tree`: the fused compositor-window ↔ AT-SPI2 accessibility tree
//! snapshot (M3.2), bounded and redacted (M3.3).
//!
//! The verb snapshots the workspace's mapped windows, connects to the
//! workspace's private a11y bus, correlates registered applications to
//! windows by pid, walks each matched subtree under fused absolute
//! coordinates and stores the bounded result as the workspace's next
//! `snapshot_id` — the basis for M3.4's staleness-checked refs and M3.5's
//! `tree_find`/`tree_diff`.
//!
//! M3.3 hardens the walk: `max_depth`/`max_nodes`/`deadline_ms` bounds
//! degrade open branches into inline `- ... <reason>` markers plus a
//! structured `truncation` summary; `root_ref` scopes the capture to one
//! previously snapshotted node (with a live staleness re-check);
//! `ROLE_PASSWORD_TEXT` text/value are redacted; and socket disconnect or
//! `client_pid` death cancels the capture mid-flight.

use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    WindowId,
    daemon::{
        a11y::{self, Limits, SubtreeRoot},
        pid_alive,
        protocol::{ErrorResponse, code},
        registry::Workspace,
        router::{Call, MethodDef, Scope},
        watched_pids,
    },
};

/// `get_tree` — fused window metadata + accessibility trees (read-only,
/// allowed on `readOnly` workspaces).
pub const GET_TREE: MethodDef = MethodDef {
    name: "get_tree",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .clone()
                .expect("workspace scope resolves a workspace");
            get_tree(ws, call).await
        })
    },
};

async fn get_tree(ws: Arc<Workspace>, call: Call) -> Result<Value, ErrorResponse> {
    let params = &call.params;
    let window_filter = params
        .get("window")
        .or_else(|| params.get("window_id"))
        .and_then(Value::as_u64)
        .map(WindowId);
    let deadline_ms = params
        .get("deadline_ms")
        .or_else(|| params.get("timeout_ms"))
        .and_then(Value::as_u64);
    let max_depth = bounded_param(params, "max_depth")?;
    let max_nodes = bounded_param(params, "max_nodes")?;
    let root_ref = params
        .get("root_ref")
        .or_else(|| params.get("root"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let snapshot_id = params
        .get("snapshot")
        .or_else(|| params.get("snapshot_id"))
        .and_then(Value::as_u64);
    let client_pid = params
        .get("client_pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32);

    let mut limits = Limits::default();
    if let Some(depth) = max_depth {
        limits.max_depth = depth;
    }
    if let Some(nodes) = max_nodes {
        limits.max_nodes = nodes;
    }
    if let Some(ms) = deadline_ms {
        limits.deadline = Duration::from_millis(ms.max(1));
    }

    // `root_ref` resolves against the requested snapshot (or the latest):
    // unknown/evicted refs fail fast with `STALE_REF` and a re-snapshot
    // hint — same contract M3.4's action verbs use.
    let subtree = match &root_ref {
        Some(node_ref) => Some(resolve_root_ref(&ws, node_ref, snapshot_id)?),
        None => None,
    };

    let reply = ws.call(|handle| handle.windows()).await?;
    let mut windows = reply.value;
    if let Some(sub) = &subtree {
        if let Some(id) = window_filter
            && id != sub.window_id
        {
            return Err(ErrorResponse::invalid_params(format!(
                "window {id:?} does not contain root_ref: the ref lives in window {:?}",
                sub.window_id
            )));
        }
        if !windows.iter().any(|w| w.id == sub.window_id) {
            return Err(stale_ref(
                root_ref.as_deref().unwrap_or_default(),
                "the window the ref was captured in is gone",
            ));
        }
        windows.retain(|w| w.id == sub.window_id);
    } else if let Some(id) = window_filter {
        if !windows.iter().any(|w| w.id == id) {
            return Err(ErrorResponse::with_data(
                code::UNKNOWN_WINDOW,
                format!("unknown window id: {id:?}"),
                json!({"window_id": id.0}),
            ));
        }
        windows.retain(|w| w.id == id);
    }

    // Cancellation: socket disconnect, workspace teardown, or death of a
    // watched pid (`client_pid` param / `SO_PEERCRED` peer) all abort the
    // capture so a slow or hung AT-SPI2 peer can never wedge the daemon.
    let pids = watched_pids(client_pid, call.peer_pid);
    let capture = tokio::select! {
        reason = cancel_reason(&call.cancel, &ws, &pids) => {
            return Err(cancelled(reason));
        }
        capture = a11y::capture(ws.a11y_address(), &windows, &limits, subtree.as_ref()) => {
            capture.map_err(|err| match err {
                a11y::CaptureError::Deadline => ErrorResponse::with_data(
                    code::TIMEOUT,
                    "get_tree exceeded its deadline: an AT-SPI2 peer stopped answering",
                    json!({"verb": "get_tree", "deadline_ms": limits.deadline.as_millis() as u64}),
                ),
                a11y::CaptureError::StaleRoot {
                    expected_role,
                    expected_name,
                    actual,
                } => {
                    let reason = match &actual {
                        Some((role, name)) => format!(
                            "the node changed (was {expected_role:?} {expected_name:?}, now {role:?} {name:?})"
                        ),
                        None => "the node is gone".to_string(),
                    };
                    stale_ref(root_ref.as_deref().unwrap_or_default(), &reason)
                }
            })?
        }
    };

    let snapshot = ws.record_tree_snapshot(capture.windows);
    let mut out = json!({
        "workspace": ws.id(),
        "snapshot_id": snapshot.id,
        "a11y": capture.bus,
        "limits": {
            "max_depth": limits.max_depth,
            "max_nodes": limits.max_nodes,
            "deadline_ms": limits.deadline.as_millis() as u64,
        },
        "truncation": capture.truncation,
        "windows": snapshot.windows,
        "seq": reply.seq,
    });
    if subtree.is_some()
        && let Some(node_ref) = &root_ref
    {
        out["root_ref"] = json!(node_ref);
    }
    Ok(out)
}

/// Reads `name` as a positive integer bound (`≥1`), defaulting to `None`.
fn bounded_param(params: &Value, name: &str) -> Result<Option<usize>, ErrorResponse> {
    match params.get(name).and_then(Value::as_u64) {
        None => Ok(None),
        Some(0) => Err(ErrorResponse::invalid_params(format!(
            "{name} must be at least 1"
        ))),
        Some(value) => Ok(Some(value as usize)),
    }
}

/// Resolves a `root_ref` (`eN`) to its staleness identity via the
/// requested snapshot (or the workspace's latest). Unknown refs and
/// evicted snapshots fail with `STALE_REF` plus a re-snapshot hint.
fn resolve_root_ref(
    ws: &Workspace,
    node_ref: &str,
    snapshot_id: Option<u64>,
) -> Result<SubtreeRoot, ErrorResponse> {
    let snapshot = match snapshot_id {
        Some(id) => ws.tree_snapshot(id).ok_or_else(|| {
            stale_ref(
                node_ref,
                &format!(
                    "snapshot {id} is not retained (the workspace keeps the last {}): take a fresh get_tree snapshot",
                    a11y::SNAPSHOT_HISTORY
                ),
            )
        })?,
        None => ws.latest_tree_snapshot().ok_or_else(|| {
            stale_ref(
                node_ref,
                "no get_tree snapshot exists yet: take one first",
            )
        })?,
    };
    let target = snapshot.lookup_ref(node_ref).ok_or_else(|| {
        stale_ref(
            node_ref,
            &format!("no ref {node_ref:?} in snapshot {}", snapshot.id),
        )
    })?;
    Ok(SubtreeRoot {
        window_id: target.window_id,
        bus_name: target.bus_name,
        object_path: target.object_path,
        role: target.role,
        name: target.name,
        anchor: target.bounds,
    })
}

/// A `STALE_REF` error: unusable ref, with a re-snapshot hint.
fn stale_ref(node_ref: &str, reason: &str) -> ErrorResponse {
    ErrorResponse::with_data(
        code::STALE_REF,
        format!("tree ref {node_ref:?} is stale: {reason}; call get_tree to take a fresh snapshot"),
        json!({"ref": node_ref, "reason": reason, "hint": "call get_tree to take a fresh snapshot"}),
    )
}

/// A `CANCELLED` error for an aborted capture.
fn cancelled(reason: &'static str) -> ErrorResponse {
    ErrorResponse::with_data(
        code::CANCELLED,
        format!("get_tree cancelled: {reason}"),
        json!({"verb": "get_tree", "reason": reason}),
    )
}

/// Resolves to the cancellation reason as soon as the connection drops,
/// the workspace starts teardown, or a watched pid exits.
async fn cancel_reason(cancel: &CancellationToken, ws: &Workspace, pids: &[u32]) -> &'static str {
    let mut liveness = tokio::time::interval(crate::daemon::LIVENESS_INTERVAL);
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let ws_closing = ws.closing_token();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return "client_disconnected",
            _ = ws_closing.cancelled() => return "workspace_closed",
            _ = liveness.tick() => {
                // Keep the idle reaper from reaping a live capture.
                ws.touch();
                if !pids.iter().all(|&pid| pid_alive(pid)) {
                    return "client_pid_exited";
                }
            }
        }
    }
}
