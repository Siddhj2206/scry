//! Targeted input and clipboard verbs: `click`, `drag`, `type_text`,
//! `key_combo`, `clipboard_get`, `clipboard_set`.
//!
//! Pointer verbs accept either workspace/output coordinates (`x`/`y`) or
//! window-local coordinates (`window` + `x`/`y`, relative to the window's
//! logical top-left corner). `drag` takes `from`/`to` points in either
//! space. Keyboard verbs optionally focus a `window` before injecting.
//! Clipboard payloads travel base64-encoded (`data_base64`); `data`
//! carries UTF-8 text as a convenience for `clipboard_set`.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Error, Reply, SelectionKind, WindowId, WindowInfo,
    daemon::{
        protocol::{ErrorResponse, code},
        registry::Workspace,
        router::{MethodDef, Scope},
    },
    input::{PointerButton, PointerTarget},
};

fn invalid_params(err: impl std::fmt::Display) -> ErrorResponse {
    ErrorResponse::new(code::INVALID_PARAMS, format!("invalid params: {err}"))
}

/// `click` — move the pointer and press/release a button (mutating).
pub const CLICK: MethodDef = MethodDef {
    name: "click",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            click(ws, call.params).await
        })
    },
};

/// `drag` — press, sweep through interpolated motions, release (mutating).
pub const DRAG: MethodDef = MethodDef {
    name: "drag",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            drag(ws, call.params).await
        })
    },
};

/// `type_text` — inject UTF-8 text, optionally focusing a window first
/// (mutating).
pub const TYPE_TEXT: MethodDef = MethodDef {
    name: "type_text",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            type_text(ws, call.params).await
        })
    },
};

/// `key_combo` — tap a key while holding modifiers, optionally focusing a
/// window first (mutating).
pub const KEY_COMBO: MethodDef = MethodDef {
    name: "key_combo",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            key_combo(ws, call.params).await
        })
    },
};

/// `clipboard_get` — read a selection's payload (observation verb).
pub const CLIPBOARD_GET: MethodDef = MethodDef {
    name: "clipboard_get",
    scope: Scope::Workspace { mutating: false },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            clipboard_get(ws, call.params).await
        })
    },
};

/// `clipboard_set` — install a compositor-provided selection (mutating).
pub const CLIPBOARD_SET: MethodDef = MethodDef {
    name: "clipboard_set",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .expect("workspace scope resolves a workspace");
            clipboard_set(ws, call.params).await
        })
    },
};

// -- parameter plumbing -------------------------------------------------------

/// A `(x, y)` point in output coordinates or relative to a window's
/// logical top-left corner (`window` = its `WindowId`).
#[derive(Debug, Deserialize)]
struct TargetPoint {
    x: f64,
    y: f64,
    #[serde(default)]
    window: Option<u64>,
}

impl TargetPoint {
    /// The compositor target this point resolves to.
    fn target(&self) -> PointerTarget {
        match self.window {
            Some(id) => PointerTarget::Window(WindowId(id)),
            None => PointerTarget::Output,
        }
    }

    /// Global logical coordinates of this point: window-local points are
    /// shifted by the window's geometry origin. Used for drag-path
    /// interpolation between two possibly different coordinate spaces.
    fn global(&self, windows: &[WindowInfo]) -> crate::Result<(f64, f64)> {
        match self.window {
            None => Ok((self.x, self.y)),
            Some(id) => {
                let geometry = windows
                    .iter()
                    .find(|w| w.id.0 == id)
                    .map(|w| w.geometry)
                    .ok_or(Error::UnknownWindow(WindowId(id)))?;
                Ok((geometry.x as f64 + self.x, geometry.y as f64 + self.y))
            }
        }
    }
}

/// Resolves a `button` parameter: a friendly name (`"left"`, `"right"`,
/// `"middle"`, `"side"`, `"extra"`, `"forward"`, `"back"`, `"task"`) or a
/// raw evdev button code (0x100–0x2FF). Defaults to `left`.
fn button_param(value: Option<&Value>) -> Result<PointerButton, ErrorResponse> {
    match value {
        None | Some(Value::Null) => Ok(PointerButton::Left),
        Some(Value::String(name)) => named_button(name).ok_or_else(|| {
            invalid_params(format!(
                "unknown button {name:?}: expected a button name or evdev code"
            ))
        }),
        Some(value @ Value::Number(_)) => {
            let code = value
                .as_u64()
                .and_then(|code| u32::try_from(code).ok())
                .ok_or_else(|| {
                    invalid_params(format!(
                        "button must be a non-negative integer, got {value}"
                    ))
                })?;
            let button = PointerButton::Other(code);
            // `code()` validates the evdev button range.
            button.code().map_err(ErrorResponse::from)?;
            Ok(button)
        }
        Some(other) => Err(invalid_params(format!(
            "button must be a name or evdev code, got {other}"
        ))),
    }
}

fn named_button(name: &str) -> Option<PointerButton> {
    Some(match name.to_ascii_lowercase().as_str() {
        "left" => PointerButton::Left,
        "right" => PointerButton::Right,
        "middle" => PointerButton::Middle,
        "side" => PointerButton::Side,
        "extra" => PointerButton::Extra,
        "forward" => PointerButton::Forward,
        "back" => PointerButton::Back,
        "task" => PointerButton::Task,
        _ => return None,
    })
}

/// A display label for a resolved button (names for known buttons, the raw
/// code for [`PointerButton::Other`]).
fn button_label(button: PointerButton) -> Value {
    match button {
        PointerButton::Left => json!("left"),
        PointerButton::Right => json!("right"),
        PointerButton::Middle => json!("middle"),
        PointerButton::Side => json!("side"),
        PointerButton::Extra => json!("extra"),
        PointerButton::Forward => json!("forward"),
        PointerButton::Back => json!("back"),
        PointerButton::Task => json!("task"),
        PointerButton::Other(code) => json!(code),
    }
}

/// Resolves `selection`/`kind` to a [`SelectionKind`] (default `clipboard`).
fn selection_kind(value: Option<&str>) -> Result<SelectionKind, ErrorResponse> {
    match value {
        None | Some("clipboard") => Ok(SelectionKind::Clipboard),
        Some("primary") => Ok(SelectionKind::Primary),
        Some(other) => Err(invalid_params(format!(
            "unknown selection {other:?}: expected \"clipboard\" or \"primary\""
        ))),
    }
}

// -- verb handlers --------------------------------------------------------------

#[derive(Deserialize)]
struct ClickParams {
    x: f64,
    y: f64,
    #[serde(default)]
    window: Option<u64>,
    #[serde(default)]
    button: Option<Value>,
    #[serde(default)]
    count: Option<u32>,
}

async fn click(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: ClickParams = serde_json::from_value(params).map_err(invalid_params)?;
    let button = button_param(params.button.as_ref())?;
    let count = params.count.unwrap_or(1);
    if !(1..=64).contains(&count) {
        return Err(invalid_params(format!(
            "click count must be between 1 and 64, got {count}"
        )));
    }
    let (x, y, window) = (params.x, params.y, params.window);
    let target = TargetPoint { x, y, window }.target();

    let reply = ws
        .call(move |handle| {
            let mut ack = handle.move_pointer(target, x, y)?;
            for _ in 0..count {
                handle.press_pointer_button(button)?;
                ack = handle.release_pointer_button(button)?;
            }
            Ok(ack)
        })
        .await?;
    Ok(json!({
        "clicked": count,
        "button": button_label(button),
        "x": x,
        "y": y,
        "window": window,
        "seq": reply.seq,
    }))
}

#[derive(Deserialize)]
struct DragParams {
    from: TargetPoint,
    to: TargetPoint,
    #[serde(default)]
    button: Option<Value>,
    /// Motion events to emit between press and release (default 8).
    #[serde(default)]
    steps: Option<u32>,
}

async fn drag(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: DragParams = serde_json::from_value(params).map_err(invalid_params)?;
    let button = button_param(params.button.as_ref())?;
    let steps = params.steps.unwrap_or(8).clamp(1, 64);
    let from = params.from;
    let to = params.to;

    let reply = ws
        .call(move |handle| {
            // Resolve both endpoints to global coordinates so intermediate
            // motions can interpolate across coordinate spaces.
            let windows = handle.windows()?.value;
            let from_g = from.global(&windows)?;
            let to_g = to.global(&windows)?;

            handle.move_pointer(from.target(), from.x, from.y)?;
            handle.press_pointer_button(button)?;
            for i in 1..=steps {
                if i == steps {
                    // The final motion targets `to`'s own space: window-local
                    // endpoints stay validated and hit occluded windows.
                    handle.move_pointer(to.target(), to.x, to.y)?;
                } else {
                    let t = f64::from(i) / f64::from(steps);
                    let x = from_g.0 + (to_g.0 - from_g.0) * t;
                    let y = from_g.1 + (to_g.1 - from_g.1) * t;
                    handle.move_pointer(PointerTarget::Output, x, y)?;
                }
            }
            let release = handle.release_pointer_button(button)?;
            Ok(Reply {
                seq: release.seq,
                value: (from_g, to_g),
            })
        })
        .await?;
    let (from_g, to_g) = reply.value;
    Ok(json!({
        "from": {"x": from_g.0, "y": from_g.1},
        "to": {"x": to_g.0, "y": to_g.1},
        "steps": steps,
        "button": button_label(button),
        "seq": reply.seq,
    }))
}

#[derive(Deserialize)]
struct TypeTextParams {
    text: String,
    /// Optional window to focus before injecting.
    #[serde(default)]
    window: Option<u64>,
}

async fn type_text(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: TypeTextParams = serde_json::from_value(params).map_err(invalid_params)?;
    let text = params.text;
    let window = params.window;
    let injected = text.clone();
    let reply = ws
        .call(move |handle| {
            if let Some(id) = window {
                handle.focus_window(WindowId(id))?;
            }
            handle.type_text(&injected)
        })
        .await?;
    Ok(json!({
        "text": text,
        "window": window,
        "seq": reply.seq,
    }))
}

#[derive(Deserialize)]
struct KeyComboParams {
    key: String,
    #[serde(default)]
    modifiers: Option<Vec<String>>,
    /// Optional window to focus before injecting.
    #[serde(default)]
    window: Option<u64>,
}

async fn key_combo(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: KeyComboParams = serde_json::from_value(params).map_err(invalid_params)?;
    let key = params.key;
    let modifiers = params.modifiers.unwrap_or_default();
    let window = params.window;
    let combo_key = key.clone();
    let combo_mods = modifiers.clone();
    let reply = ws
        .call(move |handle| {
            if let Some(id) = window {
                handle.focus_window(WindowId(id))?;
            }
            let mods: Vec<&str> = combo_mods.iter().map(String::as_str).collect();
            handle.key(&combo_key, &mods)
        })
        .await?;
    Ok(json!({
        "key": key,
        "modifiers": modifiers,
        "window": window,
        "seq": reply.seq,
    }))
}

#[derive(Deserialize)]
struct ClipboardGetParams {
    /// `"clipboard"` (default) or `"primary"`.
    #[serde(default, alias = "kind")]
    selection: Option<String>,
    /// The MIME type to negotiate; defaults to the first offered type.
    #[serde(default, alias = "mime_type")]
    mime: Option<String>,
}

async fn clipboard_get(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: ClipboardGetParams = serde_json::from_value(params).map_err(invalid_params)?;
    let kind = selection_kind(params.selection.as_deref())?;
    let requested = params.mime;

    let reply = ws
        .call(move |handle| {
            let offered = handle.selection_mime_types(kind)?.value;
            let mime = match requested {
                Some(mime) => mime,
                None => offered
                    .first()
                    .cloned()
                    .ok_or(Error::EmptySelection(kind))?,
            };
            let read = handle.read_selection(kind, mime.clone())?;
            Ok(Reply {
                seq: read.seq,
                value: (mime, offered, read.value),
            })
        })
        .await?;
    let (mime, offered, data) = reply.value;
    Ok(json!({
        "selection": kind.to_string(),
        "mime": mime,
        "mime_types": offered,
        "data_base64": BASE64.encode(&data),
        "size": data.len(),
        "seq": reply.seq,
    }))
}

#[derive(Deserialize)]
struct ClipboardSetParams {
    /// `"clipboard"` (default) or `"primary"`.
    #[serde(default, alias = "kind")]
    selection: Option<String>,
    /// The MIME type to offer (default `text/plain`).
    #[serde(default, alias = "mime_type")]
    mime: Option<String>,
    /// UTF-8 text payload.
    #[serde(default)]
    data: Option<String>,
    /// Base64-encoded binary payload (mutually exclusive with `data`).
    #[serde(default)]
    data_base64: Option<String>,
}

async fn clipboard_set(ws: Arc<Workspace>, params: Value) -> Result<Value, ErrorResponse> {
    let params: ClipboardSetParams = serde_json::from_value(params).map_err(invalid_params)?;
    let kind = selection_kind(params.selection.as_deref())?;
    let mime = params.mime.unwrap_or_else(|| TEXT_MIME.to_string());
    let data = match (params.data, params.data_base64) {
        (Some(text), None) => text.into_bytes(),
        (None, Some(b64)) => BASE64
            .decode(&b64)
            .map_err(|err| invalid_params(format!("data_base64 is not valid base64: {err}")))?,
        (None, None) => {
            return Err(invalid_params(
                "clipboard_set requires \"data\" or \"data_base64\"",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_params(
                "pass either \"data\" or \"data_base64\", not both",
            ));
        }
    };
    let size = data.len();
    let offered = mime.clone();

    let reply = ws
        .call(move |handle| handle.set_selection(kind, offered, data))
        .await?;
    Ok(json!({
        "selection": kind.to_string(),
        "mime": mime,
        "size": size,
        "seq": reply.seq,
    }))
}

/// The MIME type `clipboard_set` offers when `mime` is omitted.
const TEXT_MIME: &str = "text/plain";
