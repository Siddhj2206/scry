//! Managed child processes: `spawn_app`'s env profile, lifecycle tracking
//! and group teardown.
//!
//! Every app spawned into a workspace lands in its own process group and is
//! tracked by a [`ProcessRegistry`]: a monitor thread reaps the child and
//! records its exit status, and workspace teardown signals the whole group
//! (SIGTERM, then a bounded wait, then SIGKILL) so no orphan survives a
//! `workspace_close` or daemon shutdown.

use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::json;
use tracing::warn;

use crate::daemon::protocol::{ErrorResponse, code};

/// Grace period between SIGTERM and SIGKILL during teardown.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// Bounded wait for monitor threads to reap after SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Poll cadence while waiting for managed children to exit.
const REAP_POLL: Duration = Duration::from_millis(10);

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The default Wayland + `wl_shm` + AT-SPI environment profile injected
/// into every spawned app (before caller overrides). `XDG_RUNTIME_DIR` and
/// `WAYLAND_DISPLAY` are per-workspace and handled separately; `DISPLAY`
/// is always removed.
const DEFAULT_ENV: &[(&str, &str)] = &[
    ("GDK_BACKEND", "wayland"),
    ("GSK_RENDERER", "cairo"),
    ("QT_QPA_PLATFORM", "wayland"),
    ("MOZ_ENABLE_WAYLAND", "1"),
    ("ELECTRON_OZONE_PLATFORM_HINT", "wayland"),
    ("SDL_VIDEODRIVER", "wayland"),
    ("GTK_A11Y", "atspi"),
    ("ACCESSIBILITY_ENABLED", "1"),
];

/// What to launch, after JSON parameter validation.
#[derive(Debug)]
pub struct SpawnSpec {
    /// The executable (`PATH`-resolved unless it contains a `/`).
    pub program: String,
    /// Argument vector (excluding argv[0]).
    pub args: Vec<String>,
    /// Working directory for the child; `None` inherits the daemon's.
    pub cwd: Option<PathBuf>,
    /// Environment adjustments applied on top of the default profile:
    /// `Some(value)` sets, `None` unsets.
    pub env: Vec<(String, Option<String>)>,
}

/// Lifecycle state of a tracked child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Exited(i32),
    Signaled(i32),
}

/// The registry's internal bookkeeping for one child.
struct ProcessRecord {
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    spawned_at_ms: u64,
    state: State,
    exited_at_ms: Option<u64>,
}

/// A serializable health snapshot of one managed process — what
/// `workspace_list` reports and what M2.4's `status` consumes.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    /// The child's process id.
    pub pid: u32,
    /// The spawned executable.
    pub program: String,
    /// The spawned argument vector.
    pub args: Vec<String>,
    /// The requested working directory, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// When the child was spawned (ms since epoch).
    pub spawned_at_ms: u64,
    /// `"running"`, `"exited"` or `"signaled"`.
    pub state: &'static str,
    /// The exit code, when `state == "exited"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The terminating signal, when `state == "signaled"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// When the child was reaped (ms since epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exited_at_ms: Option<u64>,
}

/// Per-workspace registry of spawned children and their exit states.
#[derive(Default)]
pub struct ProcessRegistry {
    records: Arc<Mutex<HashMap<u32, ProcessRecord>>>,
}

impl ProcessRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns `spec` with the workspace's environment profile, registers it
    /// for lifecycle tracking and returns its initial snapshot.
    ///
    /// The child is made a process-group leader so teardown can signal its
    /// whole tree. `runtime_dir`/`wayland_display` are the workspace's
    /// compositor socket location; the daemon's own environment is
    /// inherited underneath the injected profile.
    pub fn spawn(
        &self,
        spec: &SpawnSpec,
        runtime_dir: &Path,
        wayland_display: &OsStr,
    ) -> Result<ProcessInfo, ErrorResponse> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("WAYLAND_DISPLAY", wayland_display)
            .env_remove("DISPLAY");
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in DEFAULT_ENV {
            command.env(key, value);
        }
        // Caller overrides/unsets apply last — they win over every default.
        for (key, value) in &spec.env {
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        // Own process group: teardown signals the group so detached
        // grandchildren die with the app.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|err| {
            ErrorResponse::with_data(
                code::PROCESS,
                format!("failed to spawn {:?}: {err}", spec.program),
                json!({"program": spec.program}),
            )
        })?;
        let pid = child.id();

        let record = ProcessRecord {
            program: spec.program.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            spawned_at_ms: now_ms(),
            state: State::Running,
            exited_at_ms: None,
        };
        let info = ProcessInfo {
            pid,
            program: record.program.clone(),
            args: record.args.clone(),
            cwd: record.cwd.clone(),
            spawned_at_ms: record.spawned_at_ms,
            state: "running",
            exit_code: None,
            signal: None,
            exited_at_ms: None,
        };
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid, record);

        // One monitor thread per child: blocks in wait(), reaps the zombie
        // and records the exit status for `status` reporting.
        let records = self.records.clone();
        std::thread::Builder::new()
            .name(format!("scry-child-monitor-{pid}"))
            .spawn(move || monitor_child(&mut child, pid, &records))
            .map_err(|err| {
                ErrorResponse::new(code::INTERNAL, format!("monitor thread failed: {err}"))
            })?;

        Ok(info)
    }

    /// Health snapshots of every tracked child, oldest first.
    pub fn snapshot(&self) -> Vec<ProcessInfo> {
        let mut infos: Vec<ProcessInfo> = self
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(&pid, record)| ProcessInfo {
                pid,
                program: record.program.clone(),
                args: record.args.clone(),
                cwd: record.cwd.clone(),
                spawned_at_ms: record.spawned_at_ms,
                state: match record.state {
                    State::Running => "running",
                    State::Exited(_) => "exited",
                    State::Signaled(_) => "signaled",
                },
                exit_code: match record.state {
                    State::Exited(code) => Some(code),
                    _ => None,
                },
                signal: match record.state {
                    State::Signaled(sig) => Some(sig),
                    _ => None,
                },
                exited_at_ms: record.exited_at_ms,
            })
            .collect();
        infos.sort_by_key(|info| (info.spawned_at_ms, info.pid));
        infos
    }

    /// Terminates every still-running child: SIGTERM to each process
    /// group, a bounded grace period, then SIGKILL to whatever survives.
    ///
    /// Blocks until all children are reaped (bounded by
    /// [`TERM_GRACE`] + [`KILL_GRACE`]); safe to call from a
    /// `spawn_blocking` teardown path.
    pub fn terminate_all(&self) {
        let running = self.running_pids();
        if running.is_empty() {
            return;
        }
        for pid in &running {
            signal_tree(*pid, libc::SIGTERM);
        }
        if self.await_all_exit(TERM_GRACE) {
            return;
        }
        for pid in self.running_pids() {
            warn!(pid, "managed child ignored SIGTERM; sending SIGKILL");
            signal_tree(pid, libc::SIGKILL);
        }
        if !self.await_all_exit(KILL_GRACE) {
            warn!("managed children still unreaped after SIGKILL");
        }
    }

    /// Pids of children whose monitor has not yet recorded an exit.
    fn running_pids(&self) -> Vec<u32> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, record)| record.state == State::Running)
            .map(|(&pid, _)| pid)
            .collect()
    }

    /// Polls until no tracked child is running or `within` elapses.
    fn await_all_exit(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.running_pids().is_empty() {
                return true;
            }
            std::thread::sleep(REAP_POLL);
        }
        self.running_pids().is_empty()
    }

    /// Every tracked pid (live and reaped), for persisted metadata.
    pub fn pids(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        pids.sort_unstable();
        pids
    }
}

/// Signals `pid`'s whole process group, then `pid` directly as a
/// fallback for a child that re-grouped itself. Errors (ESRCH on an
/// already-gone group) are expected and ignored.
#[cfg(unix)]
fn signal_tree(pid: u32, signal: i32) {
    let pid = pid as i32;
    // SAFETY: kill() takes a pid and signal; no memory is touched.
    unsafe {
        libc::kill(-pid, signal);
        libc::kill(pid, signal);
    }
}

/// Reaps `child` and records how it died.
fn monitor_child(child: &mut Child, pid: u32, records: &Mutex<HashMap<u32, ProcessRecord>>) {
    use std::os::unix::process::ExitStatusExt;
    let state = match child.wait() {
        Ok(status) => match status.code() {
            Some(code) => State::Exited(code),
            None => State::Signaled(status.signal().unwrap_or(0)),
        },
        // wait() only fails when the child was already reaped — treat as
        // exited without a code.
        Err(_) => State::Exited(-1),
    };
    let mut records = records.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(record) = records.get_mut(&pid) {
        record.state = state;
        record.exited_at_ms = Some(now_ms());
    }
}
