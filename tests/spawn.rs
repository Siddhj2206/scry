//! Integration tests for `spawn_app` (M2.3): managed process launch with
//! the workspace Wayland/`wl_shm`/AT-SPI environment profile, caller
//! overrides, exit-status tracking and child teardown on close/shutdown.
//!
//! The seam under test is the daemon's real Unix-socket JSON-RPC protocol;
//! spawned children are real processes observed through the files they
//! write and `/proc`.

mod common;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::daemon::{RPC_TIMEOUT, RpcClient};
use scry::daemon::{DaemonConfig, DaemonHandle};
use serde_json::{Value, json};
use tempfile::TempDir;

/// A live daemon bound to a fresh temporary runtime/state directory.
struct TestDaemon {
    handle: DaemonHandle,
    dir: TempDir,
    socket_path: PathBuf,
    #[allow(dead_code)]
    state_dir: PathBuf,
}

fn spawn_daemon() -> TestDaemon {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let socket_path = dir.path().join("run/scry.sock");
    let state_dir = dir.path().join("state");
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        state_dir: state_dir.clone(),
        default_idle_ttl: Duration::from_secs(300),
        reaper_interval: Duration::from_millis(25),
    };
    let handle = DaemonHandle::spawn(config).expect("daemon failed to start");
    TestDaemon {
        handle,
        dir,
        socket_path,
        state_dir,
    }
}

/// Waits until `path` exists (a spawned child may still be writing it) and
/// returns its contents.
fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path)
            && !contents.is_empty()
        {
            return contents;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Parses `KEY=value` lines from a spawned `env` dump.
fn parse_env(contents: &str) -> HashMap<String, String> {
    contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The `workspace_list` entry for `id`.
fn workspace_entry(rpc: &mut RpcClient, id: &str) -> Value {
    rpc.call("workspace_list", json!({}))["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("workspace {id:?} not listed"))
}

/// Waits until `pred` holds on the workspace's `workspace_list` entry and
/// returns it.
fn wait_for_workspace(rpc: &mut RpcClient, id: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        let entry = workspace_entry(rpc, id);
        if pred(&entry) {
            return entry;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for workspace {id:?} condition: {entry}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until `pid` no longer exists.
fn wait_for_exit(pid: u64) {
    let deadline = Instant::now() + RPC_TIMEOUT;
    let proc = PathBuf::from(format!("/proc/{pid}"));
    while proc.exists() {
        assert!(Instant::now() < deadline, "process {pid} was never reaped");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn spawn_app_injects_default_wayland_env_profile() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    let created = rpc.call("workspace_create", json!({"id": "envy"}));
    let ws = &created["workspace"];

    // Spawn a helper that dumps its environment to a file.
    let env_file = daemon.dir.path().join("child-env.txt");
    let result = rpc.call(
        "spawn_app",
        json!({
            "workspace": "envy",
            "program": "sh",
            "args": ["-c", "env > \"$0\"", env_file.to_str().unwrap()],
        }),
    );
    assert!(result["pid"].as_u64().unwrap() > 0);
    assert!(result["seq"].is_u64());
    assert_eq!(result["process"]["state"], "running");

    let env = parse_env(&wait_for_file(&env_file));

    // The workspace's own socket is where the child lands.
    let display = Path::new(ws["socket_path"].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(env["WAYLAND_DISPLAY"], display);
    assert_eq!(env["XDG_RUNTIME_DIR"], ws["runtime_dir"].as_str().unwrap());
    // No X11 fallback leaks through.
    assert!(!env.contains_key("DISPLAY"), "DISPLAY was not unset");

    // The default Wayland + wl_shm + AT-SPI profile.
    for (key, value) in [
        ("GDK_BACKEND", "wayland"),
        ("GSK_RENDERER", "cairo"),
        ("QT_QPA_PLATFORM", "wayland"),
        ("MOZ_ENABLE_WAYLAND", "1"),
        ("ELECTRON_OZONE_PLATFORM_HINT", "wayland"),
        ("SDL_VIDEODRIVER", "wayland"),
        ("GTK_A11Y", "atspi"),
        ("ACCESSIBILITY_ENABLED", "1"),
    ] {
        assert_eq!(env.get(key).map(String::as_str), Some(value), "env {key}");
    }
}

#[test]
fn spawn_app_applies_env_overrides_unsets_and_cwd() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "custom"}));

    let env_file = daemon.dir.path().join("override-env.txt");
    let cwd_dir = daemon.dir.path().join("workdir");
    std::fs::create_dir(&cwd_dir).unwrap();
    rpc.call(
        "spawn_app",
        json!({
            "workspace": "custom",
            "program": "sh",
            "args": ["-c", "env > \"$0\"", env_file.to_str().unwrap()],
            "cwd": cwd_dir,
            "env": {
                "GDK_BACKEND": "x11",        // override a default
                "SCRY_MARKER": "hello",      // brand-new variable
                "SDL_VIDEODRIVER": null,     // unset a default
                "GSK_RENDERER": null,        // unset another default
            },
        }),
    );

    let env = parse_env(&wait_for_file(&env_file));
    assert_eq!(env["GDK_BACKEND"], "x11");
    assert_eq!(env["SCRY_MARKER"], "hello");
    assert!(!env.contains_key("SDL_VIDEODRIVER"));
    assert!(!env.contains_key("GSK_RENDERER"));
    // Untouched defaults survive.
    assert_eq!(env["QT_QPA_PLATFORM"], "wayland");

    // The requested working directory is where the child runs.
    let cwd_file = daemon.dir.path().join("cwd.txt");
    rpc.call(
        "spawn_app",
        json!({
            "workspace": "custom",
            "program": "sh",
            "args": ["-c", "pwd > \"$0\"", cwd_file.to_str().unwrap()],
            "cwd": cwd_dir,
        }),
    );
    assert_eq!(
        wait_for_file(&cwd_file).trim(),
        cwd_dir.canonicalize().unwrap().to_str().unwrap()
    );
}

/// Finds the process record for `pid` in a `workspace_list` entry.
fn process_record(entry: &Value, pid: u64) -> &Value {
    entry["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pid"].as_u64() == Some(pid))
        .unwrap_or_else(|| panic!("pid {pid} not tracked: {entry}"))
}

#[test]
fn spawn_app_tracks_child_exit_status() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "statused"}));

    // A clean exit and a non-zero exit are both recorded with their codes.
    let ok = rpc.call(
        "spawn_app",
        json!({"workspace": "statused", "program": "sh", "args": ["-c", "exit 0"]}),
    );
    let crashed = rpc.call(
        "spawn_app",
        json!({"workspace": "statused", "program": "sh", "args": ["-c", "exit 7"]}),
    );
    let ok_pid = ok["pid"].as_u64().unwrap();
    let crashed_pid = crashed["pid"].as_u64().unwrap();

    let entry = wait_for_workspace(&mut rpc, "statused", |entry| {
        let processes = entry["processes"].as_array().unwrap();
        processes.len() == 2 && processes.iter().all(|p| p["state"] != "running")
    });
    let ok_rec = process_record(&entry, ok_pid);
    assert_eq!(ok_rec["state"], "exited");
    assert_eq!(ok_rec["exit_code"], 0);
    let crashed_rec = process_record(&entry, crashed_pid);
    assert_eq!(crashed_rec["state"], "exited");
    assert_eq!(crashed_rec["exit_code"], 7);
    assert!(crashed_rec["exited_at_ms"].as_u64().is_some());

    // A signal death is recorded as such, not confused with an exit code.
    let killed = rpc.call(
        "spawn_app",
        json!({"workspace": "statused", "program": "sh", "args": ["-c", "kill -TERM $$"]}),
    );
    let killed_pid = killed["pid"].as_u64().unwrap();
    let entry = wait_for_workspace(&mut rpc, "statused", |entry| {
        entry["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["pid"].as_u64() == Some(killed_pid) && p["state"] != "running")
    });
    let killed_rec = process_record(&entry, killed_pid);
    assert_eq!(killed_rec["state"], "signaled");
    assert_eq!(killed_rec["signal"], 15);
}

#[test]
fn workspace_close_terminates_spawned_children() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "closing"}));

    // A long-running child and a long-running grandchild both have to die.
    let sleeper = rpc.call(
        "spawn_app",
        json!({"workspace": "closing", "program": "sleep", "args": ["300"]}),
    );
    let tree = rpc.call(
        "spawn_app",
        json!({"workspace": "closing", "program": "sh", "args": ["-c", "sleep 300 & wait"]}),
    );
    let sleeper_pid = sleeper["pid"].as_u64().unwrap();
    let tree_pid = tree["pid"].as_u64().unwrap();
    assert!(PathBuf::from(format!("/proc/{sleeper_pid}")).exists());
    assert!(PathBuf::from(format!("/proc/{tree_pid}")).exists());

    // The shell's grandchild (`sleep` under `sh -c ... & wait`) shares the
    // spawned process group — it must not outlive the workspace either.
    // Wait for the shell to actually fork it.
    let grandchild_pid: u64 = {
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let children =
                std::fs::read_to_string(format!("/proc/{tree_pid}/task/{tree_pid}/children"))
                    .unwrap_or_default();
            if let Some(pid) = children
                .split_whitespace()
                .next()
                .and_then(|p| p.parse().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "spawned shell never forked a grandchild"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    rpc.call("workspace_close", json!({"workspace": "closing"}));

    wait_for_exit(sleeper_pid);
    wait_for_exit(tree_pid);
    wait_for_exit(grandchild_pid);
}

#[test]
fn daemon_shutdown_terminates_spawned_children() {
    let mut daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "doomed"}));
    let spawned = rpc.call(
        "spawn_app",
        json!({"workspace": "doomed", "program": "sleep", "args": ["300"]}),
    );
    let pid = spawned["pid"].as_u64().unwrap();
    assert!(PathBuf::from(format!("/proc/{pid}")).exists());

    daemon.handle.shutdown().expect("daemon shutdown failed");
    wait_for_exit(pid);
}

#[test]
fn spawn_app_rejected_on_read_only_workspace() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call(
        "workspace_create",
        json!({"id": "frozen", "read_only": true}),
    );

    let err = rpc.call_err(
        "spawn_app",
        json!({"workspace": "frozen", "program": "sleep", "args": ["300"]}),
    );
    assert_eq!(err.code, -32003);

    // Nothing was spawned or tracked.
    let entry = workspace_entry(&mut rpc, "frozen");
    assert_eq!(entry["processes"], json!([]));
}

#[test]
fn spawn_app_validates_params() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "params"}));

    // Missing program.
    let err = rpc.call_err("spawn_app", json!({"workspace": "params"}));
    assert_eq!(err.code, -32602);
    // Unknown workspace.
    let err = rpc.call_err(
        "spawn_app",
        json!({"workspace": "ghost", "program": "sleep"}),
    );
    assert_eq!(err.code, -32001);
    // Env var names that would panic Command::env.
    let err = rpc.call_err(
        "spawn_app",
        json!({"workspace": "params", "program": "env", "env": {"BAD=KEY": "v"}}),
    );
    assert_eq!(err.code, -32602);
    // Nonexistent program is a process error, not a crash.
    let err = rpc.call_err(
        "spawn_app",
        json!({"workspace": "params", "program": "definitely-not-a-real-binary"}),
    );
    assert_eq!(err.code, -32009);
    // Nonexistent working directory is a process error too.
    let err = rpc.call_err(
        "spawn_app",
        json!({"workspace": "params", "program": "true", "cwd": "/no/such/dir"}),
    );
    assert_eq!(err.code, -32009);
}
