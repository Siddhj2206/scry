//! Shared helpers for driving a live `scry` daemon over its real Unix-socket
//! newline-delimited JSON-RPC 2.0 control protocol.
#![allow(dead_code)] // each test binary uses a different subset of helpers

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use scry::daemon::{DaemonConfig, DaemonHandle};
use serde_json::{Value, json};
use tempfile::TempDir;

pub const RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// A live daemon bound to a fresh temporary runtime/state directory.
pub struct TestDaemon {
    pub handle: DaemonHandle,
    _dir: TempDir,
    pub socket_path: PathBuf,
    pub state_dir: PathBuf,
}

/// Spawns a daemon on an isolated control socket + state directory, with a
/// fast reaper interval so idle-TTL tests stay quick.
pub fn spawn_daemon() -> TestDaemon {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let socket_path = dir.path().join("run/scry.sock");
    let state_dir = dir.path().join("state");
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        state_dir: state_dir.clone(),
        default_idle_ttl: Duration::from_secs(300),
        reaper_interval: Duration::from_millis(25),
        change_log_capacity: scry::daemon::DEFAULT_CHANGE_LOG_CAPACITY,
    };
    let handle = DaemonHandle::spawn(config).expect("daemon failed to start");
    TestDaemon {
        handle,
        _dir: dir,
        socket_path,
        state_dir,
    }
}

/// A blocking JSON-RPC 2.0 client bound to one daemon control-socket
/// connection.
///
/// Tests stay synchronous: each call writes one request line and reads one
/// response line, exactly like the thin `scry` CLI will.
pub struct RpcClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

/// A structured JSON-RPC error returned by the daemon.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcClient {
    /// Connects to the daemon's control socket.
    pub fn connect(socket_path: &Path) -> Self {
        let stream =
            UnixStream::connect(socket_path).expect("failed to connect to daemon control socket");
        stream
            .set_read_timeout(Some(RPC_TIMEOUT))
            .expect("set_read_timeout failed");
        let reader = BufReader::new(stream.try_clone().expect("failed to clone control socket"));
        Self {
            writer: stream,
            reader,
            next_id: 0,
        }
    }

    /// Issues a request with an explicit id value and returns the full
    /// decoded response frame.
    pub fn request_frame(&mut self, id: Value, method: &str, params: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.writer
            .write_all(request.to_string().as_bytes())
            .expect("failed to write request");
        self.writer
            .write_all(b"\n")
            .expect("failed to write newline");
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .expect("failed to read response");
        serde_json::from_str(&line).expect("response was not valid JSON")
    }

    /// Issues a request and returns the full response frame, whether it
    /// carries `result` or `error` — for long-running verbs driven from a
    /// dedicated thread where either outcome is asserted by the caller.
    pub fn call_frame(&mut self, method: &str, params: Value) -> Value {
        let id = self.alloc_id();
        self.request_frame(id, method, params)
    }

    /// Half-closes the connection: shuts the socket's write side so no
    /// more requests can be sent, while the read side stays open for
    /// in-flight responses.
    pub fn shutdown_write(&mut self) {
        self.writer
            .shutdown(std::net::Shutdown::Write)
            .expect("failed to shutdown write half");
    }

    /// Reads one response frame, without sending a request first — used
    /// after [`Self::send_request`] or a [`Self::shutdown_write`].
    pub fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .expect("failed to read response");
        serde_json::from_str(&line).expect("response was not valid JSON")
    }

    /// Writes a request without reading any response, returning the id used.
    ///
    /// For blocking verbs (waits): the caller observes the in-flight request
    /// through other means (a second connection, `active_waits`) and can
    /// drop the connection to test disconnect cancellation.
    pub fn send_request(&mut self, method: &str, params: Value) -> Value {
        let id = self.alloc_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.writer
            .write_all(request.to_string().as_bytes())
            .expect("failed to write request");
        self.writer
            .write_all(b"\n")
            .expect("failed to write newline");
        id
    }

    /// Issues a request and returns the `result` object on success.
    ///
    /// Panics on a JSON-RPC error response.
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.alloc_id();
        let frame = self.request_frame(id.clone(), method, params);
        assert_eq!(frame["jsonrpc"], "2.0", "bad jsonrpc version: {frame}");
        assert_eq!(
            frame["id"], id,
            "response id does not echo request id: {frame}"
        );
        if let Some(error) = frame.get("error") {
            panic!("unexpected rpc error from {method}: {error}");
        }
        let result = frame["result"].clone();
        assert_eq!(
            result["command_id"], id,
            "result does not echo command_id: {frame}"
        );
        result
    }

    /// Issues a request expecting a structured JSON-RPC error and returns it.
    pub fn call_err(&mut self, method: &str, params: Value) -> RpcError {
        let id = self.alloc_id();
        let frame = self.request_frame(id.clone(), method, params);
        let error = frame
            .get("error")
            .unwrap_or_else(|| panic!("expected rpc error from {method}, got result: {frame}"));
        RpcError {
            code: error["code"].as_i64().expect("error.code missing"),
            message: error["message"].as_str().unwrap_or_default().to_string(),
            data: error.get("data").cloned(),
        }
    }

    /// Writes a raw frame (for protocol-error tests) and reads one response
    /// line back, unparsed.
    pub fn raw_exchange(&mut self, payload: &str) -> String {
        self.writer
            .write_all(payload.as_bytes())
            .expect("failed to write raw frame");
        self.writer
            .write_all(b"\n")
            .expect("failed to write newline");
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .expect("failed to read response");
        line
    }

    fn alloc_id(&mut self) -> Value {
        self.next_id += 1;
        Value::from(self.next_id)
    }
}
