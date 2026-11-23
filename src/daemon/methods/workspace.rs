//! Workspace lifecycle verbs: `workspace_create`, `workspace_list`,
//! `workspace_close`.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::{
    DaemonContext,
    protocol::{ErrorResponse, code},
    registry::WorkspaceOptions,
    router::{MethodDef, Scope},
};

fn invalid_params(err: impl std::fmt::Display) -> ErrorResponse {
    ErrorResponse::new(code::INVALID_PARAMS, format!("invalid params: {err}"))
}

/// `workspace_create` — idempotent creation keyed by an agent-chosen slug.
pub const CREATE: MethodDef = MethodDef {
    name: "workspace_create",
    scope: Scope::Daemon,
    run: |call| Box::pin(create(call.ctx, call.params)),
};

/// `workspace_list` — every live workspace with its current seq.
pub const LIST: MethodDef = MethodDef {
    name: "workspace_list",
    scope: Scope::Daemon,
    run: |call| Box::pin(list(call.ctx)),
};

/// `workspace_close` — tear a workspace down (allowed on `readOnly`).
pub const CLOSE: MethodDef = MethodDef {
    name: "workspace_close",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            // The Arc this handler holds keeps the record alive while
            // `close` removes it from the registry and tears down its
            // compositor.
            call.ctx.registry.close(ws.id(), "client_request").await?;
            Ok(json!({ "closed": ws.id() }))
        })
    },
};

#[derive(Deserialize)]
struct CreateParams {
    /// The agent-chosen workspace slug (`[A-Za-z0-9_-]{1,64}`).
    #[serde(alias = "workspace")]
    id: String,
    #[serde(flatten)]
    options: WorkspaceOptions,
}

async fn create(ctx: Arc<DaemonContext>, params: Value) -> Result<Value, ErrorResponse> {
    let params: CreateParams = serde_json::from_value(params).map_err(invalid_params)?;
    let created = ctx.registry.create(&params.id, params.options).await?;
    Ok(json!({
        "created": created.created,
        "workspace": created.workspace.summary(),
        "seq": created.workspace.seq(),
    }))
}

async fn list(ctx: Arc<DaemonContext>) -> Result<Value, ErrorResponse> {
    Ok(json!({ "workspaces": ctx.registry.list() }))
}
