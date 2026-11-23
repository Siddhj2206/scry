//! Integration tests for `get_tree` (M3.2): fused compositor ↔ AT-SPI2
//! snapshots, `SO_PEERCRED` ↔ `GetConnectionUnixProcessID` pid correlation,
//! absolute coordinate fusion, AT-SPI-less fallback, and `scry tree` CLI
//! output.
//!
//! The seam under test is the daemon's real Unix-socket JSON-RPC 2.0
//! protocol, paired with real `wayland-client` toplevels and a real `zbus`
//! AT-SPI2 peer served on the workspace's private a11y bus — never daemon
//! internals.

mod common;

use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

use common::{
    TestClient,
    a11y::{self, FakeNode},
    daemon::{RPC_TIMEOUT, RpcClient, spawn_daemon},
};
use serde_json::{Value, json};

/// Creates a workspace and returns `(summary, wayland socket path)`.
fn create_workspace(rpc: &mut RpcClient, id: &str, extra: Value) -> (Value, PathBuf) {
    let mut params = json!({"id": id});
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let result = rpc.call("workspace_create", params);
    let ws = result["workspace"].clone();
    let socket = PathBuf::from(ws["socket_path"].as_str().unwrap());
    (ws, socket)
}

/// Maps a toplevel and polls `list_windows` until the daemon reports it,
/// returning its `WindowInfo`.
fn map_window(
    rpc: &mut RpcClient,
    workspace: &str,
    client: &mut TestClient,
    app_id: &str,
    title: &str,
    width: i32,
    height: i32,
) -> Value {
    let _toplevel = client.map_toplevel(width, height, app_id, title);
    wait_for_window(rpc, workspace, |w| w["title"].as_str() == Some(title))
}

/// Polls `list_windows` until a window matching `pred` appears.
fn wait_for_window(rpc: &mut RpcClient, workspace: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        let listed = rpc.call("list_windows", json!({"workspace": workspace}));
        if let Some(window) = listed["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| pred(w))
        {
            return window.clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for window in {workspace:?}: {listed}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Calls `get_tree` and returns the fused `windows` array.
fn tree_windows(rpc: &mut RpcClient, workspace: &str) -> Vec<Value> {
    rpc.call("get_tree", json!({"workspace": workspace}))["windows"]
        .as_array()
        .unwrap()
        .clone()
}

/// The window entry for `window_id` inside a `get_tree` windows array.
fn fused_window(windows: &[Value], window_id: u64) -> &Value {
    windows
        .iter()
        .find(|w| w["window_id"].as_u64() == Some(window_id))
        .unwrap_or_else(|| panic!("window {window_id} missing from get_tree: {windows:?}"))
}

/// A standard fake app tree: an `application` root with one `frame`
/// per window title plus distinctive children, mirroring real toolkit
/// layout.
fn app_tree(frames: &[(&str, &str)]) -> FakeNode {
    let mut root = FakeNode::new("fake-app", "application");
    for (title, marker) in frames {
        let frame = FakeNode::new(title, "frame")
            .bounds([0, 0, 400, 300])
            .child(FakeNode::new(marker, "button").bounds([10, 20, 80, 30]));
        root = root.child(frame);
    }
    root
}

/// The env spec + spawn args for the re-exec'd helper process.
fn spawn_helper(rpc: &mut RpcClient, workspace: &str, ready: PathBuf, spec: Value) -> u64 {
    let exe = std::env::current_exe().expect("current_exe failed");
    let spawned = rpc.call(
        "spawn_app",
        json!({
            "workspace": workspace,
            "program": exe.to_str().unwrap(),
            "args": ["--ignored", "--exact", "scry_a11y_helper", "--nocapture"],
            "env": {"SCRY_A11Y_HELPER": spec.to_string()},
        }),
    );
    let pid = spawned["pid"].as_u64().unwrap();
    let deadline = Instant::now() + RPC_TIMEOUT;
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "helper (pid {pid}) never wrote its ready file"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    pid
}

/// Entry point the spawned helper re-execs into (`--ignored --exact`).
/// Not a test: `run_helper` maps a toplevel and serves the fake a11y
/// tree, then parks forever.
#[test]
#[ignore = "spawned by tests as a fake a11y app, not a test"]
fn scry_a11y_helper() {
    common::a11y::run_helper();
}

/// A mapped window with no AT-SPI2 peer still appears in `get_tree` with
/// its compositor metadata, an explicit `unavailable` status and a
/// pixel-capture hint — and `capture_window` keeps working.
#[test]
fn get_tree_reports_window_without_a11y_tree() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "plain", json!({}));
    let mut client = TestClient::connect(&socket);
    let window = map_window(
        &mut rpc,
        "plain",
        &mut client,
        "org.scry.Plain",
        "No Tree",
        320,
        200,
    );
    let window_id = window["id"].as_u64().unwrap();

    let tree = rpc.call("get_tree", json!({"workspace": "plain"}));
    assert!(tree["snapshot_id"].as_u64().unwrap() >= 1);
    assert!(tree["command_id"].as_u64().is_some());
    assert!(tree["seq"].as_u64().is_some());

    let windows = tree["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let w = &windows[0];
    assert_eq!(w["window_id"].as_u64().unwrap(), window_id);
    assert_eq!(w["app_id"], "org.scry.Plain");
    assert_eq!(w["title"], "No Tree");
    assert_eq!(w["pid"], std::process::id());
    assert_eq!(w["a11y_status"], "unavailable");
    assert!(w["a11y_error"].is_string(), "no error detail: {w}");
    assert!(
        w["hint"].as_str().unwrap().contains("capture_window"),
        "expected a capture_window hint: {w}"
    );
    assert!(w["root"].is_null());
    assert_eq!(w["geometry"]["width"], 320);
    assert_eq!(w["geometry"]["height"], 200);

    // The pixel fallback the hint promises still works.
    let capture = rpc.call(
        "capture_window",
        json!({"workspace": "plain", "window": window_id}),
    );
    assert_eq!(capture["format"], "png");
    assert!(capture["data"].as_str().unwrap().len() > 100);
}

/// `snapshot_id` is monotonic per workspace.
#[test]
fn get_tree_snapshot_ids_are_monotonic() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "snaps", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "snaps",
        &mut client,
        "org.scry.Snaps",
        "Snap",
        100,
        100,
    );

    let first = rpc.call("get_tree", json!({"workspace": "snaps"}))["snapshot_id"]
        .as_u64()
        .unwrap();
    let second = rpc.call("get_tree", json!({"workspace": "snaps"}))["snapshot_id"]
        .as_u64()
        .unwrap();
    assert!(
        second > first,
        "snapshot ids must increase: {first} → {second}"
    );
}

/// `get_tree` fuses a registered AT-SPI2 peer into its window by exact
/// pid correlation (`SO_PEERCRED` ↔ `GetConnectionUnixProcessID` — both
/// are this process), walks the subtree under fused absolute coordinates
/// and assigns deterministic `eN` refs.
#[test]
fn get_tree_fuses_peer_via_pid_correlation() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "fused", json!({}));
    let mut client = TestClient::connect(&socket);
    let window = map_window(
        &mut rpc,
        "fused",
        &mut client,
        "org.scry.Fused",
        "Tree Window",
        400,
        300,
    );
    let window_id = window["id"].as_u64().unwrap();

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &app_tree(&[("Tree Window", "OK")]));

    let tree = rpc.call("get_tree", json!({"workspace": "fused"}));
    assert_eq!(tree["a11y"]["state"], "connected");
    assert!(tree["a11y"]["registered_apps"].as_u64().unwrap() >= 1);

    let w = fused_window(tree["windows"].as_array().unwrap(), window_id);
    assert_eq!(w["a11y_status"], "ok", "window entry: {w}");
    assert_eq!(w["a11y"]["pid"], std::process::id());
    assert_eq!(w["a11y"]["app_name"], "fake-app");

    let root = &w["root"];
    assert_eq!(root["ref"], "e1");
    assert_eq!(root["role"], "frame");
    assert_eq!(root["name"], "Tree Window");
    // The frame's toolkit extents were window-relative (0,0,400,300);
    // fused onto the window's compositor geometry (placed at 0,0).
    let g = &w["geometry"];
    assert_eq!(
        root["bounds"],
        json!({"x": g["x"], "y": g["y"], "width": 400, "height": 300}),
        "fused root bounds must equal compositor geometry: {root}"
    );
    assert!(root["bus_name"].as_str().unwrap().starts_with(':'));
    assert!(root["path"].as_str().unwrap().starts_with('/'));
    assert!(
        root["states"]
            .as_array()
            .unwrap()
            .contains(&json!("enabled"))
    );

    let children = root["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    let button = &children[0];
    assert_eq!(button["ref"], "e2");
    assert_eq!(button["role"], "button");
    assert_eq!(button["name"], "OK");
    assert_eq!(
        button["bounds"],
        json!({
            "x": g["x"].as_i64().unwrap() + 10,
            "y": g["y"].as_i64().unwrap() + 20,
            "width": 80,
            "height": 30
        }),
        "fused child bounds: {button}"
    );
}

/// Two distinct-PID apps with the same `app_id` each fuse with their own
/// tree — one served in-process (this test binary's pid) and one by a
/// spawned helper process (its own pid).
#[test]
fn get_tree_correlates_distinct_pids_same_app_id() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "twopids", json!({}));
    let mut client = TestClient::connect(&socket);
    let window_a = map_window(
        &mut rpc,
        "twopids",
        &mut client,
        "org.scry.Same",
        "Proc A",
        300,
        200,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &app_tree(&[("Proc A", "from-test")]));

    // A second app instance: same app_id, different title and pid.
    let ready = daemon.state_dir.join("helper-ready");
    let helper_pid = spawn_helper(
        &mut rpc,
        "twopids",
        ready,
        json!({
            "app_id": "org.scry.Same",
            "title": "Proc B",
            "width": 300,
            "height": 200,
            "ready": daemon.state_dir.join("helper-ready"),
            "tree": app_tree(&[("Proc B", "from-helper")]),
        }),
    );
    let window_b = wait_for_window(&mut rpc, "twopids", |w| {
        w["title"].as_str() == Some("Proc B")
    });

    let windows = tree_windows(&mut rpc, "twopids");
    let a = fused_window(&windows, window_a["id"].as_u64().unwrap());
    let b = fused_window(&windows, window_b["id"].as_u64().unwrap());

    assert_eq!(a["a11y_status"], "ok", "window A: {a}");
    assert_eq!(b["a11y_status"], "ok", "window B: {b}");
    assert_eq!(a["a11y"]["pid"], std::process::id());
    assert_eq!(b["a11y"]["pid"], helper_pid);

    // Each window fused with its own application's tree — the marker
    // child names prove no cross-talk.
    let a_marker = &a["root"]["children"][0]["name"];
    let b_marker = &b["root"]["children"][0]["name"];
    assert_eq!(a_marker, "from-test");
    assert_eq!(b_marker, "from-helper");
    assert_eq!(a["root"]["name"], "Proc A");
    assert_eq!(b["root"]["name"], "Proc B");
    // The bus names of the two fused roots differ (distinct peers).
    assert_ne!(a["root"]["bus_name"], b["root"]["bus_name"]);
}

/// When one process owns several toplevels, each window fuses with the
/// toplevel-role child whose accessible name matches its title — the
/// deterministic secondary match.
#[test]
fn get_tree_matches_same_pid_windows_by_frame_title() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "multiwin", json!({}));
    let mut client = TestClient::connect(&socket);
    let one = map_window(
        &mut rpc,
        "multiwin",
        &mut client,
        "org.scry.Multi",
        "Win One",
        200,
        100,
    );
    let two = map_window(
        &mut rpc,
        "multiwin",
        &mut client,
        "org.scry.Multi",
        "Win Two",
        200,
        100,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(
        a11y_addr,
        &app_tree(&[("Win One", "one-button"), ("Win Two", "two-button")]),
    );

    let windows = tree_windows(&mut rpc, "multiwin");
    let w1 = fused_window(&windows, one["id"].as_u64().unwrap());
    let w2 = fused_window(&windows, two["id"].as_u64().unwrap());

    assert_eq!(w1["a11y_status"], "ok", "window one: {w1}");
    assert_eq!(w2["a11y_status"], "ok", "window two: {w2}");
    assert_eq!(w1["root"]["name"], "Win One");
    assert_eq!(w1["root"]["children"][0]["name"], "one-button");
    assert_eq!(w2["root"]["name"], "Win Two");
    assert_eq!(w2["root"]["children"][0]["name"], "two-button");
}

/// After `place_window`, the same toolkit extents fuse onto the window's
/// new compositor geometry — every node's bounds shift by the placement
/// delta.
#[test]
fn get_tree_bounds_track_place_window() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "placed", json!({}));
    let mut client = TestClient::connect(&socket);
    let window = map_window(
        &mut rpc,
        "placed",
        &mut client,
        "org.scry.Placed",
        "Moved",
        400,
        300,
    );
    let window_id = window["id"].as_u64().unwrap();

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &app_tree(&[("Moved", "OK")]));

    rpc.call(
        "place_window",
        json!({"workspace": "placed", "window": window_id, "x": 120, "y": 60}),
    );
    // The placed geometry must be what the tree fuses onto — wait for the
    // placement to land in list_windows before snapshotting.
    wait_for_window(&mut rpc, "placed", |w| {
        w["geometry"]["x"] == 120 && w["geometry"]["y"] == 60
    });

    let windows = tree_windows(&mut rpc, "placed");
    let w = fused_window(&windows, window_id);
    assert_eq!(w["a11y_status"], "ok");
    assert_eq!(
        w["root"]["bounds"],
        json!({"x": 120, "y": 60, "width": 400, "height": 300}),
        "fused root bounds after place_window: {}",
        w["root"]
    );
    assert_eq!(
        w["root"]["children"][0]["bounds"],
        json!({"x": 130, "y": 80, "width": 80, "height": 30}),
        "fused child bounds after place_window"
    );
}

/// `window_id` scoping returns only that window's fused entry; an unknown
/// id errors with `UNKNOWN_WINDOW`.
#[test]
fn get_tree_window_scoping() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "scoped", json!({}));
    let mut client = TestClient::connect(&socket);
    let one = map_window(
        &mut rpc,
        "scoped",
        &mut client,
        "org.scry.One",
        "One",
        100,
        100,
    );
    map_window(
        &mut rpc,
        "scoped",
        &mut client,
        "org.scry.Two",
        "Two",
        100,
        100,
    );
    let one_id = one["id"].as_u64().unwrap();

    let tree = rpc.call(
        "get_tree",
        json!({"workspace": "scoped", "window_id": one_id}),
    );
    let windows = tree["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["window_id"].as_u64().unwrap(), one_id);
    assert_eq!(windows[0]["title"], "One");

    let err = rpc.call_err(
        "get_tree",
        json!({"workspace": "scoped", "window_id": 9999}),
    );
    assert_eq!(err.code, -32004, "expected UNKNOWN_WINDOW, got {err:?}");
}

/// `get_tree` is a read-only verb: `readOnly` workspaces allow it.
#[test]
fn get_tree_allowed_on_readonly_workspace() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "viewer", json!({"read_only": true}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "viewer",
        &mut client,
        "org.scry.View",
        "Read",
        100,
        100,
    );

    let tree = rpc.call("get_tree", json!({"workspace": "viewer"}));
    assert_eq!(tree["windows"].as_array().unwrap().len(), 1);
    assert!(tree["snapshot_id"].as_u64().unwrap() >= 1);
}

/// An app that registers but exposes an empty root reports `empty`
/// instead of `ok`.
#[test]
fn get_tree_reports_empty_root() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "empty", json!({}));
    let mut client = TestClient::connect(&socket);
    let window = map_window(
        &mut rpc,
        "empty",
        &mut client,
        "org.scry.Empty",
        "Blank",
        100,
        100,
    );
    let window_id = window["id"].as_u64().unwrap();

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &FakeNode::new("fake-app", "application"));

    let windows = tree_windows(&mut rpc, "empty");
    let w = fused_window(&windows, window_id);
    assert_eq!(w["a11y_status"], "empty", "window entry: {w}");
    assert!(w["hint"].as_str().unwrap().contains("capture_window"));
}

/// `scry tree` renders the fused snapshot in compact human form and in
/// `--json`.
#[test]
fn cli_tree_output() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    let (ws, wl_socket) = create_workspace(&mut rpc, "cli-tree", json!({}));
    let mut client = TestClient::connect(&wl_socket);
    let window = map_window(
        &mut rpc,
        "cli-tree",
        &mut client,
        "org.scry.Cli",
        "Cli Win",
        400,
        300,
    );
    let window_id = window["id"].as_u64().unwrap();

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &app_tree(&[("Cli Win", "OK")]));

    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_scry"))
            .arg("--socket")
            .arg(socket)
            .args(args)
            .output()
            .expect("failed to run scry")
    };

    // Human mode: refs, roles, names, bounds, indented tree.
    let out = run(&["tree", "-w", "cli-tree"]);
    assert!(
        out.status.success(),
        "scry tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("snapshot"), "human output: {text}");
    assert!(
        text.contains(&window_id.to_string()),
        "human output: {text}"
    );
    assert!(text.contains("[e1]"), "human output: {text}");
    assert!(text.contains("frame"), "human output: {text}");
    assert!(text.contains("\"Cli Win\""), "human output: {text}");
    assert!(text.contains("button"), "human output: {text}");
    assert!(text.contains("\"OK\""), "human output: {text}");

    // JSON mode: the raw get_tree payload.
    let out = run(&["--json", "tree", "-w", "cli-tree"]);
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(result["snapshot_id"].as_u64().unwrap() >= 1);
    let w = fused_window(result["windows"].as_array().unwrap(), window_id);
    assert_eq!(w["a11y_status"], "ok");
    assert_eq!(w["root"]["ref"], "e1");
}

/// `scry tree` reports fallback status for windows without a tree, in
/// both output modes.
#[test]
fn cli_tree_unavailable_window() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    let (_ws, wl_socket) = create_workspace(&mut rpc, "cli-no-tree", json!({}));
    let mut client = TestClient::connect(&wl_socket);
    map_window(
        &mut rpc,
        "cli-no-tree",
        &mut client,
        "org.scry.None",
        "Bare",
        100,
        100,
    );

    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .args(["tree", "-w", "cli-no-tree"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("unavailable"), "human output: {text}");
    assert!(text.contains("capture_window"), "human output: {text}");
}
