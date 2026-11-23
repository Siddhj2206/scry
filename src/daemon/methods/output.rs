//! Virtual output verbs: `output_add`, `output_list`, `output_remove`.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    OutputSpec,
    daemon::{
        protocol::{ErrorResponse, code},
        router::{MethodDef, Scope},
    },
};

/// `output_add` — add or reconfigure a virtual output (mutating).
pub const ADD: MethodDef = MethodDef {
    name: "output_add",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            add(ws, call.params).await
        })
    },
};

/// `output_list` — snapshot the workspace's virtual outputs.
pub const LIST: MethodDef = MethodDef {
    name: "output_list",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            let reply = ws.call(|handle| handle.outputs()).await?;
            Ok(json!({
                "outputs": reply.value,
                "seq": reply.seq,
            }))
        })
    },
};

/// `output_remove` — withdraw a virtual output by `wl_output` name
/// (mutating).
pub const REMOVE: MethodDef = MethodDef {
    name: "output_remove",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            remove(ws, call.params).await
        })
    },
};

#[derive(Deserialize)]
struct AddParams {
    name: String,
    /// Physical mode size, either as `size: [w, h]` or `width`/`height`.
    width: Option<i32>,
    height: Option<i32>,
    size: Option<(i32, i32)>,
    /// Refresh in millihertz (default 60_000 = 60Hz).
    #[serde(default, alias = "refresh", alias = "refreshMillihertz")]
    refresh_millihertz: Option<i32>,
    /// Advertised scale factor (default 1.0).
    scale: Option<f64>,
    /// Logical position, either as `position: [x, y]` or `x`/`y`
    /// (default 0,0).
    x: Option<i32>,
    y: Option<i32>,
    position: Option<(i32, i32)>,
}

#[derive(Deserialize)]
struct RemoveParams {
    name: String,
}

async fn add(
    ws: Arc<crate::daemon::registry::Workspace>,
    params: Value,
) -> Result<Value, ErrorResponse> {
    let params: AddParams =
        serde_json::from_value(params).map_err(ErrorResponse::invalid_params)?;
    let size = params
        .size
        .or_else(|| params.width.zip(params.height))
        .ok_or_else(|| {
            ErrorResponse::invalid_params(
                "output_add requires a mode size: \"size\": [w, h] or \"width\"+\"height\"",
            )
        })?;
    let spec = OutputSpec {
        name: params.name,
        size,
        refresh_millihertz: params.refresh_millihertz.unwrap_or(60_000),
        scale: params.scale.unwrap_or(1.0),
        position: params
            .position
            .unwrap_or((params.x.unwrap_or(0), params.y.unwrap_or(0))),
    };
    let name = spec.name.clone();
    let reply = ws
        .call(move |handle| {
            handle.update_output(spec)?;
            handle.outputs()
        })
        .await?;
    let output = reply
        .value
        .iter()
        .find(|o| o.name == name)
        .ok_or_else(|| ErrorResponse::new(code::COMPOSITOR, "output disappeared after add"))?;
    Ok(json!({
        "output": output,
        "seq": reply.seq,
    }))
}

async fn remove(
    ws: Arc<crate::daemon::registry::Workspace>,
    params: Value,
) -> Result<Value, ErrorResponse> {
    let params: RemoveParams =
        serde_json::from_value(params).map_err(ErrorResponse::invalid_params)?;
    let name = params.name.clone();
    let reply = ws.call(move |handle| handle.remove_output(&name)).await?;
    Ok(json!({
        "removed": params.name,
        "seq": reply.seq,
    }))
}
