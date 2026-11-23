//! The `scry` control daemon: a persistent control plane that owns all
//! workspace, compositor and process state behind a Unix-socket,
//! newline-delimited JSON-RPC 2.0 API.
//!
//! Thin clients (the `scry` CLI, the future MCP server, ad-hoc scripts)
//! connect, issue verbs and disconnect without losing running state; the
//! daemon owns every workspace's [`crate::Compositor`] until it is closed
//! explicitly, reaped for idleness, or the daemon shuts down.
//!
//! ```text
//!   client (CLI/script/agent)            scry daemon (tokio)
//!  ┌─────────────────────┐   JSON-RPC   ┌───────────────────────────────┐
//!  │ one socket line per │ ───────────▶ │ accept loop → per-request     │
//!  │ request/response    │  over unix   │ tasks → router → workspace    │
//!  └─────────────────────┘              │ registry → CompositorHandle   │
//!                                       │          ↓                  │
//!                                       │  Workspace { Compositor,    │
//!                                       │  event pump, metadata,      │
//!                                       │  broadcast events }         │
//!                                       └───────────────────────────────┘
//! ```

pub mod a11y;
pub mod bus;
mod changelog;
pub mod methods;
pub mod portal;
pub mod process;
pub mod protocol;
pub mod registry;
pub mod router;

pub use changelog::DEFAULT_CAPACITY as DEFAULT_CHANGE_LOG_CAPACITY;

use std::{
    io,
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::channel,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    Error,
    daemon::{
        protocol::{ErrorResponse, MAX_FRAME_BYTES, Request, code},
        registry::WorkspaceRegistry,
    },
};

/// How long [`DaemonHandle::spawn`] waits for the daemon to report ready.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Milliseconds since the Unix epoch — the daemon's shared wall-clock
/// helper (activity watermarks, persisted metadata, process records).
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How often an in-flight blocking verb re-checks `client_pid` liveness.
pub(crate) const LIVENESS_INTERVAL: Duration = Duration::from_millis(100);

/// Whether `pid` still refers to a live, non-zombie process.
///
/// Reads `/proc` directly: a missing entry means the process is gone; a
/// `Z`/`X` state means it exited and awaits (or completed) reaping.
#[cfg(target_os = "linux")]
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state letter follows the last ')' (comm can contain spaces).
    match stat
        .rsplit(')')
        .next()
        .and_then(|tail| tail.split_whitespace().next())
    {
        Some(state) => !matches!(state, "Z" | "X" | "x"),
        None => true,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pid_alive(_pid: u32) -> bool {
    // Process-liveness probing is only implemented on Linux; elsewhere the
    // check is a no-op so disconnect cancellation still applies.
    true
}

/// Pids whose death cancels an in-flight verb: the explicit `client_pid`
/// param plus the socket peer's `SO_PEERCRED` pid, deduplicated.
pub(crate) fn watched_pids(client_pid: Option<u32>, peer_pid: Option<u32>) -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();
    for pid in [client_pid, peer_pid].into_iter().flatten() {
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    pids
}

/// Daemon configuration.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Where the control socket is bound. The parent directory is created
    /// `0700` if missing (XDG_RUNTIME_DIR semantics).
    pub socket_path: PathBuf,
    /// Directory for persisted workspace metadata (`workspaces/*.json`).
    pub state_dir: PathBuf,
    /// Idle TTL applied to workspaces created without `idle_ttl_ms`.
    pub default_idle_ttl: Duration,
    /// How often the idle reaper scans workspaces.
    pub reaper_interval: Duration,
    /// Compositor events retained per workspace for `get_changes_since`
    /// diffs. Smaller buffers truncate sooner; a `truncated` diff always
    /// carries a resync hint.
    pub change_log_capacity: usize,
}

impl DaemonConfig {
    /// Sensible defaults: 5-minute workspace idle TTL, 1-second reaper
    /// scans, [`DEFAULT_CHANGE_LOG_CAPACITY`] retained events.
    pub fn new(socket_path: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            socket_path,
            state_dir,
            default_idle_ttl: Duration::from_secs(300),
            reaper_interval: Duration::from_secs(1),
            change_log_capacity: changelog::DEFAULT_CAPACITY,
        }
    }
}

/// State shared by the accept loop, every connection task and the reaper.
pub struct DaemonContext {
    /// The configuration this daemon was started with.
    pub config: DaemonConfig,
    /// The live workspace registry.
    pub registry: WorkspaceRegistry,
    /// When the daemon started serving (for `status` diagnostics).
    pub started_at: Instant,
}

/// A running daemon: owns the accept task, every workspace and the runtime
/// they live on. `shutdown()` (or drop) closes all workspaces, removes the
/// control socket and joins the server thread.
pub struct DaemonHandle {
    socket_path: PathBuf,
    state_dir: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<crate::Result<()>>>,
}

impl DaemonHandle {
    /// Starts the daemon on a dedicated thread running a tokio runtime.
    ///
    /// Returns once the control socket is bound and accepting connections.
    /// Fails if `socket_path` already exists rather than stealing a live
    /// daemon's clients.
    pub fn spawn(config: DaemonConfig) -> crate::Result<Self> {
        let socket_dir = config
            .socket_path
            .parent()
            .ok_or_else(|| Error::Backend("socket_path has no parent directory".into()))?;
        let socket_dir_existed = socket_dir.exists();
        std::fs::create_dir_all(socket_dir).map_err(Error::Io)?;
        #[cfg(unix)]
        if !socket_dir_existed {
            // Only a directory we created gets 0700 — chmod'ing a shared
            // pre-existing parent (e.g. /tmp, XDG_RUNTIME_DIR) would both
            // fail and, when it didn't, lock other clients out.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(Error::Io)?;
        }
        std::fs::create_dir_all(&config.state_dir).map_err(Error::Io)?;
        if config.socket_path.exists() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "control socket {} already exists",
                    config.socket_path.display()
                ),
            )));
        }

        let (ready_tx, ready_rx) = channel::<crate::Result<()>>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let serve_config = config.clone();
        let join = std::thread::Builder::new()
            .name("scry-daemon".to_string())
            .spawn(move || serve_thread(serve_config, ready_tx, shutdown_rx))
            .map_err(Error::Io)?;

        match ready_rx.recv_timeout(SPAWN_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                socket_path: config.socket_path,
                state_dir: config.state_dir,
                shutdown: Some(shutdown_tx),
                join: Some(join),
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                Err(err)
            }
            Err(_) => {
                let _ = shutdown_tx.send(());
                let _ = join.join();
                Err(Error::Disconnected)
            }
        }
    }

    /// The bound control socket path.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// The daemon's state directory.
    pub fn state_dir(&self) -> &PathBuf {
        &self.state_dir
    }

    /// Blocks until the daemon's serve loop ends on its own — on
    /// SIGINT/SIGTERM or an internal shutdown — then joins the server
    /// thread. Unlike [`Self::shutdown`] this never asks the daemon to
    /// stop; it is the foreground `scry daemon` wait path.
    pub fn wait(mut self) -> crate::Result<()> {
        match self.join.take() {
            Some(join) => match join.join() {
                Ok(result) => result,
                Err(_) => Err(Error::Backend("daemon thread panicked".into())),
            },
            None => Ok(()),
        }
    }

    /// Shuts the daemon down: closes every workspace, removes the control
    /// socket and joins the server thread.
    pub fn shutdown(&mut self) -> crate::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            match join.join() {
                Ok(result) => result,
                Err(_) => Err(Error::Backend("daemon thread panicked".into())),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Thread entry: builds the tokio runtime and runs the accept loop.
fn serve_thread(
    config: DaemonConfig,
    ready: std::sync::mpsc::Sender<crate::Result<()>>,
    shutdown: oneshot::Receiver<()>,
) -> crate::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("scry-daemon-rt")
        .build()
        .map_err(Error::Io)?;
    runtime.block_on(serve(config, ready, shutdown))
}

/// The async server body: bind, accept, reap, shut down.
async fn serve(
    config: DaemonConfig,
    ready: std::sync::mpsc::Sender<crate::Result<()>>,
    mut shutdown: oneshot::Receiver<()>,
) -> crate::Result<()> {
    let listener = match UnixListener::bind(&config.socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            let _ = ready.send(Err(Error::Backend(format!(
                "failed to bind control socket: {err}"
            ))));
            return Err(Error::Io(err));
        }
    };
    info!(socket = %config.socket_path.display(), "daemon control socket bound");
    let _ = ready.send(Ok(()));

    let ctx = Arc::new(DaemonContext {
        registry: WorkspaceRegistry::new(
            config.state_dir.clone(),
            config.default_idle_ttl,
            config.change_log_capacity,
        ),
        config: config.clone(),
        started_at: Instant::now(),
    });

    let mut reaper = tokio::time::interval(config.reaper_interval);
    reaper.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(Error::Io)?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(Error::Io)?;

    // Set while a reaper pass is in flight so ticks never overlap — the
    // pass itself runs on its own task, keeping slow teardowns off the
    // accept/signal loop. The flag clears on task exit *and* on panic, so
    // a failed pass cannot disable the reaper permanently.
    let reaping = Arc::new(AtomicBool::new(false));
    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        tokio::spawn(serve_connection(stream, ctx.clone()));
                    }
                    Err(err) => warn!("accept failed: {err}"),
                }
            }
            _ = reaper.tick() => {
                if !reaping.swap(true, Ordering::AcqRel) {
                    let ctx = ctx.clone();
                    let reaping = ReapGuard(reaping.clone());
                    tokio::spawn(async move {
                        ctx.registry.reap_idle().await;
                        drop(reaping);
                    });
                }
            }
            _ = &mut shutdown => break,
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
        }
    }

    info!("daemon shutting down");
    ctx.registry.close_all("daemon_shutdown").await;
    drop(listener);
    if let Err(err) = std::fs::remove_file(&config.socket_path) {
        warn!(socket = %config.socket_path.display(), "failed to remove socket: {err}");
    }
    Ok(())
}

/// Clears the "reaper pass in flight" flag on drop — normal completion
/// or panic alike.
struct ReapGuard(Arc<AtomicBool>);

impl Drop for ReapGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// How often a half-closed connection re-checks whether the peer has
/// dropped the socket entirely.
const GONE_POLL: Duration = Duration::from_millis(50);

/// Serves one client connection: newline-delimited request frames in,
/// response frames out.
///
/// Each request is dispatched on its own task so that long-running verbs
/// (event-driven waits) never head-of-line block later requests on the same
/// connection; the socket writer is serialized behind a mutex so response
/// frames stay whole lines.
///
/// A read-side EOF means the peer stopped *sending* — it does not alone
/// prove the peer went away. `shutdown(SHUT_WR)` ("no more requests, but
/// I'm still reading") must not cancel in-flight work, while a real
/// disconnect must cancel it promptly; [`peer_gone`] polls the socket to
/// tell the two apart.
async fn serve_connection(stream: UnixStream, ctx: Arc<DaemonContext>) {
    // `SO_PEERCRED` identifies the connecting process; requests may also
    // tag a `client_pid` param on top of it. Either signal cancels any
    // in-flight blocking verb the request spawned.
    let peer_pid = stream
        .peer_cred()
        .ok()
        .and_then(|cred| cred.pid())
        .map(|pid| pid as u32);
    // Cancelled on real disconnect (full close, failed write, read error)
    // or connection teardown — observed by every in-flight request.
    let conn_cancel = CancellationToken::new();
    // Requests currently running on this connection; a half-closed
    // connection stays open until they drain.
    let inflight = Arc::new(AtomicUsize::new(0));

    let (read, write) = stream.into_split();
    let writer = Arc::new(Mutex::new(write));
    let mut lines = BufReader::new(read);
    let mut line = String::new();
    let mut half_closed = false;
    loop {
        line.clear();
        if half_closed {
            // The peer can send no more requests; only in-flight responses
            // remain. End when they drain, or sooner when the peer drops
            // the socket entirely (POLLHUP, detected on the next poll).
            if inflight.load(Ordering::Relaxed) == 0
                || peer_gone(lines.get_ref().as_ref().as_raw_fd())
            {
                break;
            }
            tokio::time::sleep(GONE_POLL).await;
            continue;
        }
        match lines.read_line(&mut line).await {
            Ok(0) if peer_gone(lines.get_ref().as_ref().as_raw_fd()) => break, // disconnect
            Ok(0) => half_closed = true, // SHUT_WR: keep serving in-flight requests
            Ok(_) if line.len() > MAX_FRAME_BYTES => {
                let error = ErrorResponse::new(
                    code::INVALID_REQUEST,
                    format!("request exceeds {} byte limit", MAX_FRAME_BYTES),
                );
                write_frame(&writer, protocol::error_frame(None, &error), &conn_cancel).await;
            }
            Ok(_) => {
                let frame = line.trim_end().to_string();
                let ctx = ctx.clone();
                let writer = writer.clone();
                let inflight = inflight.clone();
                let conn_cancel = conn_cancel.clone();
                let conn = router::RequestContext {
                    cancel: conn_cancel.clone(),
                    peer_pid,
                };
                inflight.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let response = handle_frame(&frame, &ctx, conn).await;
                    if let Some(frame) = response {
                        write_frame(&writer, frame, &conn_cancel).await;
                    }
                    inflight.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(err) => {
                debug!("connection read failed: {err}");
                break;
            }
        }
    }
    conn_cancel.cancel();
}

/// Whether the peer has fully dropped its end of the connection.
///
/// After a read-side EOF, `poll` distinguishes a peer that half-closed
/// (`shutdown(SHUT_WR)`: the socket is still writable — `POLLIN|POLLOUT`,
/// no hangup) from one that closed entirely (`POLLHUP`/`POLLERR`, where
/// writes would fail with EPIPE). In-flight requests survive the first
/// but must abort on the second.
#[cfg(unix)]
fn peer_gone(fd: std::os::fd::RawFd) -> bool {
    let mut polled = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: polls one borrowed fd with no timeout; no memory is touched.
    let rc = unsafe { libc::poll(&mut polled, 1, 0) };
    rc > 0 && polled.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// Non-Unix platforms cannot probe the socket state, so an EOF is treated
/// as a disconnect — the safe direction for releasing waits.
#[cfg(not(unix))]
fn peer_gone(_fd: std::os::fd::RawFd) -> bool {
    true
}

/// Parses and dispatches one request frame; `None` means the frame was a
/// notification and gets no response.
async fn handle_frame(
    frame: &str,
    ctx: &Arc<DaemonContext>,
    conn: router::RequestContext,
) -> Option<String> {
    match Request::parse(frame) {
        Ok(request) => {
            let respond = request.expects_response();
            let id = request.id.clone();
            match router::dispatch(ctx, request, conn).await {
                Ok(result) if respond => {
                    Some(protocol::result_frame(&id.unwrap_or(Value::Null), result))
                }
                Ok(_) => None,
                Err(error) if respond => Some(protocol::error_frame(id.as_ref(), &error)),
                Err(_) => None,
            }
        }
        Err(failure) => Some(protocol::error_frame(failure.id.as_ref(), &failure.error)),
    }
}

/// Writes one response frame (one line) on the connection.
///
/// A failed write means the peer went away for good: `cancel` is tripped
/// so the connection's other in-flight requests abort instead of
/// producing responses nobody can read.
async fn write_frame(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    frame: String,
    cancel: &CancellationToken,
) {
    let mut writer = writer.lock().await;
    let mut bytes = frame.into_bytes();
    bytes.push(b'\n');
    if let Err(err) = writer.write_all(&bytes).await {
        debug!("response write failed: {err}");
        cancel.cancel();
    }
}
