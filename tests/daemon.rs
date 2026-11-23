//! Integration tests for the `scry` control daemon's Unix-socket JSON-RPC 2.0
//! API and workspace lifecycle (M2.1).
//!
//! The daemon under test is real: it binds a live Unix socket, serves real
//! newline-delimited JSON-RPC frames, and each workspace owns a real
//! `scry::Compositor` whose `WAYLAND_DISPLAY` socket accepts real
//! `wayland-client` connections.

mod common;

use std::{path::PathBuf, time::Duration};

use common::daemon::{RPC_TIMEOUT, RpcClient};
use scry::daemon::{DaemonConfig, DaemonHandle};
use serde_json::{Value, json};
use tempfile::TempDir;

/// A live daemon bound to a fresh temporary runtime/state directory.
struct TestDaemon {
    handle: DaemonHandle,
    _dir: TempDir,
    socket_path: PathBuf,
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

#[test]
fn daemon_serves_jsonrpc_and_shuts_down_cleanly() {
    let mut daemon = spawn_daemon();
    assert!(daemon.socket_path.exists(), "control socket not bound");

    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let result = rpc.call("workspace_list", json!({}));
    assert_eq!(result["workspaces"], json!([]));

    // Clean shutdown removes the control socket and joins all threads.
    daemon.handle.shutdown().expect("daemon shutdown failed");
    assert!(
        !daemon.socket_path.exists(),
        "stale control socket left behind"
    );
}

#[test]
fn rpc_envelope_echoes_command_id_and_reports_seq() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    // String ids are echoed verbatim as command_id.
    let frame = rpc.request_frame(Value::from("cmd-42"), "workspace_list", json!({}));
    assert_eq!(frame["id"], "cmd-42");
    assert_eq!(frame["result"]["command_id"], "cmd-42");

    // A daemon-scoped call reports no workspace seq.
    assert!(frame["result"]["seq"].is_null());

    // Workspace-scoped results carry the workspace's seq.
    rpc.call("workspace_create", json!({"id": "seq-check"}));
    let listed = rpc.call("workspace_list", json!({}));
    let ws_seq = listed["workspaces"][0]["seq"].as_u64().unwrap();
    let outputs = rpc.call("output_list", json!({"workspace": "seq-check"}));
    assert_eq!(outputs["seq"].as_u64().unwrap(), ws_seq);
}

#[test]
fn rpc_rejects_malformed_frames_and_unknown_methods() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    // Not JSON at all → parse error (-32700) with a null id.
    let frame: Value = serde_json::from_str(&rpc.raw_exchange("this is not json")).unwrap();
    assert_eq!(frame["error"]["code"], -32700);
    assert!(frame["id"].is_null());

    // Missing "method" → invalid request (-32600).
    let frame: Value =
        serde_json::from_str(&rpc.raw_exchange(r#"{"jsonrpc":"2.0","id":7}"#)).unwrap();
    assert_eq!(frame["error"]["code"], -32600);
    assert_eq!(frame["id"], 7);

    // Unknown method → method not found (-32601).
    let err = rpc.call_err("no_such_verb", json!({}));
    assert_eq!(err.code, -32601);
}

#[test]
fn workspace_create_spawns_isolated_compositor() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    let result = rpc.call(
        "workspace_create",
        json!({"id": "alpha", "read_only": false}),
    );
    let ws = &result["workspace"];
    assert_eq!(ws["id"], "alpha");
    assert_eq!(ws["read_only"], false);
    assert_eq!(result["created"], true);

    // The workspace's WAYLAND_DISPLAY socket accepts a real client.
    let socket_path = PathBuf::from(ws["socket_path"].as_str().unwrap());
    let client = common::TestClient::connect(&socket_path);
    let globals = client.global_names();
    for expected in [
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "xdg_wm_base",
        "wl_output",
    ] {
        assert!(
            globals.contains(&expected.to_string()),
            "missing {expected}"
        );
    }

    // Metadata is persisted under the daemon's state directory.
    let meta_path = daemon.state_dir.join("workspaces/alpha.json");
    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(&meta_path).expect("workspace metadata missing"),
    )
    .expect("workspace metadata is not valid JSON");
    assert_eq!(meta["id"], "alpha");
    assert_eq!(meta["state"], "live");
    assert_eq!(meta["socket_path"], ws["socket_path"]);
}

#[test]
fn workspace_create_is_idempotent_and_rejects_conflicts() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    let first = rpc.call(
        "workspace_create",
        json!({"id": "auth-flow", "keep_open": true}),
    );
    let second = rpc.call(
        "workspace_create",
        json!({"id": "auth-flow", "keep_open": true}),
    );
    assert_eq!(
        first["workspace"]["socket_path"],
        second["workspace"]["socket_path"]
    );
    assert_eq!(second["created"], false);

    // Conflicting creation-time options fail with a structured error.
    let err = rpc.call_err(
        "workspace_create",
        json!({"id": "auth-flow", "keep_open": false}),
    );
    assert_eq!(err.code, -32002);
    let err = rpc.call_err(
        "workspace_create",
        json!({"id": "auth-flow", "keep_open": true, "read_only": true}),
    );
    assert_eq!(err.code, -32002);

    // Invalid slugs are rejected as invalid params.
    for bad in ["", "has space", "a/b", "../evil", "dot.name"] {
        let err = rpc.call_err("workspace_create", json!({"id": bad}));
        assert_eq!(err.code, -32602, "slug {bad:?} was accepted");
    }
}

#[test]
fn workspace_list_and_close_manage_the_registry() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "one"}));
    rpc.call("workspace_create", json!({"id": "two", "read_only": true}));

    let listed = rpc.call("workspace_list", json!({}));
    let ids: Vec<&str> = listed["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["one", "two"]);

    // Closing removes the workspace, kills its socket and marks metadata.
    let socket_path = PathBuf::from(
        rpc.call("workspace_list", json!({}))["workspaces"][0]["socket_path"]
            .as_str()
            .unwrap()
            .to_string(),
    );
    rpc.call("workspace_close", json!({"workspace": "one"}));
    let listed = rpc.call("workspace_list", json!({}));
    assert_eq!(listed["workspaces"].as_array().unwrap().len(), 1);
    assert!(
        std::os::unix::net::UnixStream::connect(&socket_path).is_err(),
        "closed workspace socket still accepts connections"
    );
    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(daemon.state_dir.join("workspaces/one.json"))
            .expect("closed workspace metadata missing"),
    )
    .unwrap();
    assert_eq!(meta["state"], "closed");
    assert!(meta["closed_at_ms"].as_u64().is_some());

    // Closing an unknown workspace is a structured error.
    let err = rpc.call_err("workspace_close", json!({"workspace": "one"}));
    assert_eq!(err.code, -32001);
    let err = rpc.call_err("workspace_close", json!({"workspace": "ghost"}));
    assert_eq!(err.code, -32001);
}

#[test]
fn workspaces_are_fully_isolated() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "iso-a"}));
    rpc.call("workspace_create", json!({"id": "iso-b"}));

    let listed = rpc.call("workspace_list", json!({}));
    let socket_a = PathBuf::from(listed["workspaces"][0]["socket_path"].as_str().unwrap());
    let socket_b = PathBuf::from(listed["workspaces"][1]["socket_path"].as_str().unwrap());
    assert_ne!(socket_a, socket_b);

    // An output added in workspace A is never visible in workspace B.
    rpc.call(
        "output_add",
        json!({"workspace": "iso-a", "name": "Side-1", "width": 800, "height": 600}),
    );
    let client_a = common::TestClient::connect(&socket_a);
    let client_b = common::TestClient::connect(&socket_b);
    let output_globals = |client: &common::TestClient| {
        client
            .globals
            .contents()
            .clone_list()
            .iter()
            .filter(|g| g.interface == "wl_output")
            .count()
    };
    assert_eq!(output_globals(&client_a), 2);
    assert_eq!(output_globals(&client_b), 1);

    // Activity in A advances only A's sequence timeline.
    let seq_a = rpc.call("output_list", json!({"workspace": "iso-a"}))["seq"]
        .as_u64()
        .unwrap();
    let seq_b = rpc.call("output_list", json!({"workspace": "iso-b"}))["seq"]
        .as_u64()
        .unwrap();
    assert!(
        seq_a > seq_b,
        "seq leaked across workspaces: {seq_a} vs {seq_b}"
    );
}

#[test]
fn output_verbs_configure_virtual_outputs() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "outs"}));

    let added = rpc.call(
        "output_add",
        json!({
            "workspace": "outs",
            "name": "HDMI-9",
            "width": 1280,
            "height": 720,
            "scale": 2.0,
            "x": 1920,
            "y": 0,
        }),
    );
    let output = &added["output"];
    assert_eq!(output["name"], "HDMI-9");
    assert_eq!(output["size"], json!([1280, 720]));
    assert_eq!(output["scale"], 2.0);
    assert_eq!(
        output["geometry"],
        json!({"x":1920,"y":0,"width":640,"height":360})
    );
    assert!(added["seq"].as_u64().unwrap() > 0);

    let listed = rpc.call("output_list", json!({"workspace": "outs"}));
    let names: Vec<&str> = listed["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Virtual-1", "HDMI-9"]);

    rpc.call(
        "output_remove",
        json!({"workspace": "outs", "name": "HDMI-9"}),
    );
    let listed = rpc.call("output_list", json!({"workspace": "outs"}));
    assert_eq!(listed["outputs"].as_array().unwrap().len(), 1);

    // Unknown output / last output removal / bad specs are errors.
    let err = rpc.call_err(
        "output_remove",
        json!({"workspace": "outs", "name": "nope"}),
    );
    assert_eq!(err.code, -32005);
    let err = rpc.call_err(
        "output_remove",
        json!({"workspace": "outs", "name": "Virtual-1"}),
    );
    assert_eq!(err.code, -32602);
    let err = rpc.call_err(
        "output_add",
        json!({"workspace": "outs", "name": "bad", "width": -1, "height": 600}),
    );
    assert_eq!(err.code, -32602);
}

#[test]
fn read_only_workspaces_reject_mutations_but_allow_reads() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call(
        "workspace_create",
        json!({"id": "frozen", "read_only": true}),
    );

    // Read verbs work fine.
    let listed = rpc.call("output_list", json!({"workspace": "frozen"}));
    assert_eq!(listed["outputs"].as_array().unwrap().len(), 1);

    // Mutating verbs are rejected with the read-only error before reaching
    // the compositor.
    for params in [
        json!({"workspace": "frozen", "name": "X-1", "width": 800, "height": 600}),
        json!({"workspace": "frozen", "name": "Virtual-1"}),
    ] {
        let method = if params.get("width").is_some() {
            "output_add"
        } else {
            "output_remove"
        };
        let err = rpc.call_err(method, params);
        assert_eq!(err.code, -32003, "{method} was not read-only rejected");
    }

    // Closing a read-only workspace is still allowed.
    rpc.call("workspace_close", json!({"workspace": "frozen"}));
}

/// Regression: a workspace whose teardown is slow (its app ignores
/// SIGTERM, burning the 2s termination grace) must not stall lifecycle
/// operations on *other* workspaces — create/close serialize per id, not
/// globally.
#[test]
fn slow_teardown_does_not_stall_other_workspace_creates() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "heavy"}));
    // A child that ignores SIGTERM: teardown burns the full ~2s term
    // grace before escalating to SIGKILL.
    rpc.call(
        "spawn_app",
        json!({
            "workspace": "heavy",
            "program": "sh",
            "args": ["-c", "trap '' TERM; while :; do sleep 60; done"],
        }),
    );

    // Close on a helper thread; wait until "heavy" is out of the registry
    // — from then on its multi-second teardown is in flight.
    let socket_path = daemon.socket_path.clone();
    let closer = std::thread::spawn(move || {
        let mut rpc = RpcClient::connect(&socket_path);
        rpc.call("workspace_close", json!({"workspace": "heavy"}));
    });
    let deadline = std::time::Instant::now() + RPC_TIMEOUT;
    loop {
        let listed = rpc.call("workspace_list", json!({}));
        if listed["workspaces"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "heavy workspace never left the registry"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // With per-id serialization the create is bounded by compositor
    // spawn time (~100ms), not the ~2s term grace a global lock would
    // force it to outwait.
    let start = std::time::Instant::now();
    rpc.call("workspace_create", json!({"id": "light"}));
    assert!(
        start.elapsed() < Duration::from_millis(1_500),
        "create stalled behind an unrelated teardown: {:?}",
        start.elapsed()
    );
    closer.join().expect("close thread panicked");
}

/// Regression: the idle reaper runs off the accept loop, so a slow
/// idle-ttl teardown cannot stall `accept()` for new clients.
#[test]
fn slow_reaper_teardown_does_not_stall_new_connections() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call(
        "workspace_create",
        json!({"id": "ephemeral-heavy", "idle_ttl_ms": 150}),
    );
    rpc.call(
        "spawn_app",
        json!({
            "workspace": "ephemeral-heavy",
            "program": "sh",
            "args": ["-c", "trap '' TERM; while :; do sleep 60; done"],
        }),
    );

    // Wait until the reaper removes the workspace: its ~2s teardown is
    // then in flight, and accept() must still serve a fresh client.
    let deadline = std::time::Instant::now() + RPC_TIMEOUT;
    loop {
        let listed = rpc.call("workspace_list", json!({}));
        if listed["workspaces"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "reaper never removed the expired workspace"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let start = std::time::Instant::now();
    let mut fresh = RpcClient::connect(&daemon.socket_path);
    let listed = fresh.call("workspace_list", json!({}));
    assert_eq!(listed["workspaces"], json!([]));
    assert!(
        start.elapsed() < Duration::from_millis(1_500),
        "new client stalled behind the reaper's teardown: {:?}",
        start.elapsed()
    );
}

#[test]
fn idle_reaper_closes_only_expired_workspaces() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call(
        "workspace_create",
        json!({"id": "ephemeral", "idle_ttl_ms": 150}),
    );
    rpc.call(
        "workspace_create",
        json!({"id": "pinned", "keep_open": true, "idle_ttl_ms": 150}),
    );

    // Poll workspace_list until the ephemeral workspace is reaped. Idle TTL
    // is wall-clock, so this is genuine waiting — not simulated synchronization.
    let deadline = std::time::Instant::now() + RPC_TIMEOUT;
    loop {
        let listed = rpc.call("workspace_list", json!({}));
        let ids: Vec<&str> = listed["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect();
        if ids == ["pinned"] {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ephemeral workspace was never reaped: {ids:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(daemon.state_dir.join("workspaces/ephemeral.json"))
            .expect("reaped workspace metadata missing"),
    )
    .unwrap();
    assert_eq!(meta["state"], "closed");
    assert_eq!(meta["close_reason"], "idle_ttl");
}
