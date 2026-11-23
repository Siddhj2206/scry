//! Integration tests for per-workspace private D-Bus session buses and
//! AT-SPI2 accessibility buses (M3.1): launch/ownership/lifecycle, `spawn_app`
//! bus environment injection, `status` bus health and clean teardown.
//!
//! The seam under test is the daemon's real Unix-socket JSON-RPC 2.0
//! protocol, paired with real D-Bus client connections (`zbus` blocking API)
//! to each workspace's private buses — never internal daemon state.

mod common;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use common::daemon::{RPC_TIMEOUT, RpcClient, spawn_daemon};
use serde_json::{Value, json};

/// `workspace_list`'s entry for `id`.
fn workspace_entry(rpc: &mut RpcClient, id: &str) -> Value {
    rpc.call("workspace_list", json!({}))["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("workspace {id:?} not listed"))
}

/// The `status` entry for `id`.
fn status_entry(rpc: &mut RpcClient, id: &str) -> Value {
    let report = rpc.call("status", json!({"workspace": id}));
    report["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("workspace {id:?} not in status report"))
}

/// Waits until `pred` holds on `status`'s workspace entry and returns it.
fn wait_for_status(rpc: &mut RpcClient, id: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        let entry = status_entry(rpc, id);
        if pred(&entry) {
            return entry;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for status condition on {id:?}: {entry}"
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

/// Waits until `path` exists and returns its contents.
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

/// A real D-Bus client connection to `address` (`unix:path=...`).
fn connect_bus(address: &str) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(address)
        .unwrap_or_else(|e| panic!("invalid bus address {address:?}: {e}"))
        .build()
        .unwrap_or_else(|e| panic!("failed to connect to bus {address:?}: {e}"))
}

/// `org.freedesktop.DBus.ListNames` on a live bus connection.
fn list_names(conn: &zbus::blocking::Connection) -> Vec<String> {
    let reply = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .expect("ListNames failed");
    reply
        .body()
        .deserialize::<Vec<String>>()
        .expect("ListNames reply was not a string array")
}

/// Best-effort variant of [`connect_bus`] for optional checks (e.g. a
/// host session bus that may not exist).
fn try_connect_bus(address: &str) -> Option<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::address(address)
        .ok()?
        .build()
        .ok()
}

/// Polls `ListNames` until `name` appears — a just-spawned registryd
/// needs a moment to claim its name after the bus accepts connections.
fn wait_for_name(conn: &zbus::blocking::Connection, name: &str) {
    let deadline = Instant::now() + RPC_TIMEOUT;
    while !list_names(conn).contains(&name.to_string()) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {name} on the bus"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Requests `name` on `conn`, returning `RequestName`'s result code.
fn request_name(conn: &zbus::blocking::Connection, name: &str) -> u32 {
    let reply = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "RequestName",
            &(name, 0u32),
        )
        .expect("RequestName failed");
    reply.body().deserialize::<u32>().unwrap()
}

/// Sends `signal` to `pid` via the `kill` binary (tests are unix-only).
fn kill_pid(pid: u64, signal: &str) {
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .expect("failed to run kill");
    assert!(status.success(), "kill {signal} {pid} failed");
}

#[test]
fn workspace_create_starts_private_session_and_atspi_buses() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    let created = rpc.call("workspace_create", json!({"id": "bused"}));
    let ws = &created["workspace"];
    let runtime_dir = PathBuf::from(ws["runtime_dir"].as_str().unwrap());
    let bus = &ws["bus"];

    // The private session bus: running, addressed inside the workspace's
    // runtime directory, with a real socket file.
    assert_eq!(bus["session"]["state"], "running", "bus report: {bus}");
    let session_addr = bus["session"]["address"].as_str().unwrap();
    let session_sock = PathBuf::from(session_addr.strip_prefix("unix:path=").unwrap());
    assert!(session_sock.starts_with(&runtime_dir));
    assert!(session_sock.exists());

    // The private AT-SPI2 accessibility bus and its registry daemon.
    assert_eq!(bus["at_spi"]["state"], "running", "bus report: {bus}");
    let a11y_addr = bus["at_spi"]["address"].as_str().unwrap();
    let a11y_sock = PathBuf::from(a11y_addr.strip_prefix("unix:path=").unwrap());
    assert!(a11y_sock.starts_with(&runtime_dir));
    assert!(a11y_sock.exists());
    assert_eq!(bus["at_spi"]["registry"]["state"], "running");

    // Both buses really accept D-Bus connections: the session bus answers
    // ListNames and the a11y bus has a live registry.
    let session = connect_bus(session_addr);
    assert!(list_names(&session).contains(&"org.freedesktop.DBus".to_string()));
    let a11y = connect_bus(a11y_addr);
    wait_for_name(&a11y, "org.a11y.atspi.Registry");
}

#[test]
fn workspaces_have_isolated_buses() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "one"}));
    rpc.call("workspace_create", json!({"id": "two"}));
    let one = workspace_entry(&mut rpc, "one");
    let two = workspace_entry(&mut rpc, "two");

    // Distinct buses and sockets — nothing shared.
    let a_session = one["bus"]["session"]["address"].as_str().unwrap();
    let b_session = two["bus"]["session"]["address"].as_str().unwrap();
    assert_ne!(a_session, b_session);
    let a_a11y = one["bus"]["at_spi"]["address"].as_str().unwrap();
    let b_a11y = two["bus"]["at_spi"]["address"].as_str().unwrap();
    assert_ne!(a_a11y, b_a11y);
    assert_ne!(one["runtime_dir"], two["runtime_dir"]);

    // A name claimed on workspace one's session bus is invisible on two's.
    let conn_a = connect_bus(a_session);
    assert_eq!(request_name(&conn_a, "org.scry.IsolationTest"), 1);
    let conn_b = connect_bus(b_session);
    assert!(
        !list_names(&conn_b).contains(&"org.scry.IsolationTest".to_string()),
        "session bus name leaked across workspaces"
    );
    // Neither workspace bus exposes the host user's session bus: when the
    // test itself runs inside a session, pick a well-known host bus name
    // and assert the workspace buses cannot see it.
    if let Ok(host_addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        && let Some(host) = try_connect_bus(&host_addr)
    {
        let host_names = list_names(&host);
        if let Some(host_name) = host_names
            .iter()
            .find(|n| !n.starts_with(':') && n != &"org.freedesktop.DBus")
        {
            for (label, conn) in [("one", &conn_a), ("two", &conn_b)] {
                assert!(
                    !list_names(conn).contains(host_name),
                    "host bus name {host_name} visible on workspace {label}"
                );
            }
        }
    }

    // Both a11y buses carry their own registry — same name, separate buses.
    let a11y_a = connect_bus(a_a11y);
    let a11y_b = connect_bus(b_a11y);
    for conn in [&a11y_a, &a11y_b] {
        wait_for_name(conn, "org.a11y.atspi.Registry");
    }
}

#[test]
fn spawn_app_injects_private_bus_addresses() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    // Pretend the daemon was launched inside a host desktop session: these
    // must never leak into spawned children.
    // SAFETY: scoped to this test binary's process; other tests in this
    // file do not assert on spawned-app bus vars concurrently.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/host/session");
        std::env::set_var("DBUS_STARTER_ADDRESS", "unix:path=/host/starter");
        std::env::set_var("DBUS_STARTER_BUS_TYPE", "session");
        std::env::set_var("AT_SPI_BUS_ADDRESS", "unix:path=/host/a11y");
    }

    rpc.call("workspace_create", json!({"id": "envws"}));
    let ws = workspace_entry(&mut rpc, "envws");
    let session_addr = ws["bus"]["session"]["address"].as_str().unwrap();
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let env_file = daemon.state_dir.join("bus-env.txt");

    rpc.call(
        "spawn_app",
        json!({
            "workspace": "envws",
            "program": "sh",
            "args": ["-c", "env > \"$0\"", env_file.to_str().unwrap()],
        }),
    );
    let env = parse_env(&wait_for_file(&env_file));

    // Both private bus addresses are injected, and the inherited host
    // variables were scrubbed rather than leaking through.
    assert_eq!(env["DBUS_SESSION_BUS_ADDRESS"], session_addr);
    assert_eq!(env["AT_SPI_BUS_ADDRESS"], a11y_addr);
    for key in ["DBUS_STARTER_ADDRESS", "DBUS_STARTER_BUS_TYPE"] {
        assert!(!env.contains_key(key), "{key} leaked into child env");
    }
    assert_ne!(env["DBUS_SESSION_BUS_ADDRESS"], "unix:path=/host/session");
    assert_ne!(env["AT_SPI_BUS_ADDRESS"], "unix:path=/host/a11y");

    unsafe {
        std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
        std::env::remove_var("DBUS_STARTER_ADDRESS");
        std::env::remove_var("DBUS_STARTER_BUS_TYPE");
        std::env::remove_var("AT_SPI_BUS_ADDRESS");
    }

    // Caller overrides and unsets still win over the injected defaults.
    let override_file = daemon.state_dir.join("bus-env-override.txt");
    rpc.call(
        "spawn_app",
        json!({
            "workspace": "envws",
            "program": "sh",
            "args": ["-c", "env > \"$0\"", override_file.to_str().unwrap()],
            "env": {
                "DBUS_SESSION_BUS_ADDRESS": "unix:path=/caller/bus",
                "AT_SPI_BUS_ADDRESS": null,
            },
        }),
    );
    let env = parse_env(&wait_for_file(&override_file));
    assert_eq!(env["DBUS_SESSION_BUS_ADDRESS"], "unix:path=/caller/bus");
    assert!(!env.contains_key("AT_SPI_BUS_ADDRESS"));
}

#[test]
fn workspace_close_terminates_bus_processes_and_sockets() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "closing"}));
    let ws = workspace_entry(&mut rpc, "closing");
    let runtime_dir = PathBuf::from(ws["runtime_dir"].as_str().unwrap());
    let bus = &ws["bus"];
    let pids: Vec<u64> = [
        &bus["session"]["pid"],
        &bus["at_spi"]["pid"],
        &bus["at_spi"]["registry"]["pid"],
    ]
    .iter()
    .filter_map(|p| p.as_u64())
    .collect();
    assert!(pids.len() >= 2, "expected live bus processes: {bus}");

    rpc.call("workspace_close", json!({"workspace": "closing"}));

    for pid in pids {
        wait_for_exit(pid);
    }
    // The whole runtime directory — sockets included — is gone.
    let deadline = Instant::now() + RPC_TIMEOUT;
    while runtime_dir.exists() {
        assert!(
            Instant::now() < deadline,
            "runtime dir {} still exists",
            runtime_dir.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn daemon_shutdown_terminates_bus_processes() {
    let mut daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "doomed"}));
    let ws = workspace_entry(&mut rpc, "doomed");
    let pids: Vec<u64> = [
        &ws["bus"]["session"]["pid"],
        &ws["bus"]["at_spi"]["pid"],
        &ws["bus"]["at_spi"]["registry"]["pid"],
    ]
    .iter()
    .filter_map(|p| p.as_u64())
    .collect();

    daemon.handle.shutdown().expect("daemon shutdown failed");
    for pid in pids {
        wait_for_exit(pid);
    }
}

#[test]
fn idle_reap_terminates_bus_processes() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call(
        "workspace_create",
        json!({"id": "ephemeral", "idle_ttl_ms": 100}),
    );
    let ws = workspace_entry(&mut rpc, "ephemeral");
    let pids: Vec<u64> = [
        &ws["bus"]["session"]["pid"],
        &ws["bus"]["at_spi"]["pid"],
        &ws["bus"]["at_spi"]["registry"]["pid"],
    ]
    .iter()
    .filter_map(|p| p.as_u64())
    .collect();

    // The daemon's reaper closes the idle workspace; its bus daemons must
    // go down with it.
    for pid in pids {
        wait_for_exit(pid);
    }
}

#[test]
fn status_flags_crashed_buses_with_next_steps() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "fragile"}));
    let ws = workspace_entry(&mut rpc, "fragile");

    // Kill the AT-SPI2 registry daemon: the a11y endpoint degrades.
    let registry_pid = ws["bus"]["at_spi"]["registry"]["pid"].as_u64().unwrap();
    kill_pid(registry_pid, "-9");
    let entry = wait_for_status(&mut rpc, "fragile", |entry| {
        entry["bus"]["at_spi"]["registry"]["state"] != "running"
    });
    assert_eq!(entry["bus"]["at_spi"]["registry"]["state"], "degraded");
    assert_eq!(entry["bus"]["at_spi"]["state"], "degraded");
    assert_eq!(entry["healthy"], false);

    // Kill the session broker: the session endpoint degrades and the
    // workspace reports unhealthy with an actionable next step.
    let session_pid = ws["bus"]["session"]["pid"].as_u64().unwrap();
    kill_pid(session_pid, "-9");
    let report = {
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let report = rpc.call("status", json!({"workspace": "fragile"}));
            let entry = &report["workspaces"][0];
            if entry["bus"]["session"]["state"] != "running" {
                break report;
            }
            assert!(Instant::now() < deadline, "session bus never degraded");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let entry = &report["workspaces"][0];
    assert_eq!(entry["bus"]["session"]["state"], "degraded");
    assert_eq!(report["healthy"], false);
    let steps = report["next_steps"].as_array().unwrap();
    assert!(
        steps
            .iter()
            .any(|s| s.as_str().unwrap_or("").contains("fragile")),
        "no workspace-scoped next step: {steps:?}"
    );

    // Teardown still cleans up the surviving bus processes.
    rpc.call("workspace_close", json!({"workspace": "fragile"}));
    wait_for_exit(ws["bus"]["at_spi"]["pid"].as_u64().unwrap());
}
