//! Workspace lifecycle: creation, idempotency, isolation, metadata
//! persistence and idle reaping.
//!
//! Each [`Workspace`] owns one M1 [`Compositor`] — a dedicated
//! `WAYLAND_DISPLAY` socket, window space, seat, clipboard, frame clock and
//! sequence timeline — so workspaces are isolated by construction. Blocking
//! `CompositorHandle` calls run off the async reactor via
//! [`tokio::task::spawn_blocking`]; the compositor's event receiver is pumped
//! onto a `tokio::sync::broadcast` channel so event-driven verbs (waits,
//! diffs) can subscribe without new plumbing.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    Compositor, CompositorEvent, CompositorHandle, Seq, WindowId,
    compositor::{JoinOutcome, join_bounded},
    daemon::{
        a11y,
        bus::BusManager,
        changelog::{ChangeLog, Mark},
        now_ms,
        portal::PortalManager,
        process::{ProcessInfo, ProcessRegistry, SpawnSpec},
        protocol::{ErrorResponse, code},
    },
    handle::Reply,
};

/// How many recent compositor events each workspace retains for wait
/// verbs' "already happened" checks (`wait_for_commit`).
const RECENT_EVENT_CAP: usize = 512;

/// If `event` records a surface commit newer than `after_seq` (a direct
/// `SurfaceCommitted`, or a window map/update — both produced by commits),
/// returns the commit's `seq`. When `window` is set, only commits
/// attributed to that window match.
pub(crate) fn commit_seq(
    event: &CompositorEvent,
    after_seq: Seq,
    window: Option<WindowId>,
) -> Option<Seq> {
    match event {
        CompositorEvent::SurfaceCommitted { seq, window_id, .. }
            if *seq > after_seq && window.is_none_or(|w| *window_id == Some(w)) =>
        {
            Some(*seq)
        }
        CompositorEvent::WindowMapped { seq, window: w }
        | CompositorEvent::WindowUpdated { seq, window: w }
            if *seq > after_seq && window == Some(w.id) =>
        {
            Some(*seq)
        }
        _ => None,
    }
}

/// Validates an agent-chosen workspace id: a filesystem- and slug-safe
/// identifier of `[A-Za-z0-9_-]` between 1 and 64 characters.
fn is_valid_slug(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Creation-time options for a workspace.
///
/// `read_only` and `keep_open` are fixed at creation: repeating
/// `workspace_create` with the same id but different options is a
/// [`code::WORKSPACE_CONFLICT`] error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOptions {
    /// Reject mutating verbs (input, spawn, placement, outputs, marks).
    #[serde(default, alias = "readOnly")]
    pub read_only: bool,
    /// Exempt the workspace from idle auto-cleanup.
    #[serde(default, alias = "keepOpen")]
    pub keep_open: bool,
    /// Idle time-to-live in milliseconds. `None` means the daemon default.
    #[serde(default, alias = "idleTtlMs", alias = "idle_ttl")]
    pub idle_ttl_ms: Option<u64>,
}

/// The resolved options actually held by a live workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedOptions {
    /// See [`WorkspaceOptions::read_only`].
    pub read_only: bool,
    /// See [`WorkspaceOptions::keep_open`].
    pub keep_open: bool,
    /// Effective idle TTL in milliseconds.
    pub idle_ttl_ms: u64,
}

/// On-disk workspace metadata, persisted in the daemon's state directory.
#[derive(Debug, Serialize)]
struct PersistedMeta {
    id: String,
    state: &'static str,
    read_only: bool,
    keep_open: bool,
    idle_ttl_ms: u64,
    socket_path: PathBuf,
    runtime_dir: PathBuf,
    wayland_display: String,
    created_at_ms: u64,
    last_active_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    close_reason: Option<String>,
    spawned_pids: Vec<u32>,
}

/// How long teardown waits for the event-pump thread before detaching
/// it, so a stuck pump can never deadlock `workspace_close`.
const PUMP_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A live workspace: one isolated compositor plus bookkeeping.
pub struct Workspace {
    id: String,
    options: ResolvedOptions,
    handle: CompositorHandle,
    /// The owning compositor object; `Some` until close teardown takes it.
    /// Kept behind a mutex so closing never races with in-flight calls that
    /// cloned the handle.
    compositor: Mutex<Option<Compositor>>,
    /// Join handle of the event-pump thread; joined during teardown.
    pump: Mutex<Option<JoinHandle<()>>>,
    /// Cancelled at the start of [`Self::teardown`]: in-flight wait verbs
    /// select on it to abort promptly, and `spawn_process` checks it so
    /// children can never leak past the kill window.
    closing: CancellationToken,
    /// Serializes mark allocation so `bump_seq` + `set_mark` stay atomic
    /// and marks remain in seq order.
    mark_guard: tokio::sync::Mutex<()>,
    /// Broadcast of every compositor event, for event-driven verbs.
    events: broadcast::Sender<CompositorEvent>,
    /// Bounded ring of the most recent events, for `wait_for_commit`'s
    /// "already happened" check. Shared with the event-pump thread.
    recent: Arc<Mutex<VecDeque<CompositorEvent>>>,
    /// Managed children spawned by `spawn_app`, tracked until teardown.
    processes: ProcessRegistry,
    /// The workspace's private D-Bus session bus, AT-SPI2 bus and
    /// `at-spi2-registryd` — launched at creation, torn down with it.
    buses: BusManager,
    /// Bounded ring of `get_tree` snapshots — the state behind M3.4's
    /// staleness-checked refs and M3.5's `tree_diff`/`tree_find`.
    tree_snapshots: Mutex<a11y::SnapshotStore>,
    /// The scripted XDG Desktop Portal stub on the session bus —
    /// `org.freedesktop.portal.Desktop`, plus the control verbs'
    /// scripting/history state.
    portal: PortalManager,
    /// Bounded seq-indexed event log plus named marks — the state behind
    /// `get_changes_since`, `mark_set` and `mark_list`. Shared with the
    /// event-pump thread.
    change_log: Arc<Mutex<ChangeLog>>,
    /// Highest sequence number observed (events, command replies and
    /// marks). Shared with the event-pump thread.
    seq: Arc<AtomicU64>,
    /// Wall-clock time of the last compositor event (not command) — the
    /// baseline `wait_for_idle` measures its quiet window from.
    last_event_ms: Arc<AtomicU64>,
    /// Number of wait verbs currently blocked on this workspace.
    active_waits: Arc<AtomicUsize>,
    last_active_ms: Arc<AtomicU64>,
    created_at_ms: u64,
    meta_path: PathBuf,
}

impl Workspace {
    /// Spawns the workspace's compositor off the reactor and wires up the
    /// event pump and metadata persistence.
    async fn spawn(
        id: String,
        options: ResolvedOptions,
        state_dir: PathBuf,
        change_log_capacity: usize,
    ) -> Result<Self, ErrorResponse> {
        let compositor = tokio::task::spawn_blocking(Compositor::spawn)
            .await
            .map_err(|err| ErrorResponse::new(code::INTERNAL, format!("spawn task failed: {err}")))?
            .map_err(|err| {
                ErrorResponse::new(
                    code::COMPOSITOR,
                    format!("failed to spawn compositor for workspace {id:?}: {err}"),
                )
            })?;
        let handle = compositor.handle().clone();

        // Bridge the compositor's mpsc event receiver onto a broadcast
        // channel; event-driven verbs subscribe there. The pump also keeps
        // the seq watermark and idle clock honest between commands.
        let receiver = handle.subscribe().map_err(ErrorResponse::from)?;
        let (events, _) = broadcast::channel(256);
        let seq = Arc::new(AtomicU64::new(0));
        let last_event_ms = Arc::new(AtomicU64::new(now_ms()));
        let last_active_ms = Arc::new(AtomicU64::new(now_ms()));
        let recent = Arc::new(Mutex::new(VecDeque::new()));
        let change_log = Arc::new(Mutex::new(ChangeLog::new(change_log_capacity)));
        let pump = {
            let events = events.clone();
            let seq = seq.clone();
            let last_event = last_event_ms.clone();
            let last_active = last_active_ms.clone();
            let recent = recent.clone();
            let change_log = change_log.clone();
            std::thread::Builder::new()
                .name(format!("scry-ws-events-{id}"))
                .spawn(move || {
                    for event in receiver.iter() {
                        seq.fetch_max(event.seq(), Ordering::Relaxed);
                        last_event.store(now_ms(), Ordering::Relaxed);
                        last_active.store(now_ms(), Ordering::Relaxed);
                        let mut ring = recent.lock().unwrap_or_else(|e| e.into_inner());
                        if ring.len() >= RECENT_EVENT_CAP {
                            ring.pop_front();
                        }
                        ring.push_back(event.clone());
                        drop(ring);
                        change_log
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .record(&event);
                        // No subscribers yet is fine — broadcast for verbs
                        // that need it (wait_*, get_changes_since).
                        let _ = events.send(event);
                    }
                })
                .map_err(|err| {
                    ErrorResponse::new(code::INTERNAL, format!("event pump failed: {err}"))
                })?
        };

        // The private buses launch alongside the compositor, binding
        // sockets inside the same runtime dir. Blocking-friendly: socket
        // binds, process spawns and a bounded readiness wait.
        let buses = {
            let runtime_dir = handle.runtime_dir().to_owned();
            tokio::task::spawn_blocking(move || BusManager::start(runtime_dir))
                .await
                .map_err(|err| {
                    ErrorResponse::new(code::INTERNAL, format!("bus task failed: {err}"))
                })?
        };

        // With the session bus up, serve the scripted XDG Desktop
        // Portal stub on it; a missing bus or failed claim degrades
        // into the portal report, never the workspace.
        let portal = PortalManager::start(&buses).await;

        let workspace = Self {
            meta_path: state_dir.join("workspaces").join(format!("{id}.json")),
            id,
            options,
            handle,
            buses,
            portal,
            compositor: Mutex::new(Some(compositor)),
            pump: Mutex::new(Some(pump)),
            closing: CancellationToken::new(),
            mark_guard: tokio::sync::Mutex::new(()),
            events,
            recent,
            processes: ProcessRegistry::new(),
            tree_snapshots: Mutex::new(a11y::SnapshotStore::default()),
            change_log,
            seq,
            last_event_ms,
            active_waits: Arc::new(AtomicUsize::new(0)),
            last_active_ms,
            created_at_ms: now_ms(),
        };
        if let Err(err) = workspace.persist(None, None) {
            warn!(workspace = %workspace.id, "failed to persist metadata: {err}");
        }
        Ok(workspace)
    }

    /// The workspace's agent-chosen id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The workspace's resolved creation options.
    pub fn options(&self) -> ResolvedOptions {
        self.options
    }

    /// The highest sequence number observed so far.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// Milliseconds since the workspace last saw a command or event.
    pub fn idle_ms(&self) -> u64 {
        now_ms().saturating_sub(self.last_active_ms.load(Ordering::Relaxed))
    }

    /// Marks the workspace active (called on every workspace-scoped command
    /// and from the event pump).
    pub fn touch(&self) {
        self.last_active_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Subscribe to the workspace's compositor event stream.
    ///
    /// Used by event-driven verbs (`wait_for_window`, `wait_for_commit`,
    /// `wait_for_idle`, `get_changes_since`).
    pub fn subscribe(&self) -> broadcast::Receiver<CompositorEvent> {
        self.events.subscribe()
    }

    /// Wall-clock time (epoch ms) of the last compositor event — events
    /// only, unlike [`Self::touch`], which also counts commands.
    pub fn last_event_ms(&self) -> u64 {
        self.last_event_ms.load(Ordering::Relaxed)
    }

    /// The highest `seq` of a retained commit newer than `after_seq` (and
    /// attributed to `window`, when set), or `None` when no matching
    /// commit is still in the recent-event log.
    pub fn recent_commit_seq(&self, after_seq: Seq, window: Option<WindowId>) -> Option<Seq> {
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|event| commit_seq(event, after_seq, window))
            .max()
    }

    /// How many wait verbs are currently blocked on this workspace.
    pub fn active_waits(&self) -> usize {
        self.active_waits.load(Ordering::Relaxed)
    }

    /// Marks a wait verb as in-flight; the returned guard unmarks it on
    /// drop, so cancellation and completion both release it.
    pub fn track_wait(&self) -> WaitTracker {
        self.active_waits.fetch_add(1, Ordering::Relaxed);
        WaitTracker {
            count: self.active_waits.clone(),
        }
    }

    /// The token cancelled when teardown begins. Blocking verbs select
    /// on it so a `workspace_close` never leaves them hanging on the
    /// (kept-alive) broadcast channel.
    pub(crate) fn closing_token(&self) -> CancellationToken {
        self.closing.clone()
    }

    /// Whether the workspace currently holds a live compositor.
    pub fn is_live(&self) -> bool {
        self.compositor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Spawns a managed child process (`spawn_app`) with the workspace's
    /// Wayland + AT-SPI environment profile.
    ///
    /// Fails if the workspace is already being torn down; if teardown races
    /// the spawn, the just-started child is signalled along with the rest.
    pub fn spawn_process(&self, spec: &SpawnSpec) -> Result<ProcessInfo, ErrorResponse> {
        let closing = || {
            ErrorResponse::with_data(
                code::UNKNOWN_WORKSPACE,
                format!("workspace {:?} is closing", self.id),
                json!({"workspace": self.id}),
            )
        };
        // `closing` flips before teardown signals any children, so a spawn
        // that still sees it unset is always re-checked below — no child
        // can slip between the check and `terminate_all`.
        if self.closing.is_cancelled() {
            return Err(closing());
        }
        let info = self.processes.spawn(
            spec,
            self.handle.runtime_dir(),
            self.handle.socket_name(),
            &self.buses.spawn_env(),
        )?;
        if self.closing.is_cancelled() {
            // Teardown raced the spawn: make sure nothing escapes.
            self.processes.terminate_all();
            return Err(closing());
        }
        if let Err(err) = self.persist(None, None) {
            warn!(workspace = %self.id, "failed to persist spawn metadata: {err}");
        }
        Ok(info)
    }

    /// Health snapshots of every child spawned into this workspace — the
    /// process detail behind `workspace_list` and `status`.
    pub fn process_snapshot(&self) -> Vec<ProcessInfo> {
        self.processes.snapshot()
    }

    /// Per-endpoint health of the workspace's private session D-Bus and
    /// AT-SPI2 buses — the `bus` section of `workspace_list` and `status`.
    pub fn bus_report(&self) -> Value {
        self.buses.report()
    }

    /// The workspace's live AT-SPI2 bus address, when its a11y bus is
    /// running — the address `get_tree` connects through.
    pub fn a11y_address(&self) -> Option<String> {
        self.buses.a11y_address()
    }

    /// Records `windows` as the workspace's next `get_tree` snapshot and
    /// returns it with its assigned `snapshot_id`.
    pub fn record_tree_snapshot(&self, windows: Vec<a11y::FusedWindow>) -> a11y::TreeSnapshot {
        self.tree_snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(windows)
    }

    /// A retained `get_tree` snapshot by id — M3.4's staleness checks and
    /// M3.5's `tree_diff` resolve prior snapshots through this.
    pub fn tree_snapshot(&self, id: u64) -> Option<a11y::TreeSnapshot> {
        self.tree_snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// The most recent `get_tree` snapshot, if any — the snapshot
    /// `get_tree`'s `root_ref` resolves against by default.
    pub fn latest_tree_snapshot(&self) -> Option<a11y::TreeSnapshot> {
        self.tree_snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest()
            .cloned()
    }

    /// The workspace's scripted portal stub — scripting queue, invocation
    /// history and health (`portal_*` verbs, `status`).
    pub fn portal(&self) -> &PortalManager {
        &self.portal
    }

    /// Attaches a named mark to the workspace's seq timeline (`mark_set`).
    ///
    /// The mark occupies a fresh timeline point allocated on the
    /// *compositor's* seq counter (`bump_seq`), so later compositor events
    /// land strictly above it and stay visible to `get_changes_since` and
    /// `wait_for_commit`. `mark_guard` serializes allocate-then-push so
    /// concurrent `mark_set` calls keep marks in seq order.
    pub async fn set_mark(
        &self,
        name: String,
        metadata: Option<Value>,
    ) -> Result<Mark, ErrorResponse> {
        let _guard = self.mark_guard.lock().await;
        let seq = self.call(|handle| handle.bump_seq()).await?.seq;
        let mark = self
            .change_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_mark(name, metadata, seq, now_ms());
        Ok(mark)
    }

    /// Every named mark on the timeline, in seq order (`mark_list`).
    pub fn marks(&self) -> Vec<Mark> {
        self.change_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .marks()
            .to_vec()
    }

    /// The deduplicated structured diff over `(since, current_seq]`
    /// (`get_changes_since`).
    pub fn changes_since(&self, since: Seq) -> Value {
        self.change_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .diff(since, self.seq())
    }

    /// Runs a blocking `CompositorHandle` command off the async reactor and
    /// folds its `seq` into the workspace watermark.
    pub async fn call<T, F>(&self, f: F) -> Result<Reply<T>, ErrorResponse>
    where
        T: Send + 'static,
        F: FnOnce(CompositorHandle) -> crate::Result<Reply<T>> + Send + 'static,
    {
        self.touch();
        let handle = self.handle.clone();
        let outcome = tokio::task::spawn_blocking(move || f(handle))
            .await
            .map_err(|err| {
                ErrorResponse::new(code::INTERNAL, format!("compositor task failed: {err}"))
            })?;
        let reply = outcome.map_err(ErrorResponse::from)?;
        self.seq.fetch_max(reply.seq, Ordering::Relaxed);
        Ok(reply)
    }

    /// The JSON summary exposed through `workspace_list`/`workspace_create`.
    pub fn summary(&self) -> Value {
        json!({
            "id": self.id,
            "state": if self.is_live() { "live" } else { "closed" },
            "read_only": self.options.read_only,
            "keep_open": self.options.keep_open,
            "idle_ttl_ms": self.options.idle_ttl_ms,
            "socket_path": self.handle.socket_path(),
            "runtime_dir": self.handle.runtime_dir(),
            "wayland_display": self.handle.socket_name(),
            "seq": self.seq(),
            "created_at_ms": self.created_at_ms,
            "last_active_ms": self.last_active_ms.load(Ordering::Relaxed),
            "processes": self.processes.snapshot(),
            "active_waits": self.active_waits(),
            "bus": self.buses.report(),
            "portal": self.portal.report(),
        })
    }

    /// Writes (or rewrites) the workspace's metadata file atomically.
    fn persist(
        &self,
        closed_at_ms: Option<u64>,
        close_reason: Option<&str>,
    ) -> std::io::Result<()> {
        let meta = PersistedMeta {
            id: self.id.clone(),
            state: if closed_at_ms.is_some() {
                "closed"
            } else {
                "live"
            },
            read_only: self.options.read_only,
            keep_open: self.options.keep_open,
            idle_ttl_ms: self.options.idle_ttl_ms,
            socket_path: self.handle.socket_path().to_owned(),
            runtime_dir: self.handle.runtime_dir().to_owned(),
            wayland_display: self.handle.socket_name().to_string_lossy().into_owned(),
            created_at_ms: self.created_at_ms,
            last_active_ms: self.last_active_ms.load(Ordering::Relaxed),
            closed_at_ms,
            close_reason: close_reason.map(str::to_string),
            spawned_pids: self.processes.pids(),
        };
        if let Some(parent) = self.meta_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.meta_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&meta)?)?;
        std::fs::rename(&tmp, &self.meta_path)?;
        Ok(())
    }

    /// Tears the workspace down: wakes/cancels in-flight waits, shuts the
    /// compositor (joining its thread), stops the event pump and marks the
    /// persisted metadata closed.
    ///
    /// Blocking-friendly: callers run this inside `spawn_blocking`. Every
    /// join is bounded so a wedged worker detaches (with a warning) rather
    /// than deadlocking the close.
    fn teardown(&self, reason: &str) {
        debug!(workspace = %self.id, reason, "tearing down workspace");
        // First: abort in-flight waits (they hold an Arc<Workspace>, so
        // the broadcast channel never closes on its own) and fence off
        // any concurrent spawn_process.
        self.closing.cancel();
        // Children die first (SIGTERM, then bounded SIGKILL) while their
        // Wayland connection still works, so well-behaved apps can exit
        // cleanly. The private bus daemons go down right after — before
        // the compositor's runtime dir (and their sockets) are removed.
        self.processes.terminate_all();
        self.portal.stop();
        self.buses.terminate_all();
        let compositor = self
            .compositor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // Dropping the compositor joins its thread; that drops the event
        // senders, which ends the pump's receiver loop.
        drop(compositor);
        if let Some(pump) = self.pump.lock().unwrap_or_else(|e| e.into_inner()).take() {
            match join_bounded(pump, PUMP_JOIN_TIMEOUT) {
                JoinOutcome::Joined => {}
                JoinOutcome::TimedOut => warn!(
                    workspace = %self.id,
                    "event pump thread did not exit; detached"
                ),
                JoinOutcome::Panicked => warn!(
                    workspace = %self.id,
                    "event pump thread panicked"
                ),
            }
        }
        if let Err(err) = self.persist(Some(now_ms()), Some(reason)) {
            warn!(workspace = %self.id, "failed to persist close metadata: {err}");
        }
    }
}

/// RAII marker for an in-flight wait verb; releases the workspace's
/// `active_waits` slot on drop.
pub struct WaitTracker {
    count: Arc<AtomicUsize>,
}

impl Drop for WaitTracker {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The result of a `workspace_create` call.
pub struct Created {
    /// The live workspace (new or pre-existing).
    pub workspace: Arc<Workspace>,
    /// Whether this call spawned the workspace (`false` = idempotent hit).
    pub created: bool,
}

/// The daemon's registry of live workspaces, keyed by slug id.
pub struct WorkspaceRegistry {
    workspaces: Mutex<HashMap<String, Arc<Workspace>>>,
    /// Per-workspace-id lifecycle locks: `create`/`close` serialize on the
    /// target id only, so a slow teardown of workspace A never stalls
    /// lifecycle operations on workspace B.
    lifecycle_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    state_dir: PathBuf,
    default_idle_ttl_ms: u64,
    /// Retained change-log events per workspace (`get_changes_since`).
    change_log_capacity: usize,
}

impl WorkspaceRegistry {
    /// An empty registry persisting metadata under `state_dir`.
    pub fn new(state_dir: PathBuf, default_idle_ttl: Duration, change_log_capacity: usize) -> Self {
        Self {
            workspaces: Mutex::new(HashMap::new()),
            lifecycle_locks: Mutex::new(HashMap::new()),
            state_dir,
            default_idle_ttl_ms: default_idle_ttl.as_millis() as u64,
            change_log_capacity,
        }
    }

    /// The per-id lifecycle mutex, created on first use.
    fn lifecycle_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.lifecycle_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// Drops `id`'s lifecycle lock entry when `lock` is the last live
    /// clone outside the map, so the map cannot grow unboundedly with the
    /// set of ids the daemon has ever seen.
    fn release_lifecycle_lock(&self, id: &str, lock: &Arc<tokio::sync::Mutex<()>>) {
        // One strong ref is the map's, one is `lock` itself; anything more
        // means another task is queueing on the same id.
        if Arc::strong_count(lock) == 2 {
            self.lifecycle_locks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
        }
    }

    /// Creates a workspace or returns the existing one with matching options.
    pub async fn create(
        &self,
        id: &str,
        options: WorkspaceOptions,
    ) -> Result<Created, ErrorResponse> {
        if !is_valid_slug(id) {
            return Err(ErrorResponse::invalid_params_data(
                format!("invalid workspace id {id:?}: use 1–64 of [A-Za-z0-9_-]"),
                json!({"id": id}),
            ));
        }
        let resolved = ResolvedOptions {
            read_only: options.read_only,
            keep_open: options.keep_open,
            idle_ttl_ms: options.idle_ttl_ms.unwrap_or(self.default_idle_ttl_ms),
        };

        // Serialize on this id only: a teardown of workspace B must not
        // stall a create of workspace A.
        let lock = self.lifecycle_lock(id);
        let result = self.create_locked(id, resolved, &lock).await;
        self.release_lifecycle_lock(id, &lock);
        result
    }

    /// The serialized body of [`Self::create`], holding `id`'s lifecycle
    /// lock across the check-create-insert sequence.
    async fn create_locked(
        &self,
        id: &str,
        resolved: ResolvedOptions,
        lock: &Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Created, ErrorResponse> {
        let _guard = lock.lock().await;
        if let Some(existing) = self.workspaces.lock().unwrap().get(id) {
            if existing.options == resolved {
                existing.touch();
                return Ok(Created {
                    workspace: existing.clone(),
                    created: false,
                });
            }
            return Err(ErrorResponse::with_data(
                code::WORKSPACE_CONFLICT,
                format!("workspace {id:?} already exists with different creation options"),
                json!({
                    "id": id,
                    "existing": {
                        "read_only": existing.options.read_only,
                        "keep_open": existing.options.keep_open,
                        "idle_ttl_ms": existing.options.idle_ttl_ms,
                    },
                    "requested": {
                        "read_only": resolved.read_only,
                        "keep_open": resolved.keep_open,
                        "idle_ttl_ms": resolved.idle_ttl_ms,
                    },
                }),
            ));
        }

        let workspace = Arc::new(
            Workspace::spawn(
                id.to_string(),
                resolved,
                self.state_dir.clone(),
                self.change_log_capacity,
            )
            .await?,
        );
        info!(workspace = id, "workspace created");
        self.workspaces
            .lock()
            .unwrap()
            .insert(id.to_string(), workspace.clone());
        Ok(Created {
            workspace,
            created: true,
        })
    }

    /// The live workspace named `id`, or an `UNKNOWN_WORKSPACE` error.
    pub fn get(&self, id: &str) -> Result<Arc<Workspace>, ErrorResponse> {
        self.workspaces
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ErrorResponse::with_data(
                    code::UNKNOWN_WORKSPACE,
                    format!("unknown workspace {id:?}"),
                    json!({"workspace": id}),
                )
            })
    }

    /// Every live workspace, in creation order (`status`).
    pub fn all(&self) -> Vec<Arc<Workspace>> {
        let mut all: Vec<Arc<Workspace>> =
            self.workspaces.lock().unwrap().values().cloned().collect();
        all.sort_by(|a, b| (a.created_at_ms, a.id.as_str()).cmp(&(b.created_at_ms, b.id.as_str())));
        all
    }

    /// Summaries of every live workspace, in creation order.
    pub fn list(&self) -> Vec<Value> {
        let mut summaries: Vec<Value> = self
            .workspaces
            .lock()
            .unwrap()
            .values()
            .map(|ws| ws.summary())
            .collect();
        // Deterministic ordering for callers: by creation time, then id.
        summaries.sort_by_key(|s| {
            (
                s["created_at_ms"].as_u64().unwrap_or(0),
                s["id"].as_str().unwrap_or("").to_string(),
            )
        });
        summaries
    }

    /// Closes `id`: removes it from the registry, tears down its compositor
    /// and marks its persisted metadata closed.
    pub async fn close(&self, id: &str, reason: &str) -> Result<(), ErrorResponse> {
        // Serialize on this id only (shared with `create`): a
        // teardown-in-flight must finish (and write its "closed" metadata)
        // before a same-id create can return a fresh workspace that would
        // otherwise have its "live" marker clobbered by the old teardown's
        // write.
        let lock = self.lifecycle_lock(id);
        let result = self.close_locked(id, reason, &lock).await;
        self.release_lifecycle_lock(id, &lock);
        result
    }

    /// The serialized body of [`Self::close`], holding `id`'s lifecycle
    /// lock across the remove-teardown-persist sequence.
    async fn close_locked(
        &self,
        id: &str,
        reason: &str,
        lock: &Arc<tokio::sync::Mutex<()>>,
    ) -> Result<(), ErrorResponse> {
        let _guard = lock.lock().await;
        let workspace = self.workspaces.lock().unwrap().remove(id).ok_or_else(|| {
            ErrorResponse::with_data(
                code::UNKNOWN_WORKSPACE,
                format!("unknown workspace {id:?}"),
                json!({"workspace": id}),
            )
        })?;
        info!(workspace = id, reason, "closing workspace");
        let reason = reason.to_string();
        tokio::task::spawn_blocking(move || workspace.teardown(&reason))
            .await
            .map_err(|err| {
                ErrorResponse::new(code::INTERNAL, format!("close task failed: {err}"))
            })?;
        Ok(())
    }

    /// Closes every workspace whose `keep_open` exemption does not apply and
    /// whose idle TTL has elapsed.
    pub async fn reap_idle(&self) {
        let expired: Vec<String> = self
            .workspaces
            .lock()
            .unwrap()
            .values()
            .filter(|ws| !ws.options.keep_open && ws.idle_ms() >= ws.options.idle_ttl_ms)
            .map(|ws| ws.id.clone())
            .collect();
        for id in expired {
            // Re-verify right before tearing down: a command that landed
            // between the scan and now wins the race.
            let still_idle = self
                .workspaces
                .lock()
                .unwrap()
                .get(&id)
                .map(|ws| !ws.options.keep_open && ws.idle_ms() >= ws.options.idle_ttl_ms)
                .unwrap_or(false);
            if !still_idle {
                continue;
            }
            if let Err(err) = self.close(&id, "idle_ttl").await {
                warn!(
                    workspace = id,
                    "idle reaper failed to close: {}", err.message
                );
            }
        }
    }

    /// Closes every live workspace (daemon shutdown).
    pub async fn close_all(&self, reason: &str) {
        let ids: Vec<String> = self.workspaces.lock().unwrap().keys().cloned().collect();
        for id in ids {
            if let Err(err) = self.close(&id, reason).await {
                warn!(workspace = id, "shutdown close failed: {}", err.message);
            }
        }
    }
}
