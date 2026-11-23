//! Agent-observation verbs: `get_changes_since` structured diffs and named
//! timeline marks (`mark_set`, `mark_list`).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::{
    protocol::ErrorResponse,
    registry::Workspace,
    router::{MethodDef, Scope},
};

/// `get_changes_since` — compact deduplicated diff over `(seq, current]`.
pub const GET_CHANGES_SINCE: MethodDef = MethodDef {
    name: "get_changes_since",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            let params: ChangesParams =
                serde_json::from_value(call.params).map_err(ErrorResponse::invalid_params)?;
            let mut diff = ws.changes_since(params.seq);
            // The envelope's `seq` is the workspace's current seq: pin it to
            // the same watermark the diff was computed against.
            diff["seq"] = diff["current_seq"].clone();
            Ok(diff)
        })
    },
};

/// `mark_set` — attach a named mark at a fresh point on the workspace's seq
/// timeline (mutating; `readOnly` rejects it before this handler runs).
pub const MARK_SET: MethodDef = MethodDef {
    name: "mark_set",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            mark_set(ws, call.params).await
        })
    },
};

/// `mark_list` — every named mark on the timeline, in seq order.
pub const MARK_LIST: MethodDef = MethodDef {
    name: "mark_list",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            Ok(json!({ "marks": ws.marks() }))
        })
    },
};

#[derive(Deserialize)]
struct ChangesParams {
    /// The caller's last-seen seq; the diff covers `(seq, current_seq]`.
    #[serde(alias = "since", alias = "since_seq")]
    seq: u64,
}

#[derive(Deserialize)]
struct MarkSetParams {
    /// The caller-chosen label (1–128 chars, non-blank).
    name: String,
    /// Optional metadata echoed back by `mark_list` and diffs.
    #[serde(default)]
    metadata: Option<Value>,
}

async fn mark_set(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: MarkSetParams =
        serde_json::from_value(params).map_err(ErrorResponse::invalid_params)?;
    let name = params.name.trim();
    if name.is_empty() {
        return Err(ErrorResponse::invalid_params(
            "mark_set requires a non-empty \"name\"",
        ));
    }
    if name.chars().count() > 128 {
        return Err(ErrorResponse::invalid_params(
            "mark names are limited to 128 characters",
        ));
    }
    let mark = ws.set_mark(name.to_string(), params.metadata).await?;
    Ok(json!({
        "mark": mark,
        "seq": mark.seq,
    }))
}
