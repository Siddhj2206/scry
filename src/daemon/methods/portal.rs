//! Portal control verbs: `portal_script_set`, `portal_script_clear` and
//! `portal_history` — the control-plane side of the workspace's scripted
//! XDG Desktop Portal stub (`crate::daemon::portal`).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::{
    protocol::ErrorResponse,
    registry::Workspace,
    router::{MethodDef, Scope},
};

/// `portal_script_set` — queue one scripted response for a portal
/// interface (mutating; `readOnly` rejects it before this handler runs).
pub const SCRIPT_SET: MethodDef = MethodDef {
    name: "portal_script_set",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            script_set(ws, call.params).await
        })
    },
};

/// `portal_script_clear` — drain queued scripts, for one interface or
/// all of them (mutating).
pub const SCRIPT_CLEAR: MethodDef = MethodDef {
    name: "portal_script_clear",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            script_clear(ws, call.params).await
        })
    },
};

/// `portal_history` — the recorded portal invocations (read-only; safe
/// on `readOnly` workspaces).
pub const HISTORY: MethodDef = MethodDef {
    name: "portal_history",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            history(ws, call.params).await
        })
    },
};

/// A scripted response code: a name (`"accept"`, `"cancel"`, `"close"`)
/// or the raw portal code.
#[derive(Deserialize)]
#[serde(untagged)]
enum ResponseSpec {
    /// One of the named codes.
    Named(String),
    /// A raw `Response` code (0, 1, 2).
    Code(u32),
}

impl ResponseSpec {
    /// The portal `Response` code.
    fn code(&self) -> Result<u32, String> {
        match self {
            Self::Code(code) => Ok(*code),
            Self::Named(name) => match name.to_ascii_lowercase().as_str() {
                "accept" | "accepted" | "ok" | "success" => Ok(0),
                "cancel" | "cancelled" | "canceled" | "reject" | "rejected" => Ok(1),
                "close" | "closed" | "ended" => Ok(2),
                other => Err(format!(
                    "unknown portal response {other:?}: \
                     use \"accept\", \"cancel\", \"close\" or a code 0–2"
                )),
            },
        }
    }
}

#[derive(Deserialize)]
struct ScriptSetParams {
    /// Which portal interface to script: `filechooser` or `openuri`
    /// (D-Bus names accepted too).
    interface: String,
    /// The response to emit (default `"accept"`).
    #[serde(default)]
    response: Option<ResponseSpec>,
    /// `file://` URIs to return on accept (the `uris` result).
    #[serde(default, alias = "uri")]
    uris: Vec<String>,
}

async fn script_set(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: ScriptSetParams =
        serde_json::from_value(params).map_err(ErrorResponse::invalid_params)?;
    let response = params
        .response
        .map(|spec| spec.code())
        .transpose()
        .map_err(ErrorResponse::invalid_params)?
        .unwrap_or(crate::daemon::portal::RESPONSE_ACCEPT);
    let queued = ws
        .portal()
        .script_set(&params.interface, response, params.uris.clone())?;
    Ok(json!({
        "interface": params.interface,
        "queued": queued,
        "response": response,
        "uris": params.uris,
    }))
}

#[derive(Deserialize)]
struct ScriptClearParams {
    /// Clear only this interface's queue (default: all interfaces).
    #[serde(default)]
    interface: Option<String>,
}

async fn script_clear(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: ScriptClearParams = if params.is_null() {
        ScriptClearParams { interface: None }
    } else {
        serde_json::from_value(params).map_err(ErrorResponse::invalid_params)?
    };
    let cleared = ws.portal().script_clear(params.interface.as_deref())?;
    Ok(json!({
        "cleared": cleared,
        "interface": params.interface,
    }))
}

#[derive(Deserialize)]
struct HistoryParams {
    /// Return only the newest N invocations.
    #[serde(default)]
    limit: Option<u64>,
}

async fn history(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: HistoryParams = if params.is_null() {
        HistoryParams { limit: None }
    } else {
        serde_json::from_value(params).map_err(ErrorResponse::invalid_params)?
    };
    Ok(ws.portal().history(params.limit.map(|n| n as usize)))
}
