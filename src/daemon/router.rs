//! The JSON-RPC method table and request dispatch.
//!
//! Every control verb registers one [`MethodDef`] here. Dispatch resolves
//! workspace scoping and enforces `readOnly` centrally, so verb handlers
//! only contain their own logic. New verbs (input, spawn, waits, diffs,
//! CLI-facing verbs) append a `MethodDef` and a handler — usually in their
//! own `methods/*.rs` module.

use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::{Value, json};

use crate::daemon::{
    DaemonContext,
    protocol::{ErrorResponse, Request, code},
    registry::Workspace,
};

/// What a verb needs resolved before its handler runs.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    /// Operates on the daemon as a whole (`workspace_list`, `status`, …).
    Daemon,
    /// Operates inside one workspace: the `workspace` param is resolved to
    /// a live workspace and `mutating` verbs are rejected on `readOnly`
    /// workspaces before the handler ever runs.
    Workspace {
        /// Whether the verb mutates workspace state.
        mutating: bool,
    },
}

/// The future a handler returns.
type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, ErrorResponse>> + Send + 'a>>;

/// One registered control verb.
pub struct MethodDef {
    /// The JSON-RPC `method` name.
    pub name: &'static str,
    /// Dispatch scope (daemon-wide vs workspace-scoped + mutation flag).
    pub scope: Scope,
    /// The handler: context, raw `params`, resolved workspace (for
    /// workspace-scoped verbs).
    pub run: fn(Arc<DaemonContext>, Value, Option<Arc<Workspace>>) -> HandlerFuture<'static>,
}

/// All registered verbs. Follow-on milestones append their own entries.
pub const METHODS: &[MethodDef] = &[
    crate::daemon::methods::workspace::CREATE,
    crate::daemon::methods::workspace::LIST,
    crate::daemon::methods::workspace::CLOSE,
    crate::daemon::methods::output::ADD,
    crate::daemon::methods::output::LIST,
    crate::daemon::methods::output::REMOVE,
    crate::daemon::methods::input::CLICK,
    crate::daemon::methods::input::DRAG,
    crate::daemon::methods::input::TYPE_TEXT,
    crate::daemon::methods::input::KEY_COMBO,
    crate::daemon::methods::input::CLIPBOARD_GET,
    crate::daemon::methods::input::CLIPBOARD_SET,
];

/// Dispatches one request to its verb and wraps the payload in the standard
/// `command_id`/`seq` envelope.
pub async fn dispatch(ctx: &Arc<DaemonContext>, request: Request) -> Result<Value, ErrorResponse> {
    let def = METHODS
        .iter()
        .find(|def| def.name == request.method)
        .ok_or_else(|| {
            ErrorResponse::with_data(
                code::METHOD_NOT_FOUND,
                format!("unknown method {:?}", request.method),
                json!({"method": request.method}),
            )
        })?;

    // Workspace scoping resolves the target and applies the readOnly gate
    // before any handler logic runs.
    let workspace = match def.scope {
        Scope::Daemon => None,
        Scope::Workspace { mutating } => {
            let id = workspace_param(&request.params)?;
            let ws = ctx.registry.get(&id)?;
            if mutating && ws.options().read_only {
                return Err(ErrorResponse::with_data(
                    code::READ_ONLY,
                    format!(
                        "workspace {id:?} was created readOnly: {name} is not allowed",
                        name = def.name,
                        id = ws.id(),
                    ),
                    json!({"workspace": ws.id(), "method": def.name}),
                ));
            }
            ws.touch();
            Some(ws)
        }
    };

    let payload = (def.run)(ctx.clone(), request.params, workspace.clone()).await?;
    let fallback_seq = workspace.as_ref().map(|ws| ws.seq());
    Ok(crate::daemon::protocol::envelope(
        request.id.unwrap_or(Value::Null),
        payload,
        fallback_seq,
    ))
}

/// Extracts the required `workspace` (or `id`) slug from `params`.
fn workspace_param(params: &Value) -> Result<String, ErrorResponse> {
    params
        .get("workspace")
        .or_else(|| params.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            ErrorResponse::new(
                code::INVALID_PARAMS,
                "missing required \"workspace\" parameter",
            )
        })
}
