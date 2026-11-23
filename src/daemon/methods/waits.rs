//! Event-driven wait verbs: `wait_for_window`, `wait_for_commit`,
//! `wait_for_idle` — plus `client_pid` and disconnect cancellation.
//!
//! Each verb subscribes to the workspace's event broadcast *before*
//! snapshotting current state, so no event can slip between the check and
//! the wait. Waiting is a `tokio::select!` over the event stream, the
//! connection's cancellation token, a periodic `client_pid` liveness check
//! and the overall deadline — zero polling sleeps.

use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{
    CompositorEvent, Seq, WindowId, WindowInfo,
    daemon::{
        now_ms,
        protocol::{ErrorResponse, code},
        registry::{Workspace, commit_seq},
        router::{Call, MethodDef, Scope},
    },
};

/// Default bound on any wait when the caller omits `timeout_ms`.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Default quiet window for `wait_for_idle`.
const DEFAULT_QUIET_MS: u64 = 250;
/// How often an in-flight wait re-checks `client_pid` liveness.
const LIVENESS_INTERVAL: Duration = Duration::from_millis(100);

/// Whether `pid` still refers to a live, non-zombie process.
///
/// Reads `/proc` directly: a missing entry means the process is gone; a
/// `Z`/`X` state means it exited and awaits (or completed) reaping.
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
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
fn pid_alive(_pid: u32) -> bool {
    // Process-liveness probing is only implemented on Linux; elsewhere the
    // check is a no-op so disconnect cancellation still applies.
    true
}

fn timeout_error(verb: &str, timeout_ms: u64) -> ErrorResponse {
    ErrorResponse::with_data(
        code::TIMEOUT,
        format!("{verb} timed out after {timeout_ms}ms"),
        json!({"verb": verb, "timeout_ms": timeout_ms}),
    )
}

fn cancelled(reason: &'static str) -> ErrorResponse {
    ErrorResponse::with_data(
        code::CANCELLED,
        format!("wait cancelled: {reason}"),
        json!({"reason": reason}),
    )
}

/// Pids whose death cancels a wait: the explicit `client_pid` param plus
/// the socket peer's `SO_PEERCRED` pid, deduplicated.
fn watched_pids(client_pid: Option<u32>, peer_pid: Option<u32>) -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();
    for pid in [client_pid, peer_pid].into_iter().flatten() {
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    pids
}

/// One step of a wait's select loop.
enum WaitStep {
    /// A compositor event arrived.
    Event(CompositorEvent),
    /// Events were dropped (broadcast lag); treated as activity.
    Lagged,
    /// The workspace's event stream ended (compositor shutting down).
    Closed,
    /// The caller went away: socket disconnect or `client_pid` exit.
    Cancelled(&'static str),
    /// The overall `timeout_ms` deadline elapsed.
    TimedOut,
    /// The quiet window passed with no events (`wait_for_idle` only).
    QuietElapsed,
    /// A liveness tick with nothing else to do; loops again.
    Tick,
}

/// Drives one event-driven wait: multiplexes the workspace event stream,
/// the workspace-closing and connection-cancel tokens, `client_pid`
/// liveness, and the deadline.
struct WaitDriver {
    rx: broadcast::Receiver<CompositorEvent>,
    ws: Arc<Workspace>,
    cancel: CancellationToken,
    /// Cancelled when the workspace begins teardown — the only reliable
    /// "workspace closed" signal, since the `Arc<Workspace>` this driver
    /// holds keeps the broadcast sender alive past `rx`'s `Closed`.
    ws_closing: CancellationToken,
    pids: Vec<u32>,
    liveness: tokio::time::Interval,
    deadline: tokio::time::Instant,
}

impl WaitDriver {
    /// Subscribes to the workspace event stream *before* callers snapshot
    /// current state, closing the check-then-wait race.
    fn new(
        ws: &Arc<Workspace>,
        cancel: CancellationToken,
        pids: Vec<u32>,
        timeout_ms: u64,
    ) -> Self {
        let mut liveness = tokio::time::interval(LIVENESS_INTERVAL);
        liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            rx: ws.subscribe(),
            ws: ws.clone(),
            cancel,
            ws_closing: ws.closing_token(),
            pids,
            liveness,
            deadline: tokio::time::Instant::now() + Duration::from_millis(timeout_ms.max(1)),
        }
    }

    /// Waits for the next step. `quiet` is `wait_for_idle`'s quiet window:
    /// when set, [`WaitStep::QuietElapsed`] fires once `quiet` passes with
    /// no event winning the race first.
    async fn next(&mut self, quiet: Option<Duration>) -> WaitStep {
        tokio::select! {
            _ = self.cancel.cancelled() => WaitStep::Cancelled("client_disconnected"),
            _ = self.ws_closing.cancelled() => WaitStep::Closed,
            _ = self.liveness.tick() => {
                // A live workspace with an open client is activity: the
                // tick also keeps the idle reaper from reaping us.
                self.ws.touch();
                if self.pids.iter().all(|&pid| pid_alive(pid)) {
                    WaitStep::Tick
                } else {
                    WaitStep::Cancelled("client_pid_exited")
                }
            }
            event = self.rx.recv() => match event {
                Ok(event) => WaitStep::Event(event),
                Err(broadcast::error::RecvError::Lagged(_)) => WaitStep::Lagged,
                Err(broadcast::error::RecvError::Closed) => WaitStep::Closed,
            },
            _ = tokio::time::sleep_until(self.deadline) => WaitStep::TimedOut,
            _ = async {
                match quiet {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            } => WaitStep::QuietElapsed,
        }
    }
}

// -- wait_for_window ----------------------------------------------------------

/// `wait_for_window` — block until a window matching `app_id`, `title`
/// (substring or exact) and/or `pid` is mapped in the workspace.
pub const WAIT_FOR_WINDOW: MethodDef = MethodDef {
    name: "wait_for_window",
    scope: Scope::Workspace { mutating: false },
    run: |call| Box::pin(wait_for_window(call)),
};

#[derive(Deserialize)]
struct WindowWaitParams {
    /// Exact `xdg_toplevel.app_id` match.
    app_id: Option<String>,
    /// Title criterion; `title_match` selects exact vs substring.
    title: Option<String>,
    /// `"substring"` (default) or `"exact"`.
    title_match: Option<String>,
    /// `SO_PEERCRED` pid of the client owning the window.
    pid: Option<u32>,
    /// Overall bound in milliseconds (default 30s).
    timeout_ms: Option<u64>,
    /// Pid whose exit cancels the wait.
    client_pid: Option<u32>,
}

impl WindowWaitParams {
    fn title_substring(&self) -> Result<bool, ErrorResponse> {
        match self.title_match.as_deref() {
            None | Some("substring") => Ok(true),
            Some("exact") => Ok(false),
            Some(other) => Err(ErrorResponse::invalid_params(format!(
                "unknown title_match {other:?}: use \"exact\" or \"substring\""
            ))),
        }
    }

    fn matches(&self, info: &WindowInfo, substring: bool) -> bool {
        if let Some(app_id) = &self.app_id
            && info.app_id.as_deref() != Some(app_id.as_str())
        {
            return false;
        }
        if let Some(title) = &self.title {
            match info.title.as_deref() {
                Some(have) if substring && have.contains(title.as_str()) => {}
                Some(have) if !substring && have == title => {}
                _ => return false,
            }
        }
        if let Some(pid) = self.pid
            && info.pid != Some(pid)
        {
            return false;
        }
        true
    }
}

async fn wait_for_window(call: Call) -> Result<Value, ErrorResponse> {
    let ws = call
        .workspace
        .clone()
        .expect("workspace scope resolves a workspace");
    let params: WindowWaitParams = serde_json::from_value(call.params)
        .map_err(|e| ErrorResponse::invalid_params(e.to_string()))?;
    if params.app_id.is_none() && params.title.is_none() && params.pid.is_none() {
        return Err(ErrorResponse::invalid_params(
            "wait_for_window needs at least one criterion: app_id, title or pid",
        ));
    }
    let substring = params.title_substring()?;
    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

    let mut driver = WaitDriver::new(
        &ws,
        call.cancel,
        watched_pids(params.client_pid, call.peer_pid),
        timeout_ms,
    );
    let _in_flight = ws.track_wait();

    // Already mapped? The subscription is live before this snapshot, so a
    // window mapping during the check still lands in `driver.rx`.
    let reply = ws.call(|handle| handle.windows()).await?;
    if let Some(window) = reply.value.iter().find(|w| params.matches(w, substring)) {
        return Ok(json!({
            "window": window,
            "seq": reply.seq,
        }));
    }

    loop {
        match driver.next(None).await {
            WaitStep::Event(
                CompositorEvent::WindowMapped { seq, window }
                | CompositorEvent::WindowUpdated { seq, window },
            ) if params.matches(&window, substring) => {
                return Ok(json!({
                    "window": window,
                    "seq": seq,
                }));
            }
            WaitStep::Event(_) | WaitStep::Lagged | WaitStep::Tick => {}
            WaitStep::Closed => return Err(cancelled("workspace_closed")),
            WaitStep::Cancelled(reason) => return Err(cancelled(reason)),
            WaitStep::TimedOut => return Err(timeout_error("wait_for_window", timeout_ms)),
            WaitStep::QuietElapsed => unreachable!("wait_for_window sets no quiet window"),
        }
    }
}

// -- wait_for_commit ----------------------------------------------------------

/// `wait_for_commit` — block until a surface commits at `seq > after_seq`,
/// optionally scoped to one window.
pub const WAIT_FOR_COMMIT: MethodDef = MethodDef {
    name: "wait_for_commit",
    scope: Scope::Workspace { mutating: false },
    run: |call| Box::pin(wait_for_commit(call)),
};

#[derive(Deserialize)]
struct CommitWaitParams {
    /// Only commits strictly newer than this seq resolve the wait.
    #[serde(default, alias = "afterSeq")]
    after_seq: Option<Seq>,
    /// Restrict to commits of this window's surfaces.
    #[serde(default, alias = "window_id", alias = "windowId")]
    window: Option<u64>,
    /// Overall bound in milliseconds (default 30s).
    timeout_ms: Option<u64>,
    /// Pid whose exit cancels the wait.
    client_pid: Option<u32>,
}

async fn wait_for_commit(call: Call) -> Result<Value, ErrorResponse> {
    let ws = call
        .workspace
        .clone()
        .expect("workspace scope resolves a workspace");
    let params: CommitWaitParams = serde_json::from_value(call.params)
        .map_err(|e| ErrorResponse::invalid_params(e.to_string()))?;
    let after_seq = params.after_seq.unwrap_or(0);
    let window = params.window.map(WindowId);
    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

    let mut driver = WaitDriver::new(
        &ws,
        call.cancel,
        watched_pids(params.client_pid, call.peer_pid),
        timeout_ms,
    );
    let _in_flight = ws.track_wait();

    // Already past? The recent-event log is scanned after subscribing, so
    // a commit landing between the scan and the loop is still caught.
    if let Some(seq) = ws.recent_commit_seq(after_seq, window) {
        return Ok(json!({
            "seq": seq,
            "window_id": window.map(|w| w.0),
        }));
    }

    loop {
        match driver.next(None).await {
            WaitStep::Event(event) => {
                if let Some(seq) = commit_seq(&event, after_seq, window) {
                    return Ok(json!({
                        "seq": seq,
                        "window_id": window.map(|w| w.0),
                    }));
                }
            }
            WaitStep::Lagged | WaitStep::Tick => {}
            WaitStep::Closed => return Err(cancelled("workspace_closed")),
            WaitStep::Cancelled(reason) => return Err(cancelled(reason)),
            WaitStep::TimedOut => return Err(timeout_error("wait_for_commit", timeout_ms)),
            WaitStep::QuietElapsed => unreachable!("wait_for_commit sets no quiet window"),
        }
    }
}

// -- wait_for_idle ------------------------------------------------------------

/// `wait_for_idle` — block until no surface commits or window lifecycle
/// events occur for `quiet_ms`, bounded by `timeout_ms`.
pub const WAIT_FOR_IDLE: MethodDef = MethodDef {
    name: "wait_for_idle",
    scope: Scope::Workspace { mutating: false },
    run: |call| Box::pin(wait_for_idle(call)),
};

#[derive(Deserialize)]
struct IdleWaitParams {
    /// How long the workspace must stay event-free (default 250ms).
    quiet_ms: Option<u64>,
    /// Overall bound in milliseconds (default 30s).
    timeout_ms: Option<u64>,
    /// Pid whose exit cancels the wait.
    client_pid: Option<u32>,
}

async fn wait_for_idle(call: Call) -> Result<Value, ErrorResponse> {
    let ws = call
        .workspace
        .clone()
        .expect("workspace scope resolves a workspace");
    let params: IdleWaitParams = serde_json::from_value(call.params)
        .map_err(|e| ErrorResponse::invalid_params(e.to_string()))?;
    let quiet_ms = params.quiet_ms.unwrap_or(DEFAULT_QUIET_MS);
    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);

    let mut driver = WaitDriver::new(
        &ws,
        call.cancel,
        watched_pids(params.client_pid, call.peer_pid),
        timeout_ms,
    );
    let _in_flight = ws.track_wait();

    // Seed from the pump's last-event watermark so a commit that landed
    // just before this call still counts against the quiet window.
    let mut last_event_ms = ws.last_event_ms();
    loop {
        let elapsed_ms = now_ms().saturating_sub(last_event_ms);
        if elapsed_ms >= quiet_ms {
            return Ok(json!({
                "idle": true,
                "quiet_ms": quiet_ms,
                "seq": ws.seq(),
            }));
        }
        let remaining = Duration::from_millis(quiet_ms - elapsed_ms);
        match driver.next(Some(remaining)).await {
            WaitStep::Event(_) | WaitStep::Lagged => last_event_ms = now_ms(),
            WaitStep::QuietElapsed | WaitStep::Tick => {}
            WaitStep::Closed => return Err(cancelled("workspace_closed")),
            WaitStep::Cancelled(reason) => return Err(cancelled(reason)),
            WaitStep::TimedOut => return Err(timeout_error("wait_for_idle", timeout_ms)),
        }
    }
}
