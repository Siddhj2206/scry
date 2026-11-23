//! Integration tests for `click_node` and `node_action` (M3.4):
//! staleness-checked `eN` refs, fused-coordinate pointer clicks through
//! the compositor's targeted input pipeline, AT-SPI2 `Action::DoAction`
//! invocation, and `readOnly` gating.
//!
//! The seam under test is the daemon's real Unix-socket JSON-RPC 2.0
//! protocol, paired with real `wayland-client` pointer events and a real
//! `zbus` AT-SPI2 peer on the workspace's private a11y bus — never
//! daemon internals.

mod common;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use common::{
    TestClient,
    a11y::{self, FakeNode},
    daemon::{RPC_TIMEOUT, RpcClient, spawn_daemon},
};
use serde_json::{Value, json};

/// The `STALE_REF` application error code (-32010).
const STALE_REF: i64 = -32010;
/// The `READ_ONLY` application error code (-32003).
const READ_ONLY: i64 = -32003;
/// `BTN_LEFT` on the evdev wire.
const BTN_LEFT: u32 = 0x110;

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
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        let listed = rpc.call("list_windows", json!({"workspace": workspace}));
        if let Some(window) = listed["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["title"].as_str() == Some(title))
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

/// A fake app with one `frame` per `titles` entry; each frame carries one
/// `push button` child named `button_name` with toolkit-relative bounds
/// and optional action names.
fn app_tree(title: &str, button_name: &str, actions: &[&str]) -> FakeNode {
    FakeNode::new("fake-app", "application").child(
        FakeNode::new(title, "frame")
            .bounds([0, 0, 400, 300])
            .child(
                FakeNode::new(button_name, "button")
                    .bounds([10, 20, 80, 30])
                    .actions(actions),
            ),
    )
}

/// The first `get_tree` node matching `pred` in `tree`, searched
/// depth-first across every fused window root.
fn find_node(tree: &Value, pred: impl Fn(&Value) -> bool) -> Option<&Value> {
    fn walk<'b>(node: &'b Value, pred: &dyn Fn(&Value) -> bool) -> Option<&'b Value> {
        if pred(node) {
            return Some(node);
        }
        node["children"]
            .as_array()?
            .iter()
            .find_map(|child| walk(child, pred))
    }
    tree["windows"]
        .as_array()?
        .iter()
        .filter_map(|window| window.get("root"))
        .find_map(|root| walk(root, &pred))
}

/// A `get_tree` snapshot for `workspace`.
fn snapshot(rpc: &mut RpcClient, workspace: &str) -> Value {
    rpc.call("get_tree", json!({"workspace": workspace}))
}

/// `click_node` validates the ref and delivers a genuine Wayland pointer
/// enter/motion/button press/release at the node's fused screen center,
/// returning `command_id` and the workspace `seq`.
#[test]
fn click_node_delivers_real_pointer_events() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "clicker", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "clicker",
        &mut client,
        "org.scry.Clicker",
        "Clickable",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &app_tree("Clickable", "OK", &["click"]));

    let tree = snapshot(&mut rpc, "clicker");
    let button =
        find_node(&tree, |n| n["role"] == "button").expect("no push button in the fused tree");
    let button_ref = button["ref"].as_str().unwrap().to_string();

    let result = rpc.call(
        "click_node",
        json!({"workspace": "clicker", "ref": button_ref}),
    );
    assert!(result["command_id"].as_u64().is_some(), "{result}");
    assert!(result["seq"].as_u64().is_some(), "{result}");
    assert_eq!(result["ref"], button_ref);
    assert_eq!(result["clicked"], 1);
    assert_eq!(result["button"], "left");

    // The button sits at toolkit extents (10,20,80,30) inside a frame at
    // (0,0): its fused center is window-local (50,35). A real press and
    // release of BTN_LEFT (272) must arrive at exactly that point.
    client.pump_until(|s| {
        s.pointer_buttons
            .iter()
            .any(|&(b, st)| b == BTN_LEFT && st == 1)
            && s.pointer_buttons
                .iter()
                .any(|&(b, st)| b == BTN_LEFT && st == 0)
    });
    // The pointer's reported position (enter coordinates — the position
    // event a focus change delivers) lands on the fused center.
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

/// `node_action` invokes the named `org.a11y.atspi.Action` on the ref's
/// node — the fake peer records the `DoAction` it receives.
#[test]
fn node_action_invokes_named_action() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "actor", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "actor",
        &mut client,
        "org.scry.Actor",
        "Actions",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree("Actions", "OK", &["press", "release"]));

    let tree = snapshot(&mut rpc, "actor");
    let button = find_node(&tree, |n| n["role"] == "button").unwrap();
    let button_ref = button["ref"].as_str().unwrap();
    let button_path = button["path"].as_str().unwrap().to_string();

    let result = rpc.call(
        "node_action",
        json!({"workspace": "actor", "ref": button_ref, "action": "release"}),
    );
    assert!(result["command_id"].as_u64().is_some(), "{result}");
    assert!(result["seq"].is_number(), "{result}");
    assert_eq!(result["action"], "release");
    assert_eq!(result["action_index"], 1);
    assert_eq!(result["performed"], true);
    assert_eq!(peer.invoked_actions(&button_path), vec!["release"]);
}

/// With no `action` param, `node_action` invokes the node's primary
/// (first listed) action.
#[test]
fn node_action_defaults_to_primary_action() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "primary", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "primary",
        &mut client,
        "org.scry.Primary",
        "Primary",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree("Primary", "OK", &["press", "release"]));

    let tree = snapshot(&mut rpc, "primary");
    let button = find_node(&tree, |n| n["role"] == "button").unwrap();
    let button_ref = button["ref"].as_str().unwrap();
    let button_path = button["path"].as_str().unwrap().to_string();

    let result = rpc.call(
        "node_action",
        json!({"workspace": "primary", "ref": button_ref}),
    );
    assert_eq!(result["action"], "press", "{result}");
    assert_eq!(result["action_index"], 0);
    assert_eq!(peer.invoked_actions(&button_path), vec!["press"]);
}

/// A named action the node does not offer is a clear structured error
/// listing the available actions — not a `DoAction` call.
#[test]
fn node_action_rejects_unsupported_action() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "noact", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "noact",
        &mut client,
        "org.scry.NoAct",
        "No Actions",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree("No Actions", "OK", &["press"]));

    let tree = snapshot(&mut rpc, "noact");
    let button = find_node(&tree, |n| n["role"] == "button").unwrap();
    let button_ref = button["ref"].as_str().unwrap();
    let button_path = button["path"].as_str().unwrap().to_string();

    let err = rpc.call_err(
        "node_action",
        json!({"workspace": "noact", "ref": button_ref, "action": "explode"}),
    );
    assert_ne!(err.code, STALE_REF);
    assert!(
        err.message.contains("explode"),
        "error should name the missing action: {err:?}"
    );
    assert_eq!(
        err.data.as_ref().unwrap()["available"],
        json!(["press"]),
        "error data should list available actions: {err:?}"
    );
    assert!(peer.invoked_actions(&button_path).is_empty());

    // A node without an Action interface at all also gets a clear error.
    let frame = find_node(&tree, |n| n["role"] == "frame").unwrap();
    let frame_ref = frame["ref"].as_str().unwrap();
    let err = rpc.call_err(
        "node_action",
        json!({"workspace": "noact", "ref": frame_ref, "action": "press"}),
    );
    assert!(
        err.message.contains("Action") || err.message.contains("action"),
        "no-interface error should be descriptive: {err:?}"
    );
}

/// Renaming a node between `get_tree` and the action fails fast with
/// `STALE_REF` (-32010), structured before/after details and a
/// re-snapshot hint — on both verbs.
#[test]
fn stale_ref_on_name_change() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "renamed", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "renamed",
        &mut client,
        "org.scry.Renamed",
        "Renamed",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree("Renamed", "OK", &["click"]));

    let tree = snapshot(&mut rpc, "renamed");
    let button = find_node(&tree, |n| n["role"] == "button").unwrap();
    let button_ref = button["ref"].as_str().unwrap();
    let button_path = button["path"].as_str().unwrap().to_string();

    peer.set_name(&button_path, "Submit");

    for verb in ["click_node", "node_action"] {
        let err = rpc.call_err(verb, json!({"workspace": "renamed", "ref": button_ref}));
        assert_eq!(err.code, STALE_REF, "{verb} error: {err:?}");
        assert!(
            err.message.contains("get_tree"),
            "{verb} should advise re-running get_tree: {err:?}"
        );
        let data = err.data.as_ref().unwrap();
        assert_eq!(data["ref"], button_ref);
        assert_eq!(data["mismatch"], "name", "{verb} data: {data}");
        assert_eq!(data["expected"]["name"], "OK");
        assert_eq!(data["actual"]["name"], "Submit");
    }
    // Nothing was clicked or invoked.
    assert!(peer.invoked_actions(&button_path).is_empty());
}

/// A role change between `get_tree` and the action is equally stale.
#[test]
fn stale_ref_on_role_change() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "recast", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "recast",
        &mut client,
        "org.scry.Recast",
        "Recast",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree("Recast", "OK", &[]));

    let tree = snapshot(&mut rpc, "recast");
    let button = find_node(&tree, |n| n["role"] == "button").unwrap();
    let button_ref = button["ref"].as_str().unwrap();
    let button_path = button["path"].as_str().unwrap().to_string();

    peer.set_role(&button_path, "label");

    let err = rpc.call_err(
        "click_node",
        json!({"workspace": "recast", "ref": button_ref}),
    );
    assert_eq!(err.code, STALE_REF, "{err:?}");
    let data = err.data.as_ref().unwrap();
    assert_eq!(data["mismatch"], "role");
    assert_eq!(data["expected"]["role"], "button");
    assert_eq!(data["actual"]["role"], "label");

    let err = rpc.call_err(
        "node_action",
        json!({"workspace": "recast", "ref": button_ref}),
    );
    assert_eq!(err.code, STALE_REF, "{err:?}");
    assert_eq!(err.data.as_ref().unwrap()["mismatch"], "role");
}

/// A node that disappears between `get_tree` and the action is stale,
/// and a fresh `get_tree` no longer reports it.
#[test]
fn stale_ref_on_removed_node() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "gone", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "gone",
        &mut client,
        "org.scry.Gone",
        "Gone",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree("Gone", "OK", &["click"]));

    let tree = snapshot(&mut rpc, "gone");
    let button = find_node(&tree, |n| n["role"] == "button").unwrap();
    let button_ref = button["ref"].as_str().unwrap().to_string();
    let button_path = button["path"].as_str().unwrap().to_string();

    peer.remove_node(&button_path);

    for verb in ["click_node", "node_action"] {
        let err = rpc.call_err(verb, json!({"workspace": "gone", "ref": button_ref}));
        assert_eq!(err.code, STALE_REF, "{verb} error: {err:?}");
        assert_eq!(err.data.as_ref().unwrap()["mismatch"], "missing");
        assert!(err.message.contains("get_tree"), "{verb}: {err:?}");
    }

    // A fresh snapshot heals: the same ref resolves to a live node again
    // (refs are per-snapshot, so it may now name a different node — the
    // point is the workspace recovered).
    let tree = snapshot(&mut rpc, "gone");
    assert!(find_node(&tree, |n| n["name"] == "OK").is_none());
}

/// Refs that never existed, and snapshots that were never taken, are
/// caller errors — not `STALE_REF`.
#[test]
fn unknown_ref_and_missing_snapshot_are_param_errors() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "fresh", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "fresh",
        &mut client,
        "org.scry.Fresh",
        "Fresh",
        100,
        100,
    );

    // No snapshot at all yet.
    let err = rpc.call_err("click_node", json!({"workspace": "fresh", "ref": "e1"}));
    assert_ne!(err.code, STALE_REF);
    assert!(
        err.message.contains("get_tree"),
        "should point at get_tree: {err:?}"
    );

    // A snapshot exists but the ref is not in it.
    snapshot(&mut rpc, "fresh");
    for verb in ["click_node", "node_action"] {
        let err = rpc.call_err(verb, json!({"workspace": "fresh", "ref": "e99"}));
        assert_ne!(err.code, STALE_REF, "{verb}: {err:?}");
    }
}

/// Both verbs are mutating: `readOnly` workspaces reject them with
/// `READ_ONLY` (-32003) before any ref work happens.
#[test]
fn readonly_workspace_rejects_node_verbs() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "viewer", json!({"read_only": true}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "viewer",
        &mut client,
        "org.scry.View",
        "View",
        100,
        100,
    );

    for verb in ["click_node", "node_action"] {
        let err = rpc.call_err(verb, json!({"workspace": "viewer", "ref": "e1"}));
        assert_eq!(err.code, READ_ONLY, "{verb} error: {err:?}");
    }
}

/// `scry click-node` and `scry node-action` proxy the verbs: human
/// output names the ref and what happened; `--json` emits the raw
/// result; a stale ref prints the daemon's error on stderr with a
/// non-zero exit.
#[test]
fn cli_node_verbs() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    let (ws, wl_socket) = create_workspace(&mut rpc, "cli-nodes", json!({}));
    let mut client = TestClient::connect(&wl_socket);
    map_window(
        &mut rpc,
        "cli-nodes",
        &mut client,
        "org.scry.CliNodes",
        "Cli Nodes",
        400,
        300,
    );

    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &app_tree("Cli Nodes", "OK", &["press"]));

    let tree = snapshot(&mut rpc, "cli-nodes");
    let button = find_node(&tree, |n| n["role"] == "button").unwrap();
    let button_ref = button["ref"].as_str().unwrap().to_string();
    let button_path = button["path"].as_str().unwrap().to_string();

    let run = |args: &[&str]| -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_scry"))
            .arg("--socket")
            .arg(socket)
            .args(args)
            .output()
            .expect("failed to run scry")
    };

    // Human mode: click-node reports the ref, coordinates and count.
    let out = run(&["click-node", "-w", "cli-nodes", "--ref", &button_ref]);
    assert!(
        out.status.success(),
        "scry click-node failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&button_ref), "human output: {text}");
    assert!(text.contains("click"), "human output: {text}");

    // Human mode: node-action reports the performed action.
    let out = run(&[
        "node-action",
        "-w",
        "cli-nodes",
        "--ref",
        &button_ref,
        "--action",
        "press",
    ]);
    assert!(
        out.status.success(),
        "scry node-action failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("press"), "human output: {text}");
    assert_eq!(peer.invoked_actions(&button_path), vec!["press"]);

    // --json mode: the raw verb payloads.
    let out = run(&[
        "--json",
        "click-node",
        "-w",
        "cli-nodes",
        "--ref",
        &button_ref,
    ]);
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["ref"], button_ref);
    assert_eq!(result["clicked"], 1);
    let out = run(&[
        "--json",
        "node-action",
        "-w",
        "cli-nodes",
        "--ref",
        &button_ref,
    ]);
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["action"], "press");
    assert_eq!(result["performed"], true);

    // A stale ref: non-zero exit, stderr names the drift and advises
    // get_tree, in both modes.
    peer.set_name(&button_path, "Moved");
    let out = run(&["click-node", "-w", "cli-nodes", "--ref", &button_ref]);
    assert!(!out.status.success(), "stale click-node should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("-32010"), "stderr: {stderr}");
    assert!(stderr.contains("get_tree"), "stderr: {stderr}");

    let out = run(&[
        "--json",
        "node-action",
        "-w",
        "cli-nodes",
        "--ref",
        &button_ref,
    ]);
    assert!(!out.status.success());
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], -32010);
}
