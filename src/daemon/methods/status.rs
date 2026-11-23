//! `status`: a diagnostic verb narrating what the daemon is running, what
//! is unhealthy, and what to do next.
//!
//! The report aggregates daemon metadata (uptime, socket, state dir),
//! per-workspace compositor liveness, outputs, mapped windows and spawned
//! process health into structured fields plus a human-readable `running`
//! narrative, an `unhealthy` findings list and concrete `next_steps`.

use std::{path::PathBuf, sync::Arc};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Reply,
    daemon::{
        DaemonContext,
        protocol::{ErrorResponse, code},
        registry::Workspace,
        router::{MethodDef, Scope},
    },
};

fn invalid_params(err: impl std::fmt::Display) -> ErrorResponse {
    ErrorResponse::new(code::INVALID_PARAMS, format!("invalid params: {err}"))
}

/// `status` — daemon-wide diagnostics; `{"workspace": id}` scopes the
/// workspace detail to one workspace.
pub const STATUS: MethodDef = MethodDef {
    name: "status",
    scope: Scope::Daemon,
    run: |call| Box::pin(status(call.ctx, call.params)),
};

#[derive(Deserialize)]
struct StatusParams {
    /// Restrict the workspace detail to one workspace id.
    #[serde(default)]
    workspace: Option<String>,
}

async fn status(ctx: Arc<DaemonContext>, params: Value) -> Result<Value, ErrorResponse> {
    let params: StatusParams = if params.is_null() {
        StatusParams { workspace: None }
    } else {
        serde_json::from_value(params).map_err(invalid_params)?
    };
    let targets = match &params.workspace {
        Some(id) => vec![ctx.registry.get(id)?],
        None => ctx.registry.all(),
    };

    let socket_listening = ctx.config.socket_path.exists();
    let mut unhealthy: Vec<String> = Vec::new();
    let mut next_steps: Vec<String> = Vec::new();
    if !socket_listening {
        unhealthy.push(format!(
            "control socket {} is missing from the filesystem",
            ctx.config.socket_path.display()
        ));
        next_steps.push("restart the daemon; clients cannot connect".to_string());
    }

    let mut workspaces = Vec::with_capacity(targets.len());
    let mut running_bits = Vec::with_capacity(targets.len());
    for ws in &targets {
        workspaces
            .push(workspace_entry(ws, &mut unhealthy, &mut next_steps, &mut running_bits).await);
    }

    let detail = if running_bits.is_empty() {
        "no live workspaces".to_string()
    } else {
        running_bits.join("; ")
    };
    let running = format!(
        "scry daemon v{} (pid {}) up {:.1}s on {}; {detail}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        ctx.started_at.elapsed().as_secs_f64(),
        ctx.config.socket_path.display(),
    );

    if next_steps.is_empty() {
        next_steps.push("No unhealthy findings; nothing to do.".to_string());
    }
    let healthy = unhealthy.is_empty();
    Ok(json!({
        "daemon": {
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_ms": ctx.started_at.elapsed().as_millis() as u64,
            "socket_path": ctx.config.socket_path,
            "socket_listening": socket_listening,
            "state_dir": ctx.config.state_dir,
            "workspace_count": workspaces.len(),
        },
        "workspaces": workspaces,
        "healthy": healthy,
        "running": running,
        "unhealthy": unhealthy,
        "next_steps": next_steps,
    }))
}

/// Builds the `status` report entry for one workspace, appending health
/// findings, next steps and its share of the `running` narrative.
async fn workspace_entry(
    ws: &Arc<Workspace>,
    unhealthy: &mut Vec<String>,
    next_steps: &mut Vec<String>,
    running_bits: &mut Vec<String>,
) -> Value {
    let id = ws.id().to_string();
    let mut entry = ws.summary();
    let mut healthy = true;

    // A compositor whose socket vanished from the filesystem can no longer
    // accept clients — the classic stale-workspace symptom.
    let socket = PathBuf::from(entry["socket_path"].as_str().unwrap_or_default());
    let socket_listening = socket.exists();
    entry["socket_listening"] = json!(socket_listening);
    if !socket_listening {
        healthy = false;
        unhealthy.push(format!(
            "workspace {id:?}: Wayland socket {} is missing",
            socket.display()
        ));
        next_steps.push(format!(
            "close workspace {id:?} and recreate it; clients cannot connect to a stale socket"
        ));
    }
    if !ws.is_live() {
        healthy = false;
        unhealthy.push(format!("workspace {id:?}: compositor is no longer running"));
        next_steps.push(format!(
            "close workspace {id:?} and recreate it; its compositor has exited"
        ));
    }

    // One compositor round-trip both proves responsiveness and yields the
    // output/window snapshot.
    let mut window_count = 0;
    match ws
        .call(|handle| {
            let outputs = handle.outputs()?;
            let windows = handle.windows()?;
            Ok(Reply {
                seq: windows.seq.max(outputs.seq),
                value: (outputs.value, windows.value),
            })
        })
        .await
    {
        Ok(reply) => {
            let (outputs, windows) = reply.value;
            window_count = windows.len();
            entry["compositor_responsive"] = json!(true);
            entry["outputs"] = json!(outputs);
            entry["windows"] = json!(
                windows
                    .iter()
                    .map(|w| json!({
                        "id": w.id,
                        "app_id": w.app_id,
                        "title": w.title,
                        "geometry": w.geometry,
                        "activated": w.activated,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        Err(err) => {
            healthy = false;
            entry["compositor_responsive"] = json!(false);
            unhealthy.push(format!(
                "workspace {id:?}: compositor is not responding ({})",
                err.message
            ));
            next_steps.push(format!(
                "close workspace {id:?} and recreate it; its compositor is unresponsive"
            ));
        }
    }
    entry["window_count"] = json!(window_count);

    // Spawned-app health: crashed or killed children are actionable
    // findings; clean exits and running apps are normal.
    let mut running_apps = 0usize;
    for proc in ws.process_snapshot() {
        match proc.state {
            "running" => running_apps += 1,
            "exited" => {
                if let Some(code) = proc.exit_code
                    && code != 0
                {
                    healthy = false;
                    unhealthy.push(format!(
                        "workspace {id:?}: spawned app {:?} (pid {}) exited with code {code}",
                        proc.program, proc.pid
                    ));
                    next_steps.push(format!(
                        "inspect {:?} (pid {}) for why it exited, then respawn it with \
                         spawn_app if still needed",
                        proc.program, proc.pid
                    ));
                }
            }
            "signaled" => {
                healthy = false;
                unhealthy.push(format!(
                    "workspace {id:?}: spawned app {:?} (pid {}) was killed by signal {}",
                    proc.program,
                    proc.pid,
                    proc.signal.unwrap_or(0)
                ));
                next_steps.push(format!(
                    "inspect {:?} (pid {}) for why it was killed, then respawn it with \
                     spawn_app if still needed",
                    proc.program, proc.pid
                ));
            }
            _ => {}
        }
    }

    entry["healthy"] = json!(healthy);
    running_bits.push(format!(
        "{id} ({window_count} window{}, {running_apps} app{} running)",
        if window_count == 1 { "" } else { "s" },
        if running_apps == 1 { "" } else { "s" },
    ));
    entry
}
