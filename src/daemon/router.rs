//! The JSON-RPC method table and request dispatch.
//!
//! Every control verb registers one [`MethodDef`] here. Dispatch resolves
//! workspace scoping and enforces `readOnly` centrally, so verb handlers
//! only contain their own logic. New verbs (input, spawn, waits, diffs,
//! CLI-facing verbs) append a `MethodDef` and a handler — usually in their
//! own `methods/*.rs` module.

use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

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

/// Per-connection request context, shared by every request the connection
/// spawns. Lets blocking verbs cancel when the caller goes away.
#[derive(Clone)]
pub struct RequestContext {
    /// Cancelled when the connection's read loop ends (client disconnect).
    pub cancel: CancellationToken,
    /// `SO_PEERCRED` pid of the connecting process, when the platform
    /// reports it.
    pub peer_pid: Option<u32>,
}

/// Everything a verb handler needs for one request.
pub struct Call {
    /// Shared daemon state.
    pub ctx: Arc<DaemonContext>,
    /// The request's raw `params` value.
    pub params: Value,
    /// The resolved target workspace for workspace-scoped verbs.
    pub workspace: Option<Arc<Workspace>>,
    /// Cancelled when the client's socket disconnects mid-request.
    /// Blocking verbs must treat this as immediate cancellation.
    pub cancel: CancellationToken,
    /// `SO_PEERCRED` pid of the connecting process, when reported.
    pub peer_pid: Option<u32>,
}

/// One registered control verb.
pub struct MethodDef {
    /// The JSON-RPC `method` name.
    pub name: &'static str,
    /// Dispatch scope (daemon-wide vs workspace-scoped + mutation flag).
    pub scope: Scope,
    /// The handler for one request.
    pub run: fn(Call) -> HandlerFuture<'static>,
}

/// All registered verbs. Follow-on milestones append their own entries.
pub const METHODS: &[MethodDef] = &[
    crate::daemon::methods::workspace::CREATE,
    crate::daemon::methods::workspace::LIST,
    crate::daemon::methods::workspace::CLOSE,
    crate::daemon::methods::output::ADD,
    crate::daemon::methods::output::LIST,
    crate::daemon::methods::output::REMOVE,
    crate::daemon::methods::spawn::SPAWN_APP,
    crate::daemon::methods::input::CLICK,
    crate::daemon::methods::input::DRAG,
    crate::daemon::methods::input::TYPE_TEXT,
    crate::daemon::methods::input::KEY_COMBO,
    crate::daemon::methods::input::CLIPBOARD_GET,
    crate::daemon::methods::input::CLIPBOARD_SET,
    crate::daemon::methods::windows::LIST_WINDOWS,
    crate::daemon::methods::windows::PLACE_WINDOW,
    crate::daemon::methods::windows::CAPTURE_WINDOW,
    crate::daemon::methods::windows::CAPTURE_WORKSPACE,
    crate::daemon::methods::windows::GET_DAMAGE,
    crate::daemon::methods::changes::GET_CHANGES_SINCE,
    crate::daemon::methods::changes::MARK_SET,
    crate::daemon::methods::changes::MARK_LIST,
    crate::daemon::methods::waits::WAIT_FOR_WINDOW,
    crate::daemon::methods::waits::WAIT_FOR_COMMIT,
    crate::daemon::methods::waits::WAIT_FOR_IDLE,
    crate::daemon::methods::tree::GET_TREE,
    crate::daemon::methods::observe::TREE_FIND,
    crate::daemon::methods::observe::TREE_DIFF,
    crate::daemon::methods::node::CLICK_NODE,
    crate::daemon::methods::node::NODE_ACTION,
    crate::daemon::methods::portal::SCRIPT_SET,
    crate::daemon::methods::portal::SCRIPT_CLEAR,
    crate::daemon::methods::portal::HISTORY,
    crate::daemon::methods::status::STATUS,
];

/// Dispatches one request to its verb and wraps the payload in the standard
/// `command_id`/`seq` envelope.
///
/// `conn` carries the connection-scoped cancellation token and peer pid so
/// blocking verbs can abort when the caller disappears.
pub async fn dispatch(
    ctx: &Arc<DaemonContext>,
    request: Request,
    conn: RequestContext,
) -> Result<Value, ErrorResponse> {
    let Request { id, method, params } = request;
    let def = METHODS
        .iter()
        .find(|def| def.name == method)
        .ok_or_else(|| {
            ErrorResponse::with_data(
                code::METHOD_NOT_FOUND,
                format!("unknown method {method:?}"),
                json!({"method": method}),
            )
        })?;

    // Workspace scoping resolves the target and applies the readOnly gate
    // before any handler logic runs.
    let workspace = match def.scope {
        Scope::Daemon => None,
        Scope::Workspace { mutating } => {
            let id = workspace_param(&params)?;
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

    let payload = (def.run)(Call {
        ctx: ctx.clone(),
        params,
        workspace: workspace.clone(),
        cancel: conn.cancel,
        peer_pid: conn.peer_pid,
    })
    .await?;
    let fallback_seq = workspace.as_ref().map(|ws| ws.seq());
    Ok(crate::daemon::protocol::envelope(
        id.unwrap_or(Value::Null),
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
        .ok_or_else(|| ErrorResponse::invalid_params("missing required \"workspace\" parameter"))
}
