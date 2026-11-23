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

    // `root_ref` resolves against the requested snapshot (or the
    // latest): unknown/evicted refs fail as param errors, a live
    // identity drift fails with `STALE_REF` — the same contract
    // `click_node`/`node_action` use.
    let subtree = match &root_ref {
        Some(node_ref) => Some(resolve_root_ref(&ws, node_ref, snapshot_id)?),
        None => None,
    };

    let reply = ws.call(|handle| handle.windows()).await?;
    let mut windows = reply.value;
    if let Some((snapshot, sub)) = &subtree {
        if let Some(id) = window_filter
            && id != sub.window_id
        {
            return Err(ErrorResponse::invalid_params(format!(
                "window {id:?} does not contain root_ref: the ref lives in window {:?}",
                sub.window_id
            )));
        }
        if !windows.iter().any(|w| w.id == sub.window_id) {
            // The window the ref fused into is gone — same staleness
            // contract as `click_node`'s dead-window case.
            return Err(stale_ref_error(
                root_ref.as_deref().unwrap_or_default(),
                snapshot.id,
                sub,
                "missing",
                "the window the ref was captured in no longer exists",
                None,
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
            return Err(cancelled("get_tree", reason));
        }
        capture = a11y::capture(
            ws.a11y_address(),
            &windows,
            &limits,
            subtree.as_ref().map(|(_, sub)| sub),
        ) => {
            capture.map_err(|err| match err {
                a11y::CaptureError::Deadline => ErrorResponse::with_data(
                    code::TIMEOUT,
                    "get_tree exceeded its deadline: an AT-SPI2 peer stopped answering",
                    json!({"verb": "get_tree", "deadline_ms": limits.deadline.as_millis() as u64}),
                ),
                a11y::CaptureError::StaleRoot {
                    mismatch,
                    actual,
                } => {
                    let (snapshot, sub) = subtree.as_ref().expect("stale root implies a subtree");
                    stale_ref_error(
                        root_ref.as_deref().unwrap_or_default(),
                        snapshot.id,
                        sub,
                        mismatch,
                        &match &actual {
                            Some((role, name)) => format!(
                                "its identity changed (was {:?} {:?}, now {role:?} {name:?})",
                                sub.role, sub.name,
                            ),
                            None => "the node no longer exists".to_string(),
                        },
                        actual,
                    )
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
pub(crate) fn bounded_param(params: &Value, name: &str) -> Result<Option<usize>, ErrorResponse> {
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
/// unknown/evicted snapshots are param errors — matching `click_node`/
/// `node_action`'s `resolve_ref` contract.
fn resolve_root_ref(
    ws: &Workspace,
    node_ref: &str,
    snapshot_id: Option<u64>,
) -> Result<(a11y::TreeSnapshot, SubtreeRoot), ErrorResponse> {
    let snapshot = match snapshot_id {
        Some(id) => ws.tree_snapshot(id).ok_or_else(|| {
            ErrorResponse::invalid_params_data(
                format!("snapshot {id} is unknown or has been evicted"),
                json!({
                    "snapshot": id,
                    "hint": "run get_tree to capture a fresh snapshot",
                }),
            )
        })?,
        None => ws.latest_tree_snapshot().ok_or_else(|| {
            ErrorResponse::invalid_params(format!(
                "root_ref {node_ref:?} cannot be resolved: no get_tree snapshot exists yet; \
                 run get_tree first"
            ))
        })?,
    };
    let target = snapshot.lookup_ref(node_ref).ok_or_else(|| {
        ErrorResponse::invalid_params_data(
            format!(
                "root_ref {node_ref:?} does not exist in snapshot {}",
                snapshot.id
            ),
            json!({
                "ref": node_ref,
                "snapshot_id": snapshot.id,
                "hint": "run get_tree for fresh refs",
            }),
        )
    })?;
    Ok((
        snapshot,
        SubtreeRoot {
            window_id: target.window_id,
            bus_name: target.bus_name,
            object_path: target.object_path,
            role: target.role,
            name: target.name,
            anchor: target.bounds,
        },
    ))
}

/// The structured `STALE_REF` failure for a drifted subtree root — same
/// shape `click_node`/`node_action` report through `stale_error`.
fn stale_ref_error(
    node_ref: &str,
    snapshot_id: u64,
    sub: &SubtreeRoot,
    mismatch: &'static str,
    drift: &str,
    actual: Option<(String, String)>,
) -> ErrorResponse {
    ErrorResponse::with_data(
        code::STALE_REF,
        format!(
            "ref {node_ref:?} is stale: {drift}; run get_tree to re-snapshot and use a fresh ref"
        ),
        json!({
            "ref": node_ref,
            "snapshot_id": snapshot_id,
            "mismatch": mismatch,
            "bus_name": sub.bus_name,
            "path": sub.object_path,
            "expected": {"role": sub.role, "name": sub.name},
            "actual": actual.map(|(role, name)| json!({"role": role, "name": name})),
            "hint": "run get_tree to re-snapshot and use fresh refs",
        }),
    )
}

/// A `CANCELLED` error for an aborted capture.
pub(crate) fn cancelled(verb: &'static str, reason: &'static str) -> ErrorResponse {
    ErrorResponse::with_data(
        code::CANCELLED,
        format!("{verb} cancelled: {reason}"),
        json!({"verb": verb, "reason": reason}),
    )
}

/// Resolves to the cancellation reason as soon as the connection drops,
/// the workspace starts teardown, or a watched pid exits.
pub(crate) async fn cancel_reason(
    cancel: &CancellationToken,
    ws: &Workspace,
    pids: &[u32],
) -> &'static str {
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
