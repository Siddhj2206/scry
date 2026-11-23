//! Integration tests for `tree_find` (M3.5): low-token search of the
//! workspace's fused accessibility tree by role, name/text (substring or
//! exact), state and window, returning refs, fused bounds and canonical
//! context lines — plus `scry tree-find` output.
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

/// A fake window subtree with a dialog-ish layout: a frame holding a
/// label, a text entry, a checked checkbox and two buttons.
fn dialog_frame(title: &str) -> FakeNode {
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
        )
}

/// `dialog_frame` wrapped in the application root a real toolkit serves.
fn dialog_tree(title: &str) -> FakeNode {
    FakeNode::new("fake-app", "application").child(dialog_frame(title))
}

/// `tree_find` by name substring returns each matching node with its
/// ref, fused bounds and canonical context lines around the match.
#[test]
fn tree_find_matches_name_with_context() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "find", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "find",
        &mut client,
        "org.scry.Find",
        "Find Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &dialog_tree("Find Win"));

    let result = rpc.call(
        "tree_find",
        json!({"workspace": "find", "name": "OK", "context": 1}),
    );
    assert!(result["snapshot_id"].as_u64().unwrap() >= 1);
    assert_eq!(result["match_count"], 1);
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);

    let m = &matches[0];
    assert_eq!(m["ref"], "e6", "match: {m}");
    assert_eq!(m["role"], "button");
    assert_eq!(m["name"], "OK");
    assert!(m["bounds"]["width"].as_i64().unwrap() == 80, "bounds: {m}");
    assert!(m["window_id"].as_u64().is_some());

    // Context: one canonical line before (the checkbox), the match line
    // itself, one after (the Cancel button).
    let context = &m["context"];
    let before = context["before"].as_array().unwrap();
    assert_eq!(before.len(), 1, "context: {context}");
    assert!(
        before[0].as_str().unwrap().contains("Remember me"),
        "before: {before:?}"
    );
    let matched = context["match"].as_str().unwrap();
    assert!(matched.contains("[e6]"), "match line: {matched}");
    assert!(matched.contains("button"), "match line: {matched}");
    assert!(matched.contains("\"OK\""), "match line: {matched}");
    let after = context["after"].as_array().unwrap();
    assert_eq!(after.len(), 1, "context: {context}");
    assert!(
        after[0].as_str().unwrap().contains("Cancel"),
        "after: {after:?}"
    );

    // The refs the match reports resolve in the recorded snapshot:
    // `root_ref` scoping accepts them.
    let click_ref = m["ref"].as_str().unwrap();
    let drilled = rpc.call(
        "get_tree",
        json!({"workspace": "find", "root_ref": click_ref}),
    );
    assert_eq!(drilled["windows"][0]["root"]["name"], "OK");
}

/// `tree_find` combines filters: role, state and exact-match name/text
/// all AND together, and `window_id` scopes the search to one window.
#[test]
fn tree_find_filters_role_state_exact_and_window() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "filters", json!({}));
    let mut client = TestClient::connect(&socket);
    let window = map_window(
        &mut rpc,
        "filters",
        &mut client,
        "org.scry.Filters",
        "Filter Win",
        400,
        300,
    );
    let window_id = window["id"].as_u64().unwrap();
    map_window(
        &mut rpc,
        "filters",
        &mut client,
        "org.scry.Filters",
        "Other Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(
        a11y_addr,
        &FakeNode::new("fake-app", "application")
            .child(dialog_frame("Filter Win"))
            .child(dialog_frame("Other Win")),
    );

    // Role filter: two buttons in the scoped window's tree.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "role": "button", "context": 0}),
    );
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 4, "two windows × two buttons: {matches:?}");
    assert!(matches.iter().all(|m| m["role"] == "button"));

    // Role + window scope narrows to one window's buttons.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "role": "button", "window_id": window_id, "context": 0}),
    );
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    assert!(
        matches
            .iter()
            .all(|m| m["window_id"].as_u64() == Some(window_id))
    );

    // State filter: only the checkbox carries `checked`.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "state": "checked", "window_id": window_id, "context": 0}),
    );
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["role"], "check box");
    assert_eq!(matches[0]["name"], "Remember me");

    // Exact match: substring "Remember" finds the checkbox by name but
    // "Remember" is not its exact name.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "name": "Remember", "context": 0}),
    );
    assert_eq!(result["match_count"], 2); // one per window
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "name": "Remember", "match": "exact", "context": 0}),
    );
    assert_eq!(result["match_count"], 0);
    assert!(result["matches"].as_array().unwrap().is_empty());
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "name": "Remember me", "match": "exact", "context": 0}),
    );
    assert_eq!(result["match_count"], 2);

    // Text filter: the entry's Text contents, substring and exact.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "text": "example.com", "context": 0}),
    );
    assert_eq!(result["match_count"], 2);
    assert_eq!(result["matches"][0]["role"], "entry");
    assert_eq!(result["matches"][0]["text"], "sid@example.com");

    // AND semantics: role + name + state must all hold.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "role": "button", "name": "OK", "state": "checked", "context": 0}),
    );
    assert_eq!(result["match_count"], 0);
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "filters", "role": "check box", "state": "checked", "context": 0}),
    );
    assert_eq!(result["match_count"], 2); // one per window

    // Validation: no criteria, a bad match mode, and an unknown window
    // are all structured errors.
    let err = rpc.call_err("tree_find", json!({"workspace": "filters"}));
    assert_eq!(err.code, -32602, "expected INVALID_PARAMS, got {err:?}");
    let err = rpc.call_err(
        "tree_find",
        json!({"workspace": "filters", "name": "x", "match": "fuzzy"}),
    );
    assert_eq!(err.code, -32602, "expected INVALID_PARAMS, got {err:?}");
    let err = rpc.call_err(
        "tree_find",
        json!({"workspace": "filters", "name": "x", "window_id": 9999}),
    );
    assert_eq!(err.code, -32004, "expected UNKNOWN_WINDOW, got {err:?}");
}

/// A `ROLE_PASSWORD_TEXT` match carries only the `[REDACTED]` sentinel —
/// the secret never appears in match fields, context lines, or anywhere
/// in the response.
#[test]
fn tree_find_never_exposes_password_contents() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "pw", json!({}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "pw",
        &mut client,
        "org.scry.Pw",
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
                FakeNode::new("Password", "password text")
                    .text(secret)
                    .value(21.0)
                    .value_text(secret),
            ),
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &tree);

    // Searching the password node by name/role returns its public fields
    // — role, name, ref — with only the redaction sentinel for content.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "pw", "role": "password text", "context": 2}),
    );
    let rendered = serde_json::to_string(&result).unwrap();
    assert!(
        !rendered.contains(secret),
        "the secret must never appear in tree_find output: {rendered}"
    );
    assert_eq!(result["match_count"], 1);
    let m = &result["matches"][0];
    assert_eq!(m["name"], "Password");
    assert_eq!(m["redacted"], true);
    assert_eq!(m["text"], "[REDACTED]");
    assert_eq!(m["value"], "[REDACTED]");
    assert_eq!(m["value_text"], "[REDACTED]");
    let matched_line = m["context"]["match"].as_str().unwrap();
    assert!(matched_line.contains("[REDACTED]"), "line: {matched_line}");

    // And the secret is unsearchable: text matching only ever sees the
    // sentinel.
    let result = rpc.call(
        "tree_find",
        json!({"workspace": "pw", "text": secret, "context": 0}),
    );
    assert_eq!(result["match_count"], 0);
}

/// `tree_find` is a read-only verb: `readOnly` workspaces allow it.
#[test]
fn tree_find_allowed_on_readonly_workspace() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (ws, socket) = create_workspace(&mut rpc, "ro", json!({"read_only": true}));
    let mut client = TestClient::connect(&socket);
    map_window(
        &mut rpc,
        "ro",
        &mut client,
        "org.scry.Ro",
        "Ro Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &dialog_tree("Ro Win"));

    let result = rpc.call(
        "tree_find",
        json!({"workspace": "ro", "role": "button", "context": 0}),
    );
    assert_eq!(result["match_count"], 2);
}

/// `scry tree-find` renders matches with context in compact human form
/// and the raw payload in `--json`.
#[test]
fn cli_tree_find_output() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    let (ws, wl_socket) = create_workspace(&mut rpc, "cli-find", json!({}));
    let mut client = TestClient::connect(&wl_socket);
    map_window(
        &mut rpc,
        "cli-find",
        &mut client,
        "org.scry.CliFind",
        "Find Win",
        400,
        300,
    );
    let a11y_addr = ws["bus"]["at_spi"]["address"].as_str().unwrap();
    let _peer = a11y::serve(a11y_addr, &dialog_tree("Find Win"));

    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_scry"))
            .arg("--socket")
            .arg(socket)
            .args(args)
            .output()
            .expect("failed to run scry")
    };

    let out = run(&["tree-find", "-w", "cli-find", "--name", "OK"]);
    assert!(
        out.status.success(),
        "scry tree-find failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("snapshot"), "human output: {text}");
    assert!(text.contains("[e6]"), "human output: {text}");
    assert!(text.contains("button"), "human output: {text}");
    assert!(text.contains("\"OK\""), "human output: {text}");
    // Default context shows neighbouring canonical lines.
    assert!(text.contains("Remember me"), "human output: {text}");
    assert!(text.contains("Cancel"), "human output: {text}");

    let out = run(&[
        "tree-find",
        "-w",
        "cli-find",
        "--name",
        "OK",
        "--match",
        "exact",
        "--context",
        "0",
    ]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"OK\""), "exact output: {text}");
    assert!(
        !text.contains("Remember me"),
        "--context 0 must suppress context: {text}"
    );

    // A name that misses everything is a clean no-match result.
    let out = run(&["tree-find", "-w", "cli-find", "--name", "Zzz"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("no matches"), "no-match output: {text}");

    // --json: the raw tree_find payload.
    let out = run(&["--json", "tree-find", "-w", "cli-find", "--role", "button"]);
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["match_count"], 2);
    assert_eq!(result["matches"][0]["ref"], "e6");
    assert!(result["snapshot_id"].as_u64().unwrap() >= 1);

    // A daemon-side error surfaces on stderr.
    let out = run(&["tree-find", "-w", "cli-find"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tree_find"), "stderr: {stderr}");
}
