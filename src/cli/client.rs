//! The CLI's wire half: a synchronous newline-delimited JSON-RPC 2.0
//! client over the daemon's Unix control socket, the platform default
//! paths, and the daemon auto-start helper.
//!
//! The client is deliberately stateless: one blocking request/response
//! per call and nothing remembered between invocations — all state lives
//! in the daemon.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// Longest the CLI waits for an auto-started daemon to accept connections.
const AUTOSTART_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll cadence while an auto-started daemon binds its socket.
const AUTOSTART_POLL: Duration = Duration::from_millis(20);

/// The default control socket: `$XDG_RUNTIME_DIR/scry/control.sock`,
/// falling back to `/tmp/scry-<uid>/control.sock`.
pub fn default_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("scry/control.sock"),
        _ => {
            // SAFETY: getuid() takes no arguments, cannot fail and touches
            // no memory.
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/scry-{uid}/control.sock"))
        }
    }
}

/// The default daemon state directory: `$XDG_STATE_HOME/scry`, falling
/// back to `~/.local/state/scry` (and `/tmp` as a last resort).
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("scry");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/scry");
    }
    PathBuf::from("/tmp/scry-state")
}

/// A structured JSON-RPC error returned by the daemon.
#[derive(Debug)]
pub struct RpcError {
    /// The error code (standard JSON-RPC or scry's `-320xx` range).
    pub code: i64,
    /// The short human-readable description.
    pub message: String,
    /// Optional structured detail.
    pub data: Option<Value>,
}

/// How a single call can fail.
#[derive(Debug)]
pub enum Failure {
    /// The daemon sent back a well-formed JSON-RPC error.
    Rpc(RpcError),
    /// The socket could not be reached or broke mid-call.
    Transport(std::io::Error),
    /// The daemon's response was not a valid JSON-RPC response frame.
    Protocol(String),
}

/// A blocking JSON-RPC 2.0 client on one control-socket connection.
///
/// Used for exactly one verb per CLI invocation: connect, call, exit.
pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    /// Connects to the daemon's control socket.
    pub fn connect(socket: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            writer: stream,
            reader,
            next_id: 0,
        })
    }

    /// Issues one request and returns the daemon's result envelope
    /// (`command_id` + `seq` + the verb's payload).
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut frame = request.to_string().into_bytes();
        frame.push(b'\n');
        self.writer.write_all(&frame).map_err(Failure::Transport)?;
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(Failure::Transport)?;
        if read == 0 {
            return Err(Failure::Transport(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed the connection without replying",
            )));
        }
        let response: Value = serde_json::from_str(line.trim_end())
            .map_err(|err| Failure::Protocol(format!("malformed response frame: {err}")))?;
        if let Some(error) = response.get("error") {
            return Err(Failure::Rpc(RpcError {
                code: error["code"].as_i64().unwrap_or_default(),
                message: error["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string(),
                data: error.get("data").cloned(),
            }));
        }
        match response.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(Failure::Protocol(format!(
                "response carries neither result nor error: {response}"
            ))),
        }
    }
}

/// Auto-starts the daemon: spawns the running binary as `scry daemon`
/// bound to `socket`/`state_dir`, detached so it outlives this CLI call,
/// then waits until the control socket accepts a connection.
///
/// The child's output lands in `<state_dir>/daemon.log` so a failed
/// startup stays diagnosable without the CLI holding pipes open.
pub fn autostart_daemon(socket: &Path, state_dir: &Path) -> Result<(), String> {
    match UnixStream::connect(socket) {
        // Already running: nothing to do.
        Ok(_) => return Ok(()),
        // A socket file that refuses connections is stale — remove it so
        // the daemon can bind.
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
            let _ = std::fs::remove_file(socket);
        }
        Err(_) => {}
    }

    std::fs::create_dir_all(state_dir).map_err(|err| {
        format!(
            "failed to create daemon state dir {}: {err}",
            state_dir.display()
        )
    })?;
    let log_path = state_dir.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| format!("failed to open {}: {err}", log_path.display()))?;
    let exe = std::env::current_exe().map_err(|err| format!("cannot locate scry binary: {err}"))?;
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--socket")
        .arg(socket)
        .arg("--state-dir")
        .arg(state_dir)
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .map_err(|err| format!("failed to clone log fd: {err}"))?,
        )
        .stderr(log)
        .spawn()
        .map_err(|err| format!("failed to spawn `scry daemon`: {err}"))?;

    let deadline = Instant::now() + AUTOSTART_TIMEOUT;
    loop {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "auto-started daemon exited with {status} — see {}",
                    log_path.display()
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("failed to monitor auto-started daemon: {err}")),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "daemon did not start listening on {} within {}s — see {}",
                socket.display(),
                AUTOSTART_TIMEOUT.as_secs(),
                log_path.display()
            ));
        }
        std::thread::sleep(AUTOSTART_POLL);
    }
}
