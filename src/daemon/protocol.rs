//! The daemon's wire protocol: newline-delimited JSON-RPC 2.0 frames.
//!
//! Each request is one JSON object terminated by `\n`. Successful responses
//! echo the request `id` and carry a result envelope that always includes
//! `command_id` (the request `id`, verbatim) and `seq` (the workspace's
//! compositor sequence at which the command applied, or `null` for
//! daemon-scoped calls).

use serde::Serialize;
use serde_json::{Map, Value, json};

/// The JSON-RPC version string every frame must carry.
pub const JSONRPC: &str = "2.0";

/// Maximum accepted request size in bytes (one line).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Structured error codes used on the control socket.
///
/// Standard JSON-RPC codes live at `-327xx`/`-326xx`; scry's
/// application-specific codes live in the `-32000..=-32099` implementation
/// range reserved by the spec.
pub mod code {
    /// The frame was not valid JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON was valid but not a well-formed JSON-RPC request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The requested verb does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// A parameter was missing, malformed, or failed validation.
    pub const INVALID_PARAMS: i64 = -32602;
    /// An unexpected internal failure.
    pub const INTERNAL: i64 = -32603;
    /// The named workspace does not exist (or was already closed).
    pub const UNKNOWN_WORKSPACE: i64 = -32001;
    /// `workspace_create` repeated an existing id with different
    /// creation-time options.
    pub const WORKSPACE_CONFLICT: i64 = -32002;
    /// A mutating verb targeted a workspace created `readOnly`.
    pub const READ_ONLY: i64 = -32003;
    /// The named window does not exist in the workspace.
    pub const UNKNOWN_WINDOW: i64 = -32004;
    /// The named output does not exist in the workspace.
    pub const UNKNOWN_OUTPUT: i64 = -32005;
    /// A bounded operation exceeded its deadline.
    pub const TIMEOUT: i64 = -32006;
    /// An in-flight operation was cancelled (disconnect/`client_pid`).
    pub const CANCELLED: i64 = -32007;
    /// The workspace's compositor failed to service the command.
    pub const COMPOSITOR: i64 = -32008;
    /// Spawning or managing a workspace process failed.
    pub const PROCESS: i64 = -32009;
    /// A `get_tree` ref no longer resolves: the node is gone, its role or
    /// exact name changed, or the snapshot it came from was evicted. The
    /// caller must take a fresh `get_tree` snapshot.
    pub const STALE_REF: i64 = -32010;
    /// A `tree_diff` baseline (`from_snapshot`) is unknown to the
    /// workspace or has been evicted from its bounded snapshot history —
    /// or no baseline exists yet. The caller must capture a fresh
    /// `get_tree` snapshot to diff against.
    pub const SNAPSHOT_EVICTED: i64 = -32011;
}

/// A decoded JSON-RPC request.
#[derive(Debug)]
pub struct Request {
    /// The request `id`, verbatim. `None` marks a notification: the request
    /// is executed but no response is written.
    pub id: Option<Value>,
    /// The verb name, e.g. `workspace_create`.
    pub method: String,
    /// The raw `params` value (`null` when omitted).
    pub params: Value,
}

impl Request {
    /// Parses one frame of newline-delimited JSON into a request.
    ///
    /// On failure, the returned [`ParseFailure`] still carries the request
    /// `id` when it could be recovered, so the error response can echo it.
    pub fn parse(frame: &str) -> Result<Self, ParseFailure> {
        let value: Value = serde_json::from_str(frame).map_err(|err| ParseFailure {
            error: ErrorResponse::new(code::PARSE_ERROR, format!("invalid JSON: {err}")),
            id: None,
        })?;
        let id = value.get("id").cloned();
        let fail = |error: ErrorResponse| ParseFailure {
            error,
            id: id.clone(),
        };
        let Some(object) = value.as_object() else {
            return Err(fail(invalid_request()));
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC) {
            return Err(fail(invalid_request()));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Err(fail(invalid_request()));
        };
        Ok(Self {
            id,
            method: method.to_string(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        })
    }

    /// Whether this request carries an `id` and therefore gets a response.
    pub fn expects_response(&self) -> bool {
        self.id.is_some()
    }
}

/// A frame that failed to parse, plus the request `id` if one was present.
#[derive(Debug)]
pub struct ParseFailure {
    /// The structured error to report.
    pub error: ErrorResponse,
    /// The request `id` recovered from the frame, if any.
    pub id: Option<Value>,
}

fn invalid_request() -> ErrorResponse {
    ErrorResponse::new(code::INVALID_REQUEST, "invalid JSON-RPC request")
}

/// A structured JSON-RPC error.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    /// The error code (standard or application-specific).
    pub code: i64,
    /// A short human-readable description.
    pub message: String,
    /// Optional structured details (workspace id, offending field, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorResponse {
    /// An error with no extra data.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// An error carrying structured detail.
    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    /// An [`INVALID_PARAMS`](code::INVALID_PARAMS) error: `err` is a decode
    /// failure or a caller-composed message, reported verbatim after an
    /// "invalid params:" prefix.
    pub fn invalid_params(err: impl std::fmt::Display) -> Self {
        Self::new(code::INVALID_PARAMS, format!("invalid params: {err}"))
    }

    /// An [`INVALID_PARAMS`](code::INVALID_PARAMS) error carrying
    /// structured `data` alongside the message.
    pub fn invalid_params_data(message: impl Into<String>, data: Value) -> Self {
        Self::with_data(code::INVALID_PARAMS, message, data)
    }
}

impl From<crate::Error> for ErrorResponse {
    /// Maps compositor errors onto the structured code space.
    fn from(err: crate::Error) -> Self {
        use crate::Error as E;
        match err {
            E::UnknownWindow(ref id) => Self::with_data(
                code::UNKNOWN_WINDOW,
                err.to_string(),
                json!({"window_id": id.0}),
            ),
            E::UnknownOutput(ref name) => Self::with_data(
                code::UNKNOWN_OUTPUT,
                err.to_string(),
                json!({"output": name}),
            ),
            E::Timeout => Self::new(code::TIMEOUT, err.to_string()),
            E::InvalidOutputSpec(_)
            | E::UnknownKey(_)
            | E::UnknownModifier(_)
            | E::InvalidButton(_)
            | E::InvalidCoordinates(..)
            | E::OutsideWindow(_)
            | E::NoFocusedSurface
            | E::UnmappableCharacter(_)
            | E::EmptySelection(_)
            | E::MimeTypeNotOffered { .. } => Self::invalid_params(err),
            other => Self::new(code::COMPOSITOR, other.to_string()),
        }
    }
}

/// Serializes a successful response frame (one line, no trailing newline).
pub fn result_frame(id: &Value, result: Value) -> String {
    json!({
        "jsonrpc": JSONRPC,
        "id": id,
        "result": result,
    })
    .to_string()
}

/// Serializes an error response frame (one line, no trailing newline).
pub fn error_frame(id: Option<&Value>, error: &ErrorResponse) -> String {
    json!({
        "jsonrpc": JSONRPC,
        "id": id.cloned().unwrap_or(Value::Null),
        "error": error,
    })
    .to_string()
}

/// Builds the standard result envelope: `command_id` echoes the request id
/// and `seq` reports the workspace sequence the command applied at.
///
/// `payload` fields are merged into the envelope top level. If the payload
/// already carries a `seq` (set precisely by the handler from a command
/// reply) it wins; otherwise `fallback_seq` fills it in.
pub fn envelope(request_id: Value, payload: Value, fallback_seq: Option<u64>) -> Value {
    let mut map = match payload {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    map.insert("command_id".to_string(), request_id);
    map.entry("seq".to_string())
        .or_insert_with(|| fallback_seq.map(Value::from).unwrap_or(Value::Null));
    Value::Object(map)
}
