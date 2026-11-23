//! `get_tree`: the fused compositor-window ↔ AT-SPI2 accessibility tree
//! snapshot (M3.2).
//!
//! The verb snapshots the workspace's mapped windows, connects to the
//! workspace's private a11y bus, correlates registered applications to
//! windows by pid, walks each matched subtree under fused absolute
//! coordinates and stores the bounded result as the workspace's next
//! `snapshot_id` — the basis for M3.4's staleness-checked refs and M3.5's
//! `tree_find`/`tree_diff`.

use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};

use crate::{
    WindowId,
    daemon::{
        a11y,
        protocol::{ErrorResponse, code},
        registry::Workspace,
        router::{Call, MethodDef, Scope},
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
    let window_filter = call
        .params
        .get("window")
        .or_else(|| call.params.get("window_id"))
        .and_then(Value::as_u64)
        .map(WindowId);
    let deadline = call
        .params
        .get("deadline_ms")
        .or_else(|| call.params.get("timeout_ms"))
        .and_then(Value::as_u64)
        .map(Duration::from_millis);

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

    let capture = tokio::select! {
        _ = call.cancel.cancelled() => {
            return Err(ErrorResponse::with_data(
                code::CANCELLED,
                "get_tree cancelled: client disconnected",
                json!({"reason": "disconnected"}),
            ));
        }
        capture = a11y::capture(ws.a11y_address(), &windows, deadline) => {
            capture.map_err(|message| {
                ErrorResponse::with_data(
                    code::TIMEOUT,
                    message,
                    json!({"verb": "get_tree"}),
                )
            })?
        }
    };

    let snapshot = ws.record_tree_snapshot(capture.windows);
    Ok(json!({
        "workspace": ws.id(),
        "snapshot_id": snapshot.id,
        "a11y": capture.bus,
        "windows": snapshot.windows,
        "seq": reply.seq,
    }))
}
