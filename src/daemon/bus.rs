//! Per-workspace private D-Bus session bus and AT-SPI2 accessibility bus
//! lifecycle.
//!
//! Every workspace owns two private buses bound to Unix sockets inside its
//! runtime directory — a session bus (`DBUS_SESSION_BUS_ADDRESS`) and an
//! accessibility bus (`AT_SPI_BUS_ADDRESS`) carrying `at-spi2-registryd` —
//! so spawned apps never touch the host user's real session bus or another
//! workspace's. The broker binary is auto-detected (`dbus-broker-launch`
//! on Fedora-style hosts, `dbus-daemon` on Debian-style containers);
//! `at-spi2-registryd` is discovered across `PATH` and the standard
//! `/usr/libexec` / `/usr/lib/at-spi2-core` locations.
//!
//! All bus processes run in their own process groups and are monitored like
//! managed apps: `status` reports each endpoint as `running`, `degraded`
//! (crashed or socket gone) or `stopped` (never launched — missing binary
//! or startup failure), and workspace teardown terminates every group.

use std::{
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tracing::{info, warn};

use crate::daemon::{now_ms, process::signal_tree};

/// Socket filename (inside the workspace runtime dir) for the session bus.
const SESSION_SOCKET: &str = "session-bus";
/// Socket filename for the AT-SPI2 accessibility bus.
const A11Y_SOCKET: &str = "a11y-bus";
/// Log filename capturing each broker's stderr.
const SESSION_LOG: &str = "session-bus.log";
/// Log filename for the a11y broker's stderr.
const A11Y_LOG: &str = "a11y-bus.log";
/// Log filename for `at-spi2-registryd`'s stderr.
const REGISTRY_LOG: &str = "at-spi2-registryd.log";

/// How long bus startup waits for a broker's socket to accept connections
/// before declaring the endpoint stopped.
const BUS_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll cadence while waiting for a broker socket to come up.
const READY_POLL: Duration = Duration::from_millis(10);
/// Grace between SIGTERM and SIGKILL when tearing down bus processes.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// Bounded wait for monitor threads to reap after SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Poll cadence while waiting for bus processes to exit.
const REAP_POLL: Duration = Duration::from_millis(10);

/// Extra directories searched for `at-spi2-registryd` beyond `PATH` —
/// the standard locations Debian (`/usr/libexec` on trixie+,
/// `/usr/lib/at-spi2-core` on older multiarch layouts) and Fedora use.
const REGISTRY_DIRS: &[&str] = &["/usr/libexec", "/usr/lib/at-spi2-core", "/usr/lib"];

/// Which supported session-bus broker binary was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broker {
    /// `dbus-broker-launch` (Fedora-style; needs a systemd-activation fd).
    BrokerLaunch,
    /// `dbus-daemon` (Debian/container-style; binds its own socket).
    Daemon,
}

/// A supervised bus child (a broker or `at-spi2-registryd`).
#[derive(Debug)]
struct BusChild {
    /// What the process is for (`session`, `a11y`, `registry`).
    role: &'static str,
    /// The resolved binary that was exec'd.
    program: PathBuf,
    pid: u32,
    spawned_at_ms: u64,
    state: ChildState,
    exited_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildState {
    Running,
    Exited(i32),
    Signaled(i32),
}

/// One bus endpoint's externally reported health — what `status` and
/// `spawn_app`'s env wiring consume.
#[derive(Debug, Clone)]
pub struct BusEndpoint {
    /// The `unix:path=...` address clients use, once the bus is up.
    address: Option<String>,
    /// The socket file inside the workspace runtime dir.
    socket_path: PathBuf,
    /// Why the endpoint is not running (missing binary / startup failure).
    error: Option<String>,
}

/// The D-Bus environment injected into `spawn_app` children: the
/// workspace's private bus addresses when the corresponding bus is live.
#[derive(Debug, Default, Clone)]
pub struct BusEnv {
    /// Address for `DBUS_SESSION_BUS_ADDRESS` (`None` when the session bus
    /// is not running — children then get no session bus at all rather
    /// than the host's).
    pub session_address: Option<String>,
    /// Address for `AT_SPI_BUS_ADDRESS`.
    pub a11y_address: Option<String>,
}

/// One workspace's private buses: the session bus, the AT-SPI2
/// accessibility bus, and the registry daemon on it.
pub struct BusManager {
    /// The workspace runtime dir; sockets and logs live inside it.
    runtime_dir: PathBuf,
    session: Mutex<BusEndpoint>,
    a11y: Mutex<BusEndpoint>,
    /// Everything the workspace launched, monitored until teardown. Shared
    /// with each child's monitor thread.
    children: ArcMutexChildren,
}

impl BusManager {
    /// Best-effort startup: detects the broker, launches both buses and
    /// `at-spi2-registryd`. Never fails the workspace — missing binaries
    /// and spawn failures land in the endpoint records and surface through
    /// [`Self::report`]/`status` instead.
    ///
    /// Blocking-friendly (binds sockets, spawns processes, waits for bus
    /// readiness): call from a `spawn_blocking` context.
    pub fn start(runtime_dir: PathBuf) -> Self {
        let manager = Self {
            session: Mutex::new(BusEndpoint {
                address: None,
                socket_path: runtime_dir.join(SESSION_SOCKET),
                error: None,
            }),
            a11y: Mutex::new(BusEndpoint {
                address: None,
                socket_path: runtime_dir.join(A11Y_SOCKET),
                error: None,
            }),
            children: std::sync::Arc::new(Mutex::new(Vec::new())),
            runtime_dir,
        };
        match discover_broker() {
            Some((path, broker)) => {
                manager.launch_bus(broker, &path, BusRole::Session);
                manager.launch_bus(broker, &path, BusRole::A11y);
            }
            None => {
                let error = "no D-Bus broker binary found \
                             (looked for dbus-broker-launch and dbus-daemon on PATH)"
                    .to_string();
                manager
                    .session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error = Some(error.clone());
                manager.a11y.lock().unwrap_or_else(|e| e.into_inner()).error = Some(error);
            }
        }
        manager.launch_registry();
        manager
    }

    /// Launches one private bus (`role` picks the socket/log/config names)
    /// through the detected broker, then waits until the socket accepts
    /// connections.
    fn launch_bus(&self, broker: Broker, program: &Path, role: BusRole) {
        let (socket_path, log_name, endpoint) = match role {
            BusRole::Session => (
                self.runtime_dir.join(SESSION_SOCKET),
                SESSION_LOG,
                &self.session,
            ),
            BusRole::A11y => (self.runtime_dir.join(A11Y_SOCKET), A11Y_LOG, &self.a11y),
            BusRole::Registry => unreachable!("registryd is not a bus"),
        };
        let label = role.label();
        let result = match broker {
            Broker::Daemon => self.spawn_daemon_broker(program, &socket_path, log_name, label),
            Broker::BrokerLaunch => {
                self.spawn_broker_launch(program, &socket_path, log_name, label)
            }
        };
        match result {
            Ok(()) => {
                let address = format!("unix:path={}", socket_path.display());
                let mut endpoint = endpoint.lock().unwrap_or_else(|e| e.into_inner());
                endpoint.address = Some(address);
            }
            Err(err) => {
                warn!(workspace_bus = label, "private bus startup failed: {err}");
                let mut endpoint = endpoint.lock().unwrap_or_else(|e| e.into_inner());
                endpoint.error = Some(err);
            }
        }
    }

    /// `dbus-daemon` binds its own `<listen>` socket from the config file;
    /// we then wait for the socket to accept connections.
    fn spawn_daemon_broker(
        &self,
        program: &Path,
        socket_path: &Path,
        log_name: &str,
        label: &'static str,
    ) -> Result<(), String> {
        let config_path = self.write_bus_config(socket_path, label)?;
        let mut command = Command::new(program);
        command
            .arg("--nofork")
            .arg(format!("--config-file={}", config_path.display()))
            .env("XDG_RUNTIME_DIR", &self.runtime_dir);
        self.scrub_inherited_bus_env(&mut command);
        let pid = self.spawn_child(&mut command, label, log_name)?;
        self.await_bus(socket_path, pid)
    }

    /// `dbus-broker-launch` never binds sockets itself — it expects a
    /// systemd-style activated listener. We pre-bind the socket, pass it
    /// as fd 3 (`SD_LISTEN_FDS_START`) with `LISTEN_FDS=1`, and let a
    /// `/bin/sh -c 'LISTEN_PID=$$ exec ...'` wrapper give the child its
    /// own pid for `sd_listen_fds`.
    fn spawn_broker_launch(
        &self,
        program: &Path,
        socket_path: &Path,
        log_name: &str,
        label: &'static str,
    ) -> Result<(), String> {
        let config_path = self.write_bus_config(socket_path, label)?;
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .map_err(|e| format!("failed to bind {}: {e}", socket_path.display()))?;
        let listen_fd = std::os::fd::AsRawFd::as_raw_fd(&listener);

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "LISTEN_PID=$$ exec '{}' --scope=user --config-file='{}'",
                program.display(),
                config_path.display()
            ))
            .env("LISTEN_FDS", "1")
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            // With `--scope=user`, dbus-broker-launch opens a second
            // connection to *a* user bus for systemd service activation
            // (`sd_bus_open_user`). An inherited host address is both an
            // isolation leak and a startup failure when the host has no
            // session bus; an absent or bogus one makes the launcher exit
            // with ENOENT. Point it at this broker's own socket — bound
            // and listening in the parent before the exec — where the
            // activation machinery simply finds no systemd, which is
            // fine because the hermetic config activates nothing.
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", socket_path.display()),
            );
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(move || {
                    // The listener must land exactly at fd 3
                    // (`SD_LISTEN_FDS_START`) without CLOEXEC so it
                    // survives the exec.
                    if libc::dup2(listen_fd, 3) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::fcntl(3, libc::F_SETFD, 0);
                    Ok(())
                });
            }
        }
        let pid = self.spawn_child(&mut command, label, log_name)?;
        // Drop the parent's copy; the child's fd 3 keeps the bound socket.
        drop(listener);
        self.await_bus(socket_path, pid)
    }

    /// Spawns `at-spi2-registryd` on the private a11y bus, discovered
    /// across `PATH` and the standard libexec/lib locations.
    fn launch_registry(&self) {
        let Some(a11y_address) = self.live_address(BusRole::A11y) else {
            return; // No a11y bus: nothing for the registry to sit on.
        };
        let Some(program) = find_on_path(
            "at-spi2-registryd",
            &REGISTRY_DIRS.iter().map(PathBuf::from).collect::<Vec<_>>(),
        ) else {
            warn!("at-spi2-registryd not found on PATH or standard libexec locations");
            return;
        };
        let mut command = Command::new(&program);
        command
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("AT_SPI_BUS_ADDRESS", &a11y_address)
            // registryd also opens the session bus (e.g. for
            // `org.a11y.Status`); point it at the private one when it
            // exists and scrub inherited host bus vars either way.
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("DBUS_STARTER_ADDRESS")
            .env_remove("DBUS_STARTER_BUS_TYPE")
            .env_remove("DISPLAY");
        if let Some(session) = self.live_address(BusRole::Session) {
            command.env("DBUS_SESSION_BUS_ADDRESS", session);
        }
        match self.spawn_child(&mut command, BusRole::Registry.label(), REGISTRY_LOG) {
            Ok(pid) => info!(pid, "at-spi2-registryd launched"),
            Err(err) => warn!("at-spi2-registryd failed to spawn: {err}"),
        }
    }

    /// Removes inherited host-bus variables from a bus child's env so a
    /// daemon launched inside a real desktop session can't leak it into
    /// workspace bus daemons.
    fn scrub_inherited_bus_env(&self, command: &mut Command) {
        for key in [
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SYSTEM_BUS_ADDRESS",
            "DBUS_STARTER_ADDRESS",
            "DBUS_STARTER_BUS_TYPE",
            "AT_SPI_BUS_ADDRESS",
            "DISPLAY",
        ] {
            command.env_remove(key);
        }
    }

    /// Writes the minimal hermetic bus config both brokers accept: a
    /// session-type bus listening on `socket_path`, permissive to the
    /// workspace's own uid, with no service directories — nothing on a
    /// private bus may auto-activate real host services.
    fn write_bus_config(&self, socket_path: &Path, label: &str) -> Result<PathBuf, String> {
        let config_path = self.runtime_dir.join(format!("{label}.conf"));
        let config = format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\"\n \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig>\n  <type>session</type>\n  <listen>unix:path={}</listen>\n  \
             <auth>EXTERNAL</auth>\n  <policy context=\"default\">\n    <allow user=\"*\"/>\n    \
             <allow own=\"*\"/>\n    <allow send_destination=\"*\"/>\n    \
             <allow receive_sender=\"*\"/>\n  </policy>\n</busconfig>\n",
            socket_path.display()
        );
        std::fs::write(&config_path, config)
            .map_err(|e| format!("failed to write {}: {e}", config_path.display()))?;
        Ok(config_path)
    }

    /// Spawns a monitored bus child: stderr appended to a log file inside
    /// the runtime dir, own process group, monitor thread recording the
    /// exit state for `status`.
    fn spawn_child(
        &self,
        command: &mut Command,
        role: &'static str,
        log_name: &str,
    ) -> Result<u32, String> {
        command.stdin(Stdio::null()).stdout(Stdio::null());
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.runtime_dir.join(log_name))
        {
            Ok(log) => {
                command.stderr(Stdio::from(log));
            }
            Err(err) => warn!("failed to open bus log {log_name}: {err}"),
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn {role}: {e}"))?;
        let pid = child.id();
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BusChild {
                role,
                program: command.get_program().into(),
                pid,
                spawned_at_ms: now_ms(),
                state: ChildState::Running,
                exited_at_ms: None,
            });
        let children = self.children_ref();
        std::thread::Builder::new()
            .name(format!("scry-bus-monitor-{pid}"))
            .spawn(move || monitor_child(&mut child, pid, &children))
            .map_err(|e| format!("monitor thread for {role} failed: {e}"))?;
        Ok(pid)
    }

    /// A shared handle to the children list for monitor threads.
    fn children_ref(&self) -> ArcMutexChildren {
        self.children.clone()
    }

    /// Waits until a freshly launched broker's socket accepts a client
    /// connection (backlog-ready), or the child dies / the deadline hits.
    fn await_bus(&self, socket_path: &Path, pid: u32) -> Result<(), String> {
        let deadline = Instant::now() + BUS_READY_TIMEOUT;
        loop {
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return Ok(());
            }
            if !self.child_running(pid) {
                let log = self
                    .runtime_dir
                    .join(if socket_path.ends_with(SESSION_SOCKET) {
                        SESSION_LOG
                    } else {
                        A11Y_LOG
                    });
                let tail = log_tail(&log);
                return Err(format!(
                    "broker exited before {} came up{}",
                    socket_path.display(),
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!(": {tail}")
                    }
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for bus socket {}",
                    socket_path.display()
                ));
            }
            std::thread::sleep(READY_POLL);
        }
    }

    /// Whether the child with `pid` has not yet been reaped.
    fn child_running(&self, pid: u32) -> bool {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|c| c.pid == pid && c.state == ChildState::Running)
    }

    /// The bus address for `role` when its process is running *and* its
    /// socket file still exists — the only state in which the address is
    /// safe to hand to spawned apps.
    fn live_address(&self, role: BusRole) -> Option<String> {
        let endpoint = match role {
            BusRole::Session => &self.session,
            BusRole::A11y => &self.a11y,
            BusRole::Registry => return None,
        };
        // Lock order matches `report()`: children before endpoint.
        let alive = self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|c| c.role == role.label() && c.state == ChildState::Running);
        let endpoint = endpoint.lock().unwrap_or_else(|e| e.into_inner());
        if alive && endpoint.socket_path.exists() {
            endpoint.address.clone()
        } else {
            None
        }
    }

    /// The live AT-SPI2 bus address, when the a11y bus is running — the
    /// address `get_tree` connects through.
    pub fn a11y_address(&self) -> Option<String> {
        self.live_address(BusRole::A11y)
    }

    /// The D-Bus environment to inject into the next `spawn_app` child.
    pub fn spawn_env(&self) -> BusEnv {
        BusEnv {
            session_address: self.live_address(BusRole::Session),
            a11y_address: self.live_address(BusRole::A11y),
        }
    }

    /// The `bus` section of `workspace_list`/`status`: per-endpoint health.
    pub fn report(&self) -> Value {
        let children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let session = endpoint_json(&self.session, BusRole::Session, &children);
        let registry = registry_json(&children);
        let mut a11y = endpoint_json(&self.a11y, BusRole::A11y, &children);
        // The a11y endpoint is only fully running when the bus AND its
        // registry daemon are; a dead/missing registry degrades it.
        a11y["registry"] = registry.clone();
        if a11y["state"] == "running" && registry["state"] != "running" {
            a11y["state"] = json!("degraded");
        }
        json!({
            "session": session,
            "at_spi": a11y,
        })
    }

    /// Terminates every bus process group: SIGTERM to each group, a bounded
    /// grace, then SIGKILL on survivors — the same contract as managed
    /// apps. Safe to call from `spawn_blocking` teardown paths.
    pub fn terminate_all(&self) {
        let pids: Vec<u32> = self
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|c| c.pid)
            .collect();
        if pids.is_empty() {
            return;
        }
        for pid in &pids {
            signal_tree(*pid, libc::SIGTERM);
        }
        if self.await_all_exit(TERM_GRACE) {
            return;
        }
        for pid in self.running_pids() {
            warn!(pid, "bus process ignored SIGTERM; sending SIGKILL");
        }
        for pid in &pids {
            signal_tree(*pid, libc::SIGKILL);
        }
        if !self.await_all_exit(KILL_GRACE) {
            warn!("bus processes still unreaped after SIGKILL");
        }
    }

    /// Pids whose monitor has not yet recorded an exit.
    fn running_pids(&self) -> Vec<u32> {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|c| c.state == ChildState::Running)
            .map(|c| c.pid)
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
}

/// The last few lines of a bus child's captured stderr log.
fn log_tail(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Which endpoint a [`BusChild`] (or spawn) belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusRole {
    Session,
    A11y,
    /// `at-spi2-registryd` — not a bus itself.
    Registry,
}

impl BusRole {
    fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::A11y => "a11y",
            Self::Registry => "registry",
        }
    }
}

/// A cloneable handle on the shared children list for monitor threads.
type ArcMutexChildren = std::sync::Arc<Mutex<Vec<BusChild>>>;

/// Reaps `child` and records how it died (mirrors `process.rs`'s monitor).
fn monitor_child(child: &mut Child, pid: u32, children: &Mutex<Vec<BusChild>>) {
    use std::os::unix::process::ExitStatusExt;
    let state = match child.wait() {
        Ok(status) => match status.code() {
            Some(code) => ChildState::Exited(code),
            None => ChildState::Signaled(status.signal().unwrap_or(0)),
        },
        Err(_) => ChildState::Exited(-1),
    };
    let mut children = children.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(record) = children.iter_mut().find(|c| c.pid == pid) {
        record.state = state;
        record.exited_at_ms = Some(now_ms());
    }
}

/// Finds an executable `name` on `PATH`, then in `extra_dirs`.
fn find_on_path(name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(extra_dirs.iter().cloned());
    dirs.iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is a regular executable file.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// The first usable session-bus broker: `dbus-broker-launch` preferred
/// (the modern broker on Fedora-style hosts), `dbus-daemon` as the
/// Debian/container fallback.
fn discover_broker() -> Option<(PathBuf, Broker)> {
    if let Some(path) = find_on_path("dbus-broker-launch", &[]) {
        return Some((path, Broker::BrokerLaunch));
    }
    find_on_path("dbus-daemon", &[]).map(|path| (path, Broker::Daemon))
}

/// The `state` an endpoint reports: `running` while its child is alive and
/// its socket exists, `degraded` after the child died or the socket went
/// missing, `stopped` when it was never successfully launched.
fn endpoint_state(endpoint: &BusEndpoint, label: &str, children: &[BusChild]) -> &'static str {
    let child = children.iter().find(|c| c.role == label);
    match child {
        Some(c) if c.state == ChildState::Running && endpoint.socket_path.exists() => "running",
        Some(_) => "degraded",
        None => "stopped",
    }
}

/// One endpoint's JSON report.
fn endpoint_json(endpoint: &Mutex<BusEndpoint>, role: BusRole, children: &[BusChild]) -> Value {
    let endpoint = endpoint.lock().unwrap_or_else(|e| e.into_inner());
    let label = role.label();
    let child = children.iter().find(|c| c.role == label);
    let mut json = json!({
        "state": endpoint_state(&endpoint, label, children),
        "socket_path": endpoint.socket_path,
    });
    if let Some(address) = &endpoint.address {
        json["address"] = Value::from(address.clone());
    }
    if let Some(error) = &endpoint.error {
        json["error"] = Value::from(error.clone());
    }
    if let Some(child) = child {
        json["pid"] = Value::from(child.pid);
        json["program"] = Value::from(child.program.display().to_string());
        json["spawned_at_ms"] = Value::from(child.spawned_at_ms);
        json_child_exit(&mut json, child);
    }
    // For a dead endpoint with no recorded reason, the child's captured
    // stderr is the best diagnostic available.
    if json["state"] == "degraded" && json["error"].is_null() {
        let log_name = match label {
            "session" => Some(SESSION_LOG),
            "a11y" => Some(A11Y_LOG),
            _ => None,
        };
        if let Some(log_name) = log_name {
            let tail = log_tail(&endpoint.socket_path.with_file_name(log_name));
            if !tail.is_empty() {
                json["error"] = Value::from(tail);
            }
        }
    }
    json
}

/// Folds a child's exit status into a report object.
fn json_child_exit(json: &mut Value, child: &BusChild) {
    match child.state {
        ChildState::Running => {}
        ChildState::Exited(code) => {
            json["exit_code"] = Value::from(code);
        }
        ChildState::Signaled(sig) => {
            json["signal"] = Value::from(sig);
        }
    }
    if let Some(ms) = child.exited_at_ms {
        json["exited_at_ms"] = Value::from(ms);
    }
}

/// The registry daemon's JSON report (a child record, not a bus endpoint).
fn registry_json(children: &[BusChild]) -> Value {
    match children.iter().find(|c| c.role == "registry") {
        Some(child) => {
            let mut json = json!({
                "state": if child.state == ChildState::Running {
                    "running"
                } else {
                    "degraded"
                },
                "pid": child.pid,
                "program": child.program,
                "spawned_at_ms": child.spawned_at_ms,
            });
            json_child_exit(&mut json, child);
            json
        }
        None => json!({
            "state": "stopped",
            "error": "at-spi2-registryd not found (install at-spi2-core) or a11y bus unavailable",
        }),
    }
}
