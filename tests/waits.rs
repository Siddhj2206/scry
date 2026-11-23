//! Integration tests for the daemon's event-driven wait verbs (M2.5):
//! `wait_for_window`, `wait_for_commit`, `wait_for_idle`, plus `client_pid`
//! tagging and connection-disconnect cancellation.
//!
//! The seam under test is the live daemon over its real Unix-socket
//! JSON-RPC protocol, with real `wayland-client` connections driving the
//! workspace compositor. Blocking waits run on dedicated threads; every
//! synchronization point is either the RPC response itself or a bounded
//! poll of server-side `active_waits` state — no wait or test is
//! synchronized by fixed `sleep()` calls.

mod common;

use std::{
    path::PathBuf,
    process::Command,
    sync::mpsc::{Receiver, channel},
    thread,
    time::{Duration, Instant},
};

use common::daemon::{RPC_TIMEOUT, RpcClient, spawn_daemon};
use serde_json::{Value, json};

const INVALID_PARAMS: i64 = -32602;
const TIMEOUT_CODE: i64 = -32006;
const CANCELLED_CODE: i64 = -32007;
const READ_ONLY_CODE: i64 = -32003;

/// Runs a blocking RPC call on a background thread so the test can drive
/// workspace events from the main thread while the request is in-flight.
fn rpc_on_thread(mut rpc: RpcClient, method: &'static str, params: Value) -> Receiver<Value> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let _ = tx.send(rpc.call_frame(method, params));
    });
    rx
}

/// The workspace's currently in-flight wait count as the daemon reports it.
fn active_waits(rpc: &mut RpcClient, workspace: &str) -> u64 {
    rpc.call("workspace_list", json!({}))["workspaces"]
        .as_array()
        .expect("workspaces list")
        .iter()
        .find(|w| w["id"] == workspace)
        .map(|w| w["active_waits"].as_u64().unwrap_or(0))
        .unwrap_or(0)
}

/// Bounded wait on genuinely-asynchronous daemon state (a wait verb
/// registering or cancelling). Mirrors the reaper test's reasoning: the
/// observed transition is real wall-clock server work, so polling the
/// reported counter is waiting, not simulated synchronization.
fn until_active_waits(rpc: &mut RpcClient, workspace: &str, expected: u64) {
    let deadline = Instant::now() + RPC_TIMEOUT;
    while active_waits(rpc, workspace) != expected {
        assert!(
            Instant::now() < deadline,
            "active_waits for {workspace:?} never reached {expected}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Creates `workspace` and returns its compositor socket path.
fn create_workspace(rpc: &mut RpcClient, id: &str, options: Value) -> PathBuf {
    let mut params = json!({"id": id});
    params
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    rpc.call("workspace_create", params);
    PathBuf::from(
        rpc.call("workspace_list", json!({}))["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == id)
            .expect("workspace listed")["socket_path"]
            .as_str()
            .unwrap(),
    )
}

// -- wait_for_window ---------------------------------------------------------

#[test]
fn wait_for_window_resolves_immediately_for_mapped_window() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let _top = client.map_toplevel(200, 100, "org.test.app", "Test Window");

    let result = rpc.call(
        "wait_for_window",
        json!({"workspace": "w", "app_id": "org.test.app", "timeout_ms": 5_000}),
    );
    let window = &result["window"];
    assert_eq!(window["app_id"], "org.test.app");
    assert_eq!(window["title"], "Test Window");
    assert_eq!(window["pid"], std::process::id());
    assert!(result["seq"].as_u64().unwrap() > 0);
}

#[test]
fn wait_for_window_matches_title_variants_and_pid() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let _top = client.map_toplevel(200, 100, "app", "Hello Scry World");

    for params in [
        json!({"title": "Scry"}), // substring (default)
        json!({"title": "Scry", "title_match": "substring"}),
        json!({"title": "Hello Scry World", "title_match": "exact"}),
        json!({"pid": std::process::id()}), // SO_PEERCRED pid
        json!({"app_id": "app", "title": "World"}), // AND-ed criteria
    ] {
        let mut params = params;
        params["workspace"] = json!("w");
        params["timeout_ms"] = json!(2_000);
        let result = rpc.call("wait_for_window", params.clone());
        assert_eq!(
            result["window"]["app_id"], "app",
            "criteria {params} did not match the mapped window"
        );
    }

    // Criteria that don't match anything block until timeout.
    let frame = rpc.call_frame(
        "wait_for_window",
        json!({
            "workspace": "w",
            "title": "Goodbye",
            "title_match": "exact",
            "timeout_ms": 200,
        }),
    );
    assert_eq!(frame["error"]["code"], TIMEOUT_CODE);
}

#[test]
fn wait_for_window_blocks_until_window_maps() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "late", json!({}));

    let waiter = RpcClient::connect(&daemon.socket_path);
    let rx = rpc_on_thread(
        waiter,
        "wait_for_window",
        json!({
            "workspace": "late",
            "app_id": "late.app",
            "timeout_ms": RPC_TIMEOUT.as_millis() as u64,
        }),
    );
    // The wait is subscribed before we map the window: it must resolve via
    // the event stream, not the initial snapshot.
    until_active_waits(&mut rpc, "late", 1);

    let mut client = common::TestClient::connect(&socket);
    let _top = client.map_toplevel(64, 64, "late.app", "Late");

    let frame = rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("wait_for_window never resolved");
    assert_eq!(frame["result"]["window"]["app_id"], "late.app");
    assert_eq!(frame["result"]["window"]["title"], "Late");
    assert!(frame["result"]["seq"].as_u64().unwrap() > 0);
}

#[test]
fn wait_for_window_resolves_on_window_updated() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "ren", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let top = client.map_toplevel(64, 64, "ren.app", "before");

    let waiter = RpcClient::connect(&daemon.socket_path);
    let rx = rpc_on_thread(
        waiter,
        "wait_for_window",
        json!({
            "workspace": "ren",
            "title": "after",
            "title_match": "exact",
            "timeout_ms": RPC_TIMEOUT.as_millis() as u64,
        }),
    );
    until_active_waits(&mut rpc, "ren", 1);

    // A title change commits a WindowUpdated — the wait resolves on it.
    top.toplevel.set_title("after".to_string());
    top.surface.commit();
    client.roundtrip();

    let frame = rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("wait_for_window never resolved");
    assert_eq!(frame["result"]["window"]["title"], "after");
}

#[test]
fn wait_for_window_times_out_and_validates_params() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "w", json!({}));

    let start = Instant::now();
    let frame = rpc.call_frame(
        "wait_for_window",
        json!({"workspace": "w", "app_id": "nope", "timeout_ms": 200}),
    );
    assert_eq!(frame["error"]["code"], TIMEOUT_CODE);
    assert!(
        start.elapsed() >= Duration::from_millis(200),
        "wait returned before its timeout elapsed"
    );

    // No match criteria is a params error, not a wait.
    let frame = rpc.call_frame(
        "wait_for_window",
        json!({"workspace": "w", "timeout_ms": 200}),
    );
    assert_eq!(frame["error"]["code"], INVALID_PARAMS);

    let frame = rpc.call_frame(
        "wait_for_window",
        json!({"workspace": "w", "title": "x", "title_match": "fuzzy"}),
    );
    assert_eq!(frame["error"]["code"], INVALID_PARAMS);
}

// -- wait_for_commit ---------------------------------------------------------

#[test]
fn wait_for_commit_resolves_immediately_past_after_seq() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let _top = client.map_toplevel(100, 100, "app", "Win");

    // Commits already happened: after_seq = 0 resolves at once.
    let result = rpc.call(
        "wait_for_commit",
        json!({"workspace": "w", "after_seq": 0, "timeout_ms": 5_000}),
    );
    let seq = result["seq"].as_u64().unwrap();
    assert!(seq > 0);

    // Nothing newer than the latest commit: blocks until timeout.
    let start = Instant::now();
    let frame = rpc.call_frame(
        "wait_for_commit",
        json!({"workspace": "w", "after_seq": seq, "timeout_ms": 200}),
    );
    assert_eq!(frame["error"]["code"], TIMEOUT_CODE);
    assert!(start.elapsed() >= Duration::from_millis(200));
}

#[test]
fn wait_for_commit_waits_for_next_commit() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let top = client.map_toplevel(100, 100, "app", "Win");
    let seq0 = rpc.call("wait_for_commit", json!({"workspace": "w", "after_seq": 0}))["seq"]
        .as_u64()
        .unwrap();

    let waiter = RpcClient::connect(&daemon.socket_path);
    let rx = rpc_on_thread(
        waiter,
        "wait_for_commit",
        json!({
            "workspace": "w",
            "after_seq": seq0,
            "timeout_ms": RPC_TIMEOUT.as_millis() as u64,
        }),
    );
    until_active_waits(&mut rpc, "w", 1);

    // A fresh commit on the mapped surface resolves the wait with a
    // strictly greater seq.
    let buffer = client.shm_buffer(100, 100, 0xffaa_bbcc);
    top.surface.attach(Some(&buffer), 0, 0);
    top.surface.commit();
    client.roundtrip();

    let frame = rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("wait_for_commit never resolved");
    assert!(
        frame["result"]["seq"].as_u64().unwrap() > seq0,
        "wait_for_commit seq did not advance past after_seq: {frame}"
    );
}

#[test]
fn wait_for_commit_scopes_to_a_target_window() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let top_a = client.map_toplevel(100, 100, "app.a", "A");
    let top_b = client.map_toplevel(100, 100, "app.b", "B");

    let id_a = rpc.call(
        "wait_for_window",
        json!({"workspace": "w", "app_id": "app.a"}),
    )["window"]["id"]
        .as_u64()
        .unwrap();
    let seq0 = rpc.call("wait_for_commit", json!({"workspace": "w", "after_seq": 0}))["seq"]
        .as_u64()
        .unwrap();

    let waiter = RpcClient::connect(&daemon.socket_path);
    let rx = rpc_on_thread(
        waiter,
        "wait_for_commit",
        json!({
            "workspace": "w",
            "window": id_a,
            "after_seq": seq0,
            "timeout_ms": RPC_TIMEOUT.as_millis() as u64,
        }),
    );
    until_active_waits(&mut rpc, "w", 1);

    // A commit on window B must not resolve the window-A wait.
    let buffer_b = client.shm_buffer(50, 50, 0xff11_2233);
    top_b.surface.attach(Some(&buffer_b), 0, 0);
    top_b.surface.commit();
    client.roundtrip();
    assert!(
        rx.try_recv().is_err(),
        "wait_for_commit resolved on another window's commit"
    );

    // A commit on window A resolves it, attributed to window A.
    let buffer_a = client.shm_buffer(50, 50, 0xff44_5566);
    top_a.surface.attach(Some(&buffer_a), 0, 0);
    top_a.surface.commit();
    client.roundtrip();

    let frame = rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("wait_for_commit never resolved");
    assert_eq!(frame["result"]["window_id"], id_a);
    assert!(frame["result"]["seq"].as_u64().unwrap() > seq0);
}

// -- wait_for_idle -----------------------------------------------------------

#[test]
fn wait_for_idle_resolves_after_quiet_window() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let _top = client.map_toplevel(100, 100, "app", "Win");

    // The last event is already behind us: the wait resolves once quiet_ms
    // has elapsed since it, not instantly.
    let start = Instant::now();
    let result = rpc.call(
        "wait_for_idle",
        json!({"workspace": "w", "quiet_ms": 250, "timeout_ms": 5_000}),
    );
    assert_eq!(result["idle"], true);
    assert!(
        start.elapsed() >= Duration::from_millis(200),
        "wait_for_idle resolved before the quiet window elapsed"
    );
}

#[test]
fn wait_for_idle_waits_out_a_commit_burst() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    let mut client = common::TestClient::connect(&socket);
    let top = client.map_toplevel(100, 100, "app", "Win");
    let buffer = client.shm_buffer(100, 100, 0xff99_00ff);

    let waiter = RpcClient::connect(&daemon.socket_path);
    let start = Instant::now();
    let rx = rpc_on_thread(
        waiter,
        "wait_for_idle",
        json!({
            "workspace": "w",
            "quiet_ms": 300,
            "timeout_ms": RPC_TIMEOUT.as_millis() as u64,
        }),
    );
    until_active_waits(&mut rpc, "w", 1);

    // Commits spread by real frame-clock callbacks (~16ms each): every one
    // resets the quiet timer, so the wait outlives the whole burst.
    for i in 0..5 {
        top.surface.frame(&client.qh, i);
        top.surface.attach(Some(&buffer), 0, 0);
        top.surface.commit();
        client.pump_until(|s| s.frame_callbacks > i as usize);
    }

    let frame = rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("wait_for_idle never resolved");
    assert_eq!(frame["result"]["idle"], true);
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "wait_for_idle resolved before quiet_ms after the last commit"
    );
}

#[test]
fn wait_for_idle_honours_the_overall_timeout() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "w", json!({}));

    // quiet window exceeds the bound: bounded by timeout_ms.
    let start = Instant::now();
    let frame = rpc.call_frame(
        "wait_for_idle",
        json!({"workspace": "w", "quiet_ms": 30_000, "timeout_ms": 200}),
    );
    assert_eq!(frame["error"]["code"], TIMEOUT_CODE);
    assert!(start.elapsed() >= Duration::from_millis(200));
    assert!(start.elapsed() < Duration::from_secs(5));
}

// -- cancellation ------------------------------------------------------------

#[test]
fn client_pid_death_cancels_in_flight_wait() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "w", json!({}));

    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("failed to spawn marker process");

    let waiter = RpcClient::connect(&daemon.socket_path);
    let rx = rpc_on_thread(
        waiter,
        "wait_for_window",
        json!({
            "workspace": "w",
            "app_id": "never",
            "timeout_ms": 30_000,
            "client_pid": child.id(),
        }),
    );
    until_active_waits(&mut rpc, "w", 1);

    child.kill().expect("kill failed");
    child.wait().expect("wait failed");

    let frame = rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("wait was not cancelled after client_pid exited");
    assert_eq!(frame["error"]["code"], CANCELLED_CODE);
    until_active_waits(&mut rpc, "w", 0);
}

#[test]
fn dead_client_pid_cancels_wait_promptly() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "w", json!({}));

    let mut child = Command::new("true").spawn().expect("spawn failed");
    child.wait().expect("wait failed");

    let start = Instant::now();
    let frame = rpc.call_frame(
        "wait_for_window",
        json!({
            "workspace": "w",
            "app_id": "never",
            "timeout_ms": 30_000,
            "client_pid": child.id(),
        }),
    );
    assert_eq!(frame["error"]["code"], CANCELLED_CODE);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "wait on a dead client_pid was not cancelled promptly"
    );
}

#[test]
fn disconnect_cancels_wait_without_leaking_or_blocking_others() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    // A blocking wait on its own connection.
    let mut waiting_conn = RpcClient::connect(&daemon.socket_path);
    waiting_conn.send_request(
        "wait_for_window",
        json!({"workspace": "w", "app_id": "never", "timeout_ms": 30_000}),
    );
    until_active_waits(&mut rpc, "w", 1);

    // Other clients on the same workspace are unaffected while it waits.
    let listed = rpc.call("workspace_list", json!({}));
    assert_eq!(listed["workspaces"].as_array().unwrap().len(), 1);

    // Dropping the socket cancels the in-flight wait: no leaked task.
    drop(waiting_conn);
    until_active_waits(&mut rpc, "w", 0);

    // The daemon and workspace are still fully operational.
    let mut client = common::TestClient::connect(&socket);
    let _top = client.map_toplevel(64, 64, "app", "Win");
    let result = rpc.call(
        "wait_for_window",
        json!({"workspace": "w", "app_id": "app", "timeout_ms": 5_000}),
    );
    assert_eq!(result["window"]["app_id"], "app");
}

#[test]
fn workspace_close_cancels_in_flight_wait() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "w", json!({}));

    // A blocking wait on its own connection, far inside its timeout.
    let waiter = RpcClient::connect(&daemon.socket_path);
    let rx = rpc_on_thread(
        waiter,
        "wait_for_window",
        json!({"workspace": "w", "app_id": "never", "timeout_ms": 30_000}),
    );
    until_active_waits(&mut rpc, "w", 1);

    // Closing the workspace out from under the wait must abort it
    // promptly — the waiter holds the broadcast channel open, so only the
    // workspace's closing signal can wake it before the 30s timeout.
    rpc.call("workspace_close", json!({"workspace": "w"}));
    let frame = rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("wait was not cancelled by workspace_close");
    assert_eq!(frame["error"]["code"], CANCELLED_CODE);
}

#[test]
fn half_closed_connection_keeps_in_flight_wait() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "w", json!({}));

    // A blocking wait, then a write-side shutdown (SHUT_WR): "no more
    // requests, still reading". This must NOT cancel the in-flight wait.
    let mut waiter = RpcClient::connect(&daemon.socket_path);
    waiter.send_request(
        "wait_for_window",
        json!({"workspace": "w", "app_id": "late.app", "timeout_ms": 30_000}),
    );
    until_active_waits(&mut rpc, "w", 1);
    waiter.shutdown_write();

    // Map the matching window: the wait resolves and the response is
    // still delivered on the read half — a spurious cancellation would
    // have produced a CANCELLED error (or torn the socket down) instead.
    let mut client = common::TestClient::connect(&socket);
    let _top = client.map_toplevel(64, 64, "late.app", "Late");
    let frame = waiter.read_frame();
    assert_eq!(frame["result"]["window"]["app_id"], "late.app");

    // The connection is still fully usable from the daemon's side: the
    // workspace lives on and the client sees a clean EOF after the reply.
    until_active_waits(&mut rpc, "w", 0);
}

#[test]
fn half_close_then_full_close_cancels_in_flight_wait() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "w", json!({}));

    let mut waiter = RpcClient::connect(&daemon.socket_path);
    waiter.send_request(
        "wait_for_window",
        json!({"workspace": "w", "app_id": "never", "timeout_ms": 30_000}),
    );
    until_active_waits(&mut rpc, "w", 1);

    // SHUT_WR alone keeps the wait alive; dropping the last fd afterward
    // is a real disconnect and must cancel it promptly.
    waiter.shutdown_write();
    assert_eq!(active_waits(&mut rpc, "w"), 1);
    drop(waiter);
    until_active_waits(&mut rpc, "w", 0);
}

// -- readOnly ----------------------------------------------------------------

#[test]
fn wait_verbs_run_on_read_only_workspaces() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "ro", json!({"read_only": true}));

    // wait_for_idle resolves; the blocking waits time out rather than
    // being rejected as mutations.
    let result = rpc.call(
        "wait_for_idle",
        json!({"workspace": "ro", "quiet_ms": 50, "timeout_ms": 5_000}),
    );
    assert_eq!(result["idle"], true);

    for method in ["wait_for_window", "wait_for_commit"] {
        let frame = rpc.call_frame(
            method,
            json!({"workspace": "ro", "app_id": "nope", "timeout_ms": 150}),
        );
        assert_eq!(frame["error"]["code"], TIMEOUT_CODE, "{method}");
        assert_ne!(frame["error"]["code"], READ_ONLY_CODE, "{method}");
    }
}
