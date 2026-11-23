//! `spawn_app`: launch a managed process inside a workspace with the
//! default Wayland + `wl_shm` + AT-SPI environment profile.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::{
    process::SpawnSpec,
    protocol::{ErrorResponse, code},
    registry::Workspace,
    router::{MethodDef, Scope},
};

fn invalid_params(err: impl std::fmt::Display) -> ErrorResponse {
    ErrorResponse::new(code::INVALID_PARAMS, format!("invalid params: {err}"))
}

/// `spawn_app` — spawn a managed app (mutating; `readOnly` rejects it
/// before this handler runs).
pub const SPAWN_APP: MethodDef = MethodDef {
    name: "spawn_app",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            spawn_app(ws, call.params).await
        })
    },
};

#[derive(Deserialize)]
struct SpawnParams {
    /// The executable to run (`PATH`-resolved unless it contains a `/`).
    #[serde(alias = "command", alias = "cmd")]
    program: String,
    /// Argument vector (excluding argv[0]).
    #[serde(default)]
    args: Vec<String>,
    /// Working directory for the child.
    #[serde(default, alias = "working_dir")]
    cwd: Option<PathBuf>,
    /// Environment overrides: `{"KEY": "value"}` sets, `{"KEY": null}`
    /// unsets. Applied on top of the default profile.
    #[serde(default)]
    env: HashMap<String, Option<String>>,
}

async fn spawn_app(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: SpawnParams = serde_json::from_value(params).map_err(invalid_params)?;
    if params.program.is_empty() {
        return Err(ErrorResponse::new(
            code::INVALID_PARAMS,
            "spawn_app requires a non-empty \"program\"",
        ));
    }
    // Command::env panics on these; reject them as invalid params instead.
    for (key, value) in &params.env {
        if key.is_empty() || key.contains(['=', '\0']) {
            return Err(ErrorResponse::new(
                code::INVALID_PARAMS,
                format!("invalid environment variable name {key:?}"),
            ));
        }
        if let Some(value) = value
            && value.contains('\0')
        {
            return Err(ErrorResponse::new(
                code::INVALID_PARAMS,
                format!("environment value for {key:?} contains a NUL byte"),
            ));
        }
    }

    let spec = SpawnSpec {
        program: params.program,
        args: params.args,
        cwd: params.cwd,
        env: params.env.into_iter().collect(),
    };
    let info = ws.spawn_process(&spec)?;
    Ok(json!({
        "pid": info.pid,
        "process": info,
    }))
}
