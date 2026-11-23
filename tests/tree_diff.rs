//! Integration tests for `tree_diff` (M3.5): line-oriented LCS diffs
//! between a retained `from_snapshot` and a fresh bounded capture —
//! added/removed/changed hunks, default baselines, evicted-snapshot
//! resync hints, password redaction, `readOnly` access and the
//! `scry tree-diff` CLI.
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

/// A fake app tree: app → frame → panel → [label, entry, checkbox, OK,
/// Cancel], with object paths `/fake/a11y/app/...` assigned by the serve
/// helper's depth-first layout.
fn dialog_tree(title: &str) -> FakeNode {
    FakeNode::new("fake-app", "application").child(
        FakeNode::new(title, "frame")
            .bounds([0, 0, 400, 300])
            .child(
                FakeNode::new("form", "panel")
                    .bounds([10, 10, 380, 280])
                    .child(FakeNode::new("Username", "label").bounds([20, 20, 100, 20]))
                    .child(
                        FakeNode::new("user", "entry")
                            .bounds([20, 45, 200, 25])
                            .text("sid@example.com"),
                    )
                    .child(
                        FakeNode::new("Remember me", "check box")
                            .bounds([20, 80, 16, 16])
                            .states(&["checked", "enabled", "showing", "visible"]),
                    )
                    .child(FakeNode::new("OK", "button").bounds([20, 250, 80, 30]))
                    .child(FakeNode::new("Cancel", "button").bounds([110, 250, 80, 30])),
            ),
    )
}

/// The panel's object path inside `dialog_tree` (app/0/0).
const PANEL: &str = "/fake/a11y/app/0/0";
/// The OK button's object path.
const OK_BUTTON: &str = "/fake/a11y/app/0/0/3";
/// The Cancel button's object path.
const CANCEL_BUTTON: &str = "/fake/a11y/app/0/0/4";

/// Every `removed` line across a diff's hunks, concatenated.
fn removed_lines(diff: &Value) -> Vec<String> {
    diff["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|h| {
            h["removed"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Every `added` line across a diff's hunks, concatenated.
fn added_lines(diff: &Value) -> Vec<String> {
    diff["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|h| {
            h["added"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `tree_diff` emits `changed` hunks for renames, `removed` hunks for
/// vanished trailing nodes and `added` hunks for appended ones — with
/// the new `snapshot_id` and, when `from_snapshot` is omitted, the
/// workspace's immediately preceding snapshot as the baseline.
#[test]
fn tree_diff_reports_added_removed_and_changed() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "diff", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "diff",
        &mut client,
        "org.scry.Diff",
        "Diff Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &dialog_tree("Diff Win"));

    let baseline = rpc.call("get_tree", json!({"workspace": "diff"}));
    let from = baseline["snapshot_id"].as_u64().unwrap();

    // A rename: "OK" → "Apply" shows up as one `changed` hunk holding
    // the old and new canonical lines.
    peer.set_name(OK_BUTTON, "Apply");
    let diff = rpc.call(
        "tree_diff",
        json!({"workspace": "diff", "from_snapshot": from}),
    );
    assert_eq!(diff["from_snapshot"], from);
    let snap2 = diff["snapshot_id"].as_u64().unwrap();
    assert!(snap2 > from, "new snapshot_id must follow the baseline");
    assert_eq!(diff["changed"], true);
    let hunks = diff["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1, "one line changed, one hunk: {hunks:?}");
    assert_eq!(hunks[0]["kind"], "changed");
    let removed = removed_lines(&diff);
    let added = added_lines(&diff);
    assert_eq!(removed.len(), 1, "removed: {removed:?}");
    assert!(removed[0].contains("\"OK\""), "removed: {removed:?}");
    assert_eq!(added.len(), 1, "added: {added:?}");
    assert!(added[0].contains("\"Apply\""), "added: {added:?}");
    // Unchanged lines stay out of the diff — the whole point of the verb.
    assert!(
        !removed.iter().any(|l| l.contains("Username")),
        "unchanged lines must not appear: {removed:?}"
    );
    assert!(
        !added.iter().any(|l| l.contains("Username")),
        "unchanged lines must not appear: {added:?}"
    );

    // No from_snapshot: the immediately preceding snapshot (snap2) is
    // the baseline. Removing the trailing Cancel node is a `removed`
    // hunk.
    peer.remove_node(CANCEL_BUTTON);
    let diff = rpc.call("tree_diff", json!({"workspace": "diff"}));
    assert_eq!(diff["from_snapshot"], snap2);
    let hunks = diff["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1, "hunks: {hunks:?}");
    assert_eq!(hunks[0]["kind"], "removed");
    let removed = removed_lines(&diff);
    assert!(removed[0].contains("\"Cancel\""), "removed: {removed:?}");
    assert!(added_lines(&diff).is_empty());

    // Appending a node at the end is an `added` hunk.
    let snap3 = diff["snapshot_id"].as_u64().unwrap();
    peer.add_child(
        PANEL,
        FakeNode::new("Help", "button").bounds([200, 250, 80, 30]),
    );
    let diff = rpc.call("tree_diff", json!({"workspace": "diff"}));
    assert_eq!(diff["from_snapshot"], snap3);
    let hunks = diff["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1, "hunks: {hunks:?}");
    assert_eq!(hunks[0]["kind"], "added");
    let added = added_lines(&diff);
    assert!(added[0].contains("\"Help\""), "added: {added:?}");
    assert!(removed_lines(&diff).is_empty());

    // A pure state flip (focus moves) is a `changed` hunk — the diff
    // must catch state changes that leave role/name/bounds alone.
    peer.set_states(OK_BUTTON, &["enabled", "focused", "showing", "visible"]);
    let diff = rpc.call("tree_diff", json!({"workspace": "diff"}));
    let hunks = diff["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1, "state flip hunks: {hunks:?}");
    assert_eq!(hunks[0]["kind"], "changed");
    assert!(
        added_lines(&diff)[0].contains("focused"),
        "new line should show the flipped state: {diff}"
    );

    // Identical trees diff cleanly.
    let diff = rpc.call("tree_diff", json!({"workspace": "diff"}));
    assert_eq!(diff["changed"], false);
    assert!(diff["hunks"].as_array().unwrap().is_empty());
}

/// Referencing an unknown or evicted `from_snapshot` fails with a
/// structured resync error pointing at `get_tree`; omitting it when no
/// snapshot exists fails the same way.
#[test]
fn tree_diff_unknown_or_evicted_snapshot_resync_hint() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "evict", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "evict",
        &mut client,
        "org.scry.Evict",
        "Evict Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &dialog_tree("Evict Win"));

    // No snapshots at all: no baseline to diff against.
    let err = rpc.call_err("tree_diff", json!({"workspace": "evict"}));
    assert_eq!(err.code, -32011, "expected SNAPSHOT_EVICTED, got {err:?}");
    assert!(err.message.contains("get_tree"), "message: {}", err.message);

    let first = rpc.call("get_tree", json!({"workspace": "evict"}))["snapshot_id"]
        .as_u64()
        .unwrap();

    // An id that was never issued is unknown.
    let err = rpc.call_err(
        "tree_diff",
        json!({"workspace": "evict", "from_snapshot": 9999}),
    );
    assert_eq!(err.code, -32011, "expected SNAPSHOT_EVICTED, got {err:?}");
    assert!(err.message.contains("get_tree"), "message: {}", err.message);
    let data = err.data.expect("structured error data");
    assert!(
        data["hint"].as_str().unwrap().contains("get_tree"),
        "hint: {data}"
    );

    // Push the first snapshot out of the bounded history (capacity 8)
    // with more captures, then reference it: evicted.
    for _ in 0..8 {
        rpc.call("get_tree", json!({"workspace": "evict"}));
    }
    let err = rpc.call_err(
        "tree_diff",
        json!({"workspace": "evict", "from_snapshot": first}),
    );
    assert_eq!(err.code, -32011, "expected SNAPSHOT_EVICTED, got {err:?}");
    let data = err.data.expect("structured error data");
    assert_eq!(data["from_snapshot"], first);
    assert!(
        data["hint"].as_str().unwrap().contains("get_tree"),
        "hint: {data}"
    );
}

/// `tree_diff` preserves `ROLE_PASSWORD_TEXT` redaction: a password
/// value changing under the hood produces no leak and no false hunk —
/// both sides of any line carry only `[REDACTED]`.
#[test]
fn tree_diff_never_exposes_password_contents() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "pw-diff", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "pw-diff",
        &mut client,
        "org.scry.PwDiff",
        "Secret Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let secret = "hunter2-correct-horse";
    let tree = FakeNode::new("fake-app", "application").child(
        FakeNode::new("Secret Win", "frame")
            .bounds([0, 0, 400, 300])
            .child(
                FakeNode::new("Password", "password text")
                    .text(secret)
                    .value(21.0)
                    .value_text(secret),
            )
            .child(FakeNode::new("OK", "button")),
    );
    let peer = a11y::serve(a11y_addr, &tree);

    rpc.call("get_tree", json!({"workspace": "pw-diff"}));

    // Rename the password node and rotate the secret: the diff shows
    // the rename with [REDACTED] content, never either secret.
    let pw_path = "/fake/a11y/app/0/0";
    peer.set_name(pw_path, "Password!");
    peer.set_text(pw_path, "a-different-secret-entirely");

    let diff = rpc.call("tree_diff", json!({"workspace": "pw-diff"}));
    let rendered = serde_json::to_string(&diff).unwrap();
    assert!(
        !rendered.contains(secret),
        "diff leaked old secret: {rendered}"
    );
    assert!(
        !rendered.contains("a-different-secret-entirely"),
        "diff leaked new secret: {rendered}"
    );
    let hunks = diff["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1, "rename should be one hunk: {hunks:?}");
    let added = added_lines(&diff);
    assert!(added[0].contains("Password!"), "added: {added:?}");
    assert!(added[0].contains("[REDACTED]"), "added: {added:?}");
}

/// `tree_diff` is a read-only verb: `readOnly` workspaces allow it.
#[test]
fn tree_diff_allowed_on_readonly_workspace() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "ro-diff", json!({"read_only": true}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "ro-diff",
        &mut client,
        "org.scry.RoDiff",
        "Ro Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &dialog_tree("Ro Win"));

    rpc.call("get_tree", json!({"workspace": "ro-diff"}));
    peer.set_name(OK_BUTTON, "Apply");
    let diff = rpc.call("tree_diff", json!({"workspace": "ro-diff"}));
    assert_eq!(diff["changed"], true);
}

/// Death of a `client_pid` aborts an in-flight `tree_diff` with
/// `CANCELLED`, same as `get_tree`.
#[test]
fn tree_diff_cancelled_when_client_pid_exits() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "diff-cancel", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "diff-cancel",
        &mut client,
        "org.scry.DiffCancel",
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
    rpc.call("get_tree", json!({"workspace": "diff-cancel"}));

    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("failed to spawn sleep");
    rpc.send_request(
        "tree_diff",
        json!({"workspace": "diff-cancel", "client_pid": child.id()}),
    );
    std::thread::sleep(Duration::from_millis(100));
    child.kill().unwrap();
    child.wait().unwrap();

    let frame = rpc.read_frame();
    let error = &frame["error"];
    assert_eq!(error["code"], -32007, "expected CANCELLED, got {frame}");
}

/// `scry tree-diff` renders hunks compactly in human form and the raw
/// payload in `--json`.
#[test]
fn cli_tree_diff_output() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    let (ws, wl_socket) = create_workspace(&mut rpc, "cli-diff", json!({}));
    let mut client = TestClient::connect(&wl_socket);
    map_window(
        &mut rpc,
        "cli-diff",
        &mut client,
        "org.scry.CliDiff",
        "Diff Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let peer = a11y::serve(a11y_addr, &dialog_tree("Diff Win"));
    rpc.call("get_tree", json!({"workspace": "cli-diff"}));
    peer.set_name(OK_BUTTON, "Apply");

    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_scry"))
            .arg("--socket")
            .arg(socket)
            .args(args)
            .output()
            .expect("failed to run scry")
    };

    let out = run(&["tree-diff", "-w", "cli-diff"]);
    assert!(
        out.status.success(),
        "scry tree-diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("snapshot"), "human output: {text}");
    assert!(text.contains("\"OK\""), "removed line: {text}");
    assert!(text.contains("\"Apply\""), "added line: {text}");
    assert!(text.contains('-'), "removed marker: {text}");
    assert!(text.contains('+'), "added marker: {text}");

    let out = run(&["--json", "tree-diff", "-w", "cli-diff"]);
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(result["from_snapshot"].as_u64().is_some());
    assert!(result["snapshot_id"].as_u64().is_some());
    assert!(result["hunks"].is_array());

    // No further changes: clean "no changes" output.
    let out = run(&["tree-diff", "-w", "cli-diff"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("no changes"), "clean output: {text}");

    // A stale baseline surfaces the structured error on stderr.
    let out = run(&["tree-diff", "-w", "cli-diff", "--from", "9999"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("get_tree"), "resync hint: {stderr}");
}
