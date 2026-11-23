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

/// Regression (real GTK4, M3.7): a peer whose `CoordType::Screen`
/// extents are degenerate — `(0, 0, w, h)` for every node, exactly what
/// GTK4 serves under Wayland where clients cannot know their compositor
/// position — must still fuse correctly. The daemon reads
/// `CoordType::Window` extents and anchors them on the window's
/// compositor geometry; a `click_node` then lands on the widget's real
/// position rather than the window's top-left corner.
#[test]
fn get_tree_fuses_degenerate_screen_extents_peer() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "winrel", json!({}));
    let mut client = TestClient::connect(&socket);
    let window = map_window(
        &mut rpc,
        "winrel",
        &mut client,
        "org.scry.WinRel",
        "WinRel",
        400,
        300,
    );
    let window_id = window["id"].as_u64().unwrap();

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree(&[("WinRel", "OK")]));
    // GTK4 mode: frame + button lose their position under
    // CoordType::Screen but keep it under Window/Parent.
    peer.degenerate_screen("/fake/a11y/app/0");
    peer.degenerate_screen("/fake/a11y/app/0/0");

    rpc.call(
        "place_window",
        json!({"workspace": "winrel", "window": window_id, "x": 120, "y": 60}),
    );
    wait_for_window(&mut rpc, "winrel", |w| {
        w["geometry"]["x"] == 120 && w["geometry"]["y"] == 60
    });

    let windows = tree_windows(&mut rpc, "winrel");
    let w = fused_window(&windows, window_id);
    assert_eq!(w["a11y_status"], "ok", "window entry: {w}");
    assert_eq!(
        w["root"]["bounds"],
        json!({"x": 120, "y": 60, "width": 400, "height": 300}),
        "fused root bounds: {}",
        w["root"]
    );
    // The button's real position (10,20 inside the frame) fuses onto
    // the placed geometry — under CoordType::Screen it would collapse
    // to the window's own origin.
    assert_eq!(
        w["root"]["children"][0]["bounds"],
        json!({"x": 130, "y": 80, "width": 80, "height": 30}),
        "fused child bounds: {}",
        w["root"]["children"][0]
    );

    // click_node computes the fused center from the same live extents:
    // the pointer must land on the button's real (50,35) window-local
    // center, not on a degenerate (40,15).
    rpc.call("click_node", json!({"workspace": "winrel", "ref": "e2"}));
    const BTN_LEFT: u32 = 0x110;
    client.pump_until(|s| {
        s.pointer_buttons
            .iter()
            .any(|&(b, st)| b == BTN_LEFT && st == 0)
    });
    let position = client
        .state
        .pointer_enters
        .iter()
        .map(|&(_, x, y)| (x, y))
        .chain(client.state.pointer_motions.iter().copied())
        .last()
        .expect("no pointer position event delivered");
    assert!(
        (position.0 - 50.0).abs() < 1.0 && (position.1 - 35.0).abs() < 1.0,
        "pointer landed at {position:?}, expected ~(50,35)"
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

/// `max_depth` stops descending branches past the cap: nodes at the cap
/// that still had children carry a structured `truncated` marker whose
/// note is the inline `- ... depth limit reached` line, and the
/// response-level `truncation` block reports `depth` as tripped.
#[test]
fn get_tree_max_depth_marks_truncated_branches() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "deep", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "deep",
        &mut client,
        "org.scry.Deep",
        "Deep Win",
        400,
        300,
    );

    // A 4-level tree: frame → panel → box → button.
    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Deep Win", "frame")
            .bounds([0, 0, 400, 300])
            .child(
                FakeNode::new("panel", "panel")
                    .child(FakeNode::new("box", "filler").child(FakeNode::new("OK", "button"))),
            ),
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    let tree = rpc.call("get_tree", json!({"workspace": "deep", "max_depth": 2}));
    assert_eq!(tree["limits"]["max_depth"], 2);
    assert!(
        tree["truncation"]["tripped"]
            .as_array()
            .unwrap()
            .contains(&json!("depth")),
        "truncation summary: {}",
        tree["truncation"]
    );
    assert!(tree["truncation"]["markers"].as_u64().unwrap() >= 1);

    let root = &tree["windows"][0]["root"];
    assert_eq!(root["name"], "Deep Win");
    // Depth 1 = frame, depth 2 = panel: the panel had children but could
    // not be descended.
    let panel = &root["children"][0];
    assert_eq!(panel["name"], "panel");
    assert_eq!(panel["truncated"]["reason"], "depth");
    assert_eq!(panel["truncated"]["note"], "- ... depth limit reached");
    assert!(
        panel["children"].is_null() || panel["children"].as_array().unwrap().is_empty(),
        "truncated node must not emit children: {panel}"
    );
}

/// `scry tree` renders inline `- ... <reason>` markers for truncated
/// branches.
#[test]
fn cli_tree_depth_limit_marker() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    let (ws, wl_socket) = create_workspace(&mut rpc, "cli-depth", json!({}));
    let mut client = TestClient::connect(&wl_socket);
    map_window(
        &mut rpc,
        "cli-depth",
        &mut client,
        "org.scry.CliDepth",
        "Depth Win",
        400,
        300,
    );

    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Depth Win", "frame")
            .bounds([0, 0, 400, 300])
            .child(FakeNode::new("panel", "panel").child(FakeNode::new("OK", "button"))),
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .args(["tree", "-w", "cli-depth", "--max-depth", "1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "scry tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("- ... depth limit reached"),
        "expected inline depth marker: {text}"
    );
    assert!(!text.contains("OK"), "cut children must not render: {text}");
}

/// `max_nodes` bounds total emitted nodes capture-wide: the walk stops
/// at the budget, every open branch carrying unread children gets a
/// `- ... node limit reached` marker, and `nodes_emitted` reports the
/// count actually emitted.
#[test]
fn get_tree_max_nodes_marks_and_counts() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "wide", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "wide",
        &mut client,
        "org.scry.Wide",
        "Wide Win",
        400,
        300,
    );

    // frame → 4 panel children, each with a button leaf (10 nodes total
    // below the application root).
    let mut frame = FakeNode::new("Wide Win", "frame").bounds([0, 0, 400, 300]);
    for i in 0..4 {
        frame = frame.child(
            FakeNode::new(&format!("panel {i}"), "panel")
                .child(FakeNode::new(&format!("btn {i}"), "button")),
        );
    }
    let tree = FakeNode::new("fake-app", "application").child(frame);
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    // Emit exactly the frame + one panel (2 nodes): the panel's button
    // child and the frame's remaining panels are all cut.
    let tree = rpc.call("get_tree", json!({"workspace": "wide", "max_nodes": 2}));
    assert_eq!(tree["limits"]["max_nodes"], 2);
    let truncation = &tree["truncation"];
    assert!(
        truncation["tripped"]
            .as_array()
            .unwrap()
            .contains(&json!("nodes"))
    );
    assert_eq!(truncation["nodes_emitted"], 2);
    assert_eq!(truncation["markers"], 2);

    // The root emitted one panel child then stopped; every open branch
    // carrying unread children carries the inline note.
    let root = &tree["windows"][0]["root"];
    let children = root["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "root children: {children:?}");
    assert_eq!(root["truncated"]["reason"], "nodes");
    assert_eq!(root["truncated"]["note"], "- ... node limit reached");
    // The panel was read before the budget ran out, but its button child
    // was never emitted.
    assert_eq!(children[0]["name"], "panel 0");
    assert_eq!(children[0]["truncated"]["reason"], "nodes");
    assert!(
        children[0]["children"].is_null() || children[0]["children"].as_array().unwrap().is_empty(),
        "cut node's children must be absent: {}",
        children[0]
    );

    // Invalid bound values are rejected.
    let err = rpc.call_err("get_tree", json!({"workspace": "wide", "max_nodes": 0}));
    assert_eq!(err.code, -32602, "expected INVALID_PARAMS, got {err:?}");
    let err = rpc.call_err("get_tree", json!({"workspace": "wide", "max_depth": 0}));
    assert_eq!(err.code, -32602, "expected INVALID_PARAMS, got {err:?}");
}

/// `deadline_ms` turns a deliberately slow peer's unfinished branches
/// into `- ... deadline exceeded` markers instead of an error, and the
/// daemon stays responsive afterwards.
#[test]
fn get_tree_deadline_marks_partial_tree() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "slow", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "slow",
        &mut client,
        "org.scry.Slow",
        "Slow Win",
        400,
        300,
    );

    // Every `GetChildren` sleeps 150ms: with a 400ms deadline the walk
    // gets a couple of nodes deep, then unwinds with deadline markers.
    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Slow Win", "frame")
            .bounds([0, 0, 400, 300])
            .delay_ms(150)
            .child(
                FakeNode::new("a", "panel").delay_ms(150).child(
                    FakeNode::new("b", "panel")
                        .delay_ms(150)
                        .child(FakeNode::new("c", "panel")),
                ),
            ),
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    let start = Instant::now();
    let tree = rpc.call("get_tree", json!({"workspace": "slow", "deadline_ms": 400}));
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "get_tree took {elapsed:?} — the deadline should bound it"
    );
    assert_eq!(tree["limits"]["deadline_ms"], 400);
    assert!(
        tree["truncation"]["tripped"]
            .as_array()
            .unwrap()
            .contains(&json!("deadline")),
        "truncation summary: {}",
        tree["truncation"]
    );
    assert!(tree["truncation"]["markers"].as_u64().unwrap() >= 1);

    // The deepest emitted node carries the inline deadline marker; the
    // never-reached "c" node is absent entirely.
    let rendered = serde_json::to_string(&tree["windows"]).unwrap();
    assert!(
        rendered.contains("- ... deadline exceeded"),
        "windows: {rendered}"
    );
    assert!(!rendered.contains("\"c\""), "windows: {rendered}");

    // The daemon is not wedged: a fresh request answers promptly.
    let listed = rpc.call("list_windows", json!({"workspace": "slow"}));
    assert_eq!(listed["windows"].as_array().unwrap().len(), 1);
}

/// `root_ref` scopes a capture to one previously snapshotted node: the
/// response holds only that node's window, the node becomes the new
/// subtree root (`e1`), refs renumber deterministically, and fused
/// coordinates match the original snapshot's absolute positions.
#[test]
fn get_tree_root_ref_drills_into_subtree() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "drill", json!({}));
    let mut client = TestClient::connect(&socket);
    let window = map_window(
        &mut rpc,
        "drill",
        &mut client,
        "org.scry.Drill",
        "Drill Win",
        400,
        300,
    );
    let window_id = window["id"].as_u64().unwrap();

    // frame → panel (bounded) → [btn-a, btn-b]; e2 is the panel.
    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Drill Win", "frame")
            .bounds([0, 0, 400, 300])
            .child(
                FakeNode::new("panel", "panel")
                    .bounds([40, 50, 200, 100])
                    .child(FakeNode::new("btn-a", "button").bounds([10, 20, 60, 20]))
                    .child(FakeNode::new("btn-b", "button").bounds([10, 50, 60, 20])),
            ),
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    let full = rpc.call("get_tree", json!({"workspace": "drill"}));
    let panel = &full["windows"][0]["root"]["children"][0];
    assert_eq!(panel["ref"], "e2", "full tree: {full}");
    let panel_bounds = panel["bounds"].clone();
    let btn_a_bounds = panel["children"][0]["bounds"].clone();

    let scoped = rpc.call("get_tree", json!({"workspace": "drill", "root_ref": "e2"}));
    assert_eq!(scoped["root_ref"], "e2");
    let windows = scoped["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["window_id"].as_u64().unwrap(), window_id);
    assert_eq!(windows[0]["a11y_status"], "ok");

    let root = &windows[0]["root"];
    assert_eq!(root["ref"], "e1");
    assert_eq!(root["name"], "panel");
    assert_eq!(root["role"], "panel");
    assert_eq!(
        root["bounds"], panel_bounds,
        "subtree root must anchor on its snapshotted bounds"
    );
    let children = root["children"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0]["ref"], "e2");
    assert_eq!(children[0]["name"], "btn-a");
    assert_eq!(
        children[0]["bounds"], btn_a_bounds,
        "descendants keep absolute coordinates"
    );

    // The scoped result is itself a snapshot whose refs resolve.
    assert!(scoped["snapshot_id"].as_u64().unwrap() > full["snapshot_id"].as_u64().unwrap());

    // Unknown ref → INVALID_PARAMS with a re-snapshot hint (same
    // contract as click_node/node_action).
    let err = rpc.call_err("get_tree", json!({"workspace": "drill", "root_ref": "e99"}));
    assert_eq!(err.code, -32602, "expected INVALID_PARAMS, got {err:?}");
    assert!(err.message.contains("e99"));

    // A ref requested before any snapshot exists fails the same way.
    let fresh = create_workspace(&mut rpc, "drill-fresh", json!({})).0;
    let _ = fresh;
    let err = rpc.call_err(
        "get_tree",
        json!({"workspace": "drill-fresh", "root_ref": "e1"}),
    );
    assert_eq!(err.code, -32602, "expected INVALID_PARAMS, got {err:?}");
}

/// A `root_ref` whose node changed since its snapshot — the peer went
/// away — fails with `STALE_REF` rather than walking blindly.
#[test]
fn get_tree_root_ref_fails_when_node_is_gone() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "stale", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "stale",
        &mut client,
        "org.scry.Stale",
        "Stale Win",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree(&[("Stale Win", "OK")]));
    let full = rpc.call("get_tree", json!({"workspace": "stale"}));
    assert_eq!(full["windows"][0]["a11y_status"], "ok");

    // The peer exits: the ref's bus name no longer resolves.
    drop(peer);

    let err = rpc.call_err("get_tree", json!({"workspace": "stale", "root_ref": "e2"}));
    assert_eq!(err.code, -32010, "expected STALE_REF, got {err:?}");
}

/// `scry tree --root-ref` drills into a subtree captured by a prior
/// snapshot.
#[test]
fn cli_tree_root_ref() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    let (ws, wl_socket) = create_workspace(&mut rpc, "cli-ref", json!({}));
    let mut client = TestClient::connect(&wl_socket);
    map_window(
        &mut rpc,
        "cli-ref",
        &mut client,
        "org.scry.CliRef",
        "Ref Win",
        400,
        300,
    );

    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Ref Win", "frame")
            .bounds([0, 0, 400, 300])
            .child(
                FakeNode::new("panel", "panel")
                    .child(FakeNode::new("btn-a", "button"))
                    .child(FakeNode::new("btn-b", "button")),
            ),
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_scry"))
            .arg("--socket")
            .arg(socket)
            .args(args)
            .output()
            .expect("failed to run scry")
    };

    // First snapshot establishes the refs, then --root-ref drills in.
    assert!(run(&["tree", "-w", "cli-ref"]).status.success());
    let out = run(&["tree", "-w", "cli-ref", "--root-ref", "e2"]);
    assert!(
        out.status.success(),
        "scry tree --root-ref failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("panel"), "scoped output: {text}");
    assert!(text.contains("btn-a"), "scoped output: {text}");

    // A stale ref surfaces the structured error on stderr.
    let out = run(&["tree", "-w", "cli-ref", "--root-ref", "e99"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("e99"), "stderr: {stderr}");
}

/// `Text`/`Value` interfaces surface their contents on emitted nodes —
/// and a `ROLE_PASSWORD_TEXT` node never exposes its secret: text, value
/// and value_text are all replaced by the `[REDACTED]` sentinel, in JSON
/// and in `scry tree` output.
#[test]
fn get_tree_extracts_text_and_value_and_redacts_passwords() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "secrets", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "secrets",
        &mut client,
        "org.scry.Secrets",
        "Secret Win",
        400,
        300,
    );

    let secret = "hunter2-correct-horse";
    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Secret Win", "frame")
            .bounds([0, 0, 400, 300])
            .child(FakeNode::new("user", "entry").text("sid@example.com"))
            .child(
                FakeNode::new("volume", "slider")
                    .value(0.42)
                    .value_text("42%"),
            )
            .child(
                FakeNode::new("Password", "password text")
                    .text(secret)
                    .value(21.0)
                    .value_text(secret),
            ),
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    let tree = rpc.call("get_tree", json!({"workspace": "secrets"}));
    let rendered = serde_json::to_string(&tree).unwrap();
    assert!(
        !rendered.contains(secret),
        "the secret must never appear in the snapshot: {rendered}"
    );

    let children = tree["windows"][0]["root"]["children"].as_array().unwrap();
    let entry = &children[0];
    assert_eq!(entry["role"], "entry");
    assert_eq!(entry["text"], "sid@example.com");
    assert!(entry["redacted"].is_null() || entry["redacted"] == false);

    let slider = &children[1];
    assert_eq!(slider["value"], 0.42);
    assert_eq!(slider["value_text"], "42%");

    let password = &children[2];
    assert_eq!(password["role"], "password text");
    assert_eq!(password["name"], "Password");
    assert_eq!(password["redacted"], true);
    assert_eq!(password["text"], "[REDACTED]");
    assert_eq!(password["value"], "[REDACTED]");
    assert_eq!(password["value_text"], "[REDACTED]");

    // `scry tree` shows the sentinel too, and never the secret.
    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(&daemon.socket_path)
        .args(["tree", "-w", "secrets"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains(secret), "cli leaked secret: {text}");
    assert!(text.contains("[REDACTED]"), "cli output: {text}");
    assert!(text.contains("sid@example.com"), "cli output: {text}");
}

/// Death of a `client_pid` (or the `SO_PEERCRED` peer) aborts an
/// in-flight capture with `CANCELLED` — a slow peer can't pin a request
/// whose caller is gone.
#[test]
fn get_tree_cancelled_when_client_pid_exits() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "cancel", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "cancel",
        &mut client,
        "org.scry.Cancel",
        "Cancel Win",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Cancel Win", "frame")
            .bounds([0, 0, 400, 300])
            .delay_ms(500)
            .child(FakeNode::new("a", "panel").delay_ms(500)),
    );
    let _peer = a11y::serve(a11y_addr, &tree);

    // A caller-tagged process whose death cancels the capture.
    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("failed to spawn sleep");
    rpc.send_request(
        "get_tree",
        json!({"workspace": "cancel", "client_pid": child.id()}),
    );
    std::thread::sleep(Duration::from_millis(100));
    child.kill().unwrap();
    child.wait().unwrap();

    let frame = rpc.read_frame();
    let error = &frame["error"];
    assert_eq!(error["code"], -32007, "expected CANCELLED, got {frame}");
    assert!(
        error["message"].as_str().unwrap().contains("client_pid"),
        "message: {error}"
    );

    // The daemon stays healthy and answers promptly.
    let listed = rpc.call("list_windows", json!({"workspace": "cancel"}));
    assert_eq!(listed["windows"].as_array().unwrap().len(), 1);
}

/// Dropping the control socket mid-capture aborts traversal: the daemon
/// keeps answering other connections promptly.
#[test]
fn get_tree_cancelled_on_disconnect() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "disc", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "disc",
        &mut client,
        "org.scry.Disc",
        "Disc Win",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Disc Win", "frame")
            .bounds([0, 0, 400, 300])
            .delay_ms(500)
            .child(FakeNode::new("a", "panel").delay_ms(500)),
    );
    let _peer = a11y::serve(a11y_addr, &tree);

    {
        let mut gone = RpcClient::connect(&daemon.socket_path);
        gone.send_request("get_tree", json!({"workspace": "disc"}));
        std::thread::sleep(Duration::from_millis(100));
        drop(gone); // full disconnect cancels the in-flight capture
    }

    // The capture's dead caller never wedges the daemon: this connection
    // gets a fresh, complete answer right away.
    let start = Instant::now();
    let listed = rpc.call("list_windows", json!({"workspace": "disc"}));
    assert_eq!(listed["windows"].as_array().unwrap().len(), 1);
    let tree = rpc.call(
        "get_tree",
        json!({"workspace": "disc", "deadline_ms": 5000}),
    );
    assert_eq!(tree["windows"][0]["a11y_status"], "ok", "tree: {tree}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "daemon appears wedged after disconnect"
    );
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
