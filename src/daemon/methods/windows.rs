//! Window observation, placement, capture and damage verbs:
//! `list_windows`, `place_window`, `capture_window`, `capture_workspace`,
//! `get_damage`.
//!
//! `list_windows` reports every mapped toplevel (plus popups) in a
//! workspace; `place_window` applies deterministic `(x, y[, w, h])`
//! placement through the compositor's configure/ack lifecycle; the capture
//! verbs return base64-encoded PNG/WebP frames with the `seq` at capture
//! time; `get_damage` replays the retained normalized-damage log filtered
//! by window and/or `after_seq`.

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Rect, WindowId,
    daemon::{
        protocol::{ErrorResponse, code},
        registry::Workspace,
        router::{MethodDef, Scope},
    },
    handle::Reply,
};

fn invalid_params(err: impl std::fmt::Display) -> ErrorResponse {
    ErrorResponse::new(code::INVALID_PARAMS, format!("invalid params: {err}"))
}

fn missing_params(what: &str) -> ErrorResponse {
    ErrorResponse::new(
        code::INVALID_PARAMS,
        format!("missing required parameter {what}"),
    )
}

/// The capture encodings accepted by `capture_window`/`capture_workspace`.
#[derive(Debug, Clone, Copy)]
enum CaptureFormat {
    Png,
    WebP,
}

impl CaptureFormat {
    fn parse(params: &Value) -> Result<Self, ErrorResponse> {
        match params.get("format").and_then(Value::as_str) {
            None | Some("png") => Ok(Self::Png),
            Some("webp") => Ok(Self::WebP),
            Some(other) => Err(ErrorResponse::with_data(
                code::INVALID_PARAMS,
                format!("unsupported capture format {other:?}: use \"png\" or \"webp\""),
                json!({"format": other}),
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::WebP => "webp",
        }
    }

    /// Encodes `frame` in this format into `(width, height, bytes)`.
    fn encode(self, frame: crate::Frame) -> crate::Result<(u32, u32, Vec<u8>)> {
        let bytes = match self {
            Self::Png => frame.encode_png()?,
            Self::WebP => frame.encode_webp()?,
        };
        Ok((frame.width, frame.height, bytes))
    }
}

/// `list_windows` — structured metadata for every mapped toplevel and popup
/// in the workspace.
pub const LIST_WINDOWS: MethodDef = MethodDef {
    name: "list_windows",
    scope: Scope::Workspace { mutating: false },
    run: |_ctx, _params, ws| {
        Box::pin(async move {
            let ws = ws.expect("workspace scope resolves a workspace");
            let reply = ws
                .call(|handle| {
                    let windows = handle.windows()?;
                    let popups = handle.popups()?;
                    Ok(Reply {
                        seq: windows.seq.max(popups.seq),
                        value: json!({
                            "windows": windows.value,
                            "popups": popups.value,
                        }),
                    })
                })
                .await?;
            let mut payload = reply.value;
            payload["seq"] = json!(reply.seq);
            Ok(payload)
        })
    },
};

/// `place_window` — deterministic `(x, y)` placement plus an optional
/// `(width, height)` resize configure (mutating, rejected on `readOnly`).
pub const PLACE_WINDOW: MethodDef = MethodDef {
    name: "place_window",
    scope: Scope::Workspace { mutating: true },
    run: |_ctx, params, ws| {
        Box::pin(async move {
            let ws = ws.expect("workspace scope resolves a workspace");
            place(ws, params).await
        })
    },
};

/// `capture_window` — occlusion-free capture of one window as base64
/// PNG/WebP.
pub const CAPTURE_WINDOW: MethodDef = MethodDef {
    name: "capture_window",
    scope: Scope::Workspace { mutating: false },
    run: |_ctx, params, ws| {
        Box::pin(async move {
            let ws = ws.expect("workspace scope resolves a workspace");
            capture_window(ws, params).await
        })
    },
};

/// `capture_workspace` — a workspace output composited in z-order as
/// base64 PNG/WebP (the primary output unless `output` names another).
pub const CAPTURE_WORKSPACE: MethodDef = MethodDef {
    name: "capture_workspace",
    scope: Scope::Workspace { mutating: false },
    run: |_ctx, params, ws| {
        Box::pin(async move {
            let ws = ws.expect("workspace scope resolves a workspace");
            capture_workspace(ws, params).await
        })
    },
};

/// `get_damage` — normalized damage rectangles, optionally filtered by
/// `window` and/or `after_seq`.
pub const GET_DAMAGE: MethodDef = MethodDef {
    name: "get_damage",
    scope: Scope::Workspace { mutating: false },
    run: |_ctx, params, ws| {
        Box::pin(async move {
            let ws = ws.expect("workspace scope resolves a workspace");
            get_damage(ws, params).await
        })
    },
};

/// Resolves the window id from `params.window` / `window_id` / `id`.
fn window_param(params: &Value) -> Result<WindowId, ErrorResponse> {
    params
        .get("window")
        .or_else(|| params.get("window_id"))
        .or_else(|| params.get("id"))
        .and_then(Value::as_u64)
        .map(WindowId)
        .ok_or_else(|| missing_params("\"window\" (a window id from list_windows)"))
}

#[derive(Deserialize)]
struct PlaceParams {
    #[serde(default, alias = "window_id", alias = "id")]
    window: Option<u64>,
    /// Full placement rectangle, alternative to flat `x`/`y`/`width`/`height`.
    rect: Option<Rect>,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
}

async fn place(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: PlaceParams = serde_json::from_value(params).map_err(invalid_params)?;
    let id = params
        .window
        .map(WindowId)
        .ok_or_else(|| missing_params("\"window\" (a window id from list_windows)"))?;

    let (position, size) = if let Some(rect) = params.rect {
        if rect.width <= 0 || rect.height <= 0 {
            return Err(ErrorResponse::new(
                code::INVALID_PARAMS,
                "\"rect\" requires positive \"width\" and \"height\"",
            ));
        }
        ((rect.x, rect.y), Some((rect.width, rect.height)))
    } else {
        let x = params.x.ok_or_else(|| missing_params("\"x\""))?;
        let y = params.y.ok_or_else(|| missing_params("\"y\""))?;
        let size = match (params.width, params.height) {
            (None, None) => None,
            (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
            _ => {
                return Err(ErrorResponse::new(
                    code::INVALID_PARAMS,
                    "\"width\" and \"height\" must be given together and be positive",
                ));
            }
        };
        ((x, y), size)
    };

    let reply = ws
        .call(move |handle| {
            handle.place_window(id, position, size)?;
            handle.windows()
        })
        .await?;
    let window =
        reply.value.iter().find(|w| w.id == id).ok_or_else(|| {
            ErrorResponse::new(code::COMPOSITOR, "window disappeared after place")
        })?;
    Ok(json!({
        "window": window,
        "seq": reply.seq,
    }))
}

async fn capture_window(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let id = window_param(&params)?;
    let format = CaptureFormat::parse(&params)?;
    let reply = ws
        .call(move |handle| {
            handle
                .capture_window(id)?
                .try_map(|frame| format.encode(frame))
        })
        .await?;
    let (width, height, bytes) = reply.value;
    Ok(json!({
        "window_id": id.0,
        "format": format.name(),
        "width": width,
        "height": height,
        "data": BASE64.encode(bytes),
        "seq": reply.seq,
    }))
}

async fn capture_workspace(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let format = CaptureFormat::parse(&params)?;
    let requested = params
        .get("output")
        .and_then(Value::as_str)
        .map(str::to_string);
    let reply = ws
        .call(move |handle| {
            let name = match requested {
                Some(name) => name,
                // Default to the primary (first-registered) virtual output.
                None => handle
                    .outputs()?
                    .value
                    .first()
                    .map(|output| output.name.clone())
                    .ok_or_else(|| crate::Error::Capture("workspace has no outputs".into()))?,
            };
            handle
                .capture_output(&name)?
                .try_map(|frame| format.encode(frame).map(|(w, h, b)| (name, w, h, b)))
        })
        .await?;
    let (output, width, height, bytes) = reply.value;
    Ok(json!({
        "output": output,
        "format": format.name(),
        "width": width,
        "height": height,
        "data": BASE64.encode(bytes),
        "seq": reply.seq,
    }))
}

async fn get_damage(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let after_seq = params
        .get("after_seq")
        .or_else(|| params.get("since"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let window = params
        .get("window")
        .or_else(|| params.get("window_id"))
        .and_then(Value::as_u64)
        .map(WindowId);

    let reply = ws
        .call(move |handle| {
            let damage = handle.damage_since(after_seq)?;
            let windows = handle.windows()?;
            Ok(Reply {
                seq: damage.seq.max(windows.seq),
                value: (damage.value, windows.value),
            })
        })
        .await?;
    let (mut entries, windows) = reply.value;

    if let Some(id) = window {
        // A window filter must name a real window — currently mapped, or
        // one that left damage behind in the retained log (it may have
        // closed since).
        let known =
            windows.iter().any(|w| w.id == id) || entries.iter().any(|e| e.window_id == Some(id));
        if !known {
            return Err(ErrorResponse::with_data(
                code::UNKNOWN_WINDOW,
                format!("unknown window id: {id:?}"),
                json!({"window_id": id.0}),
            ));
        }
        entries.retain(|entry| entry.window_id == Some(id));
    }

    Ok(json!({
        "damage": entries,
        "seq": reply.seq,
    }))
}
