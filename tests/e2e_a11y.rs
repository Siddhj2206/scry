//! Real-application M3 E2E verification suite (issue #27): unmodified
//! GTK4 (`zenity` with `GSK_RENDERER=cairo`, `GTK_A11Y=atspi`) and
//! AT-SPI-less Wayland apps driven end-to-end through the live `scry`
//! daemon's Unix-socket JSON-RPC seam *and* the `scry` CLI.
//!
//! Covers the whole M3 surface against real toolkits: fused `get_tree`
//! bounds matching `place_window`, `tree_find` locating a real button,
//! `click_node` driving a real click verified by the app's exit status,
//! `tree_diff` across a real UI transition, the `unavailable` fallback
//! for apps that never touch AT-SPI2, the scripted XDG portal stub on
//! the workspace's private session bus, and orphan-free teardown of
//! apps, D-Bus brokers and `at-spi2-registryd`.
//!
//! Every test is `#[ignore]`d — opt in with:
//!
//!     cargo test --test e2e_a11y -- --ignored --nocapture
//!
//! Tests skip cleanly when the app binaries they need are absent from
//! `PATH`, so the suite runs both on developer hosts and inside the
//! `scry-e2e` container. All synchronization is event-driven through the
//! daemon's own wait verbs (`wait_for_window`, `wait_for_commit`,
//! `wait_for_idle`) — no `sleep()` anywhere.

mod common;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::mpsc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use common::daemon::{RPC_TIMEOUT, RpcClient, spawn_daemon};
use serde_json::{Value, json};
use zbus::zvariant;

/// Server-side bound for every wait verb: comfortably over a real app's
/// startup time, and under `RpcClient`'s 15s read timeout so a stuck app
/// surfaces as a clean RPC error rather than a transport failure.
const WAIT_TIMEOUT_MS: u64 = 12_000;
/// `deadline_ms` passed to tree verbs: bounds each a11y capture while a
/// real toolkit finishes registering with `at-spi2-registryd`.
const TREE_DEADLINE_MS: u64 = 8_000;
/// How many `wait_for_idle`-paced rounds a condition gets to come true —
/// roughly 10s of event-driven polling at the 250ms quiet window used
/// below.
const POLL_ROUNDS: usize = 40;
/// Quiet window for `wait_for_idle` pacing between polls.
const POLL_QUIET_MS: u64 = 250;
/// Slack around fused size assertions: GTK4's CSD shadow may live inside
/// the toplevel surface, so toolkit extents and compositor geometry can
/// legitimately differ by the shadow margin on each edge.
const FUSED_SIZE_SLACK_PX: i64 = 128;

/// Finds `program` on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// Whether `pid` is a live process on this host.
fn pid_alive(pid: u64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// `wait_for_idle` shorthand: block until the workspace is event-free.
fn settle(rpc: &mut RpcClient, workspace: &str, quiet_ms: u64) -> Value {
    rpc.call(
        "wait_for_idle",
        json!({
            "workspace": workspace,
            "quiet_ms": quiet_ms,
            "timeout_ms": WAIT_TIMEOUT_MS,
        }),
    )
}

/// `wait_for_window` by pid.
fn wait_window(rpc: &mut RpcClient, workspace: &str, pid: u64) -> Value {
    rpc.call(
        "wait_for_window",
        json!({"workspace": workspace, "pid": pid, "timeout_ms": WAIT_TIMEOUT_MS}),
    )["window"]
        .clone()
}

/// One bounded `get_tree` capture for `workspace`.
fn get_tree(rpc: &mut RpcClient, workspace: &str) -> Value {
    rpc.call(
        "get_tree",
        json!({"workspace": workspace, "deadline_ms": TREE_DEADLINE_MS}),
    )
}

/// `workspace_list`'s entry for `id` (or `status`'s when `through_status`).
fn workspace_entry(rpc: &mut RpcClient, id: &str) -> Value {
    rpc.call("workspace_list", json!({}))["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("workspace {id:?} not listed"))
}

/// The `status` entry for workspace `id`.
fn status_entry(rpc: &mut RpcClient, id: &str) -> Value {
    rpc.call("status", json!({"workspace": id}))["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("workspace {id:?} missing from status"))
}

/// The fused entry for `window_id` inside a `get_tree` payload.
fn fused_window(tree: &Value, window_id: u64) -> &Value {
    tree["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["window_id"].as_u64() == Some(window_id))
        .unwrap_or_else(|| panic!("window {window_id} missing from get_tree: {tree}"))
}

/// Polls `get_tree` until `window_id` reports `a11y_status == "ok"`,
/// paced by `wait_for_idle` so every iteration is a real bounded wait —
/// a real toolkit takes a moment to `Embed` into `at-spi2-registryd`.
fn await_fused_tree(rpc: &mut RpcClient, workspace: &str, window_id: u64) -> Value {
    for round in 0..POLL_ROUNDS {
        settle(rpc, workspace, POLL_QUIET_MS);
        let tree = get_tree(rpc, workspace);
        let window = &tree["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["window_id"].as_u64() == Some(window_id));
        if let Some(window) = window
            && window["a11y_status"].as_str() == Some("ok")
        {
            return tree;
        }
        if round % 4 == 3 {
            eprintln!(
                "round {round}: window {window_id} a11y_status = {:?}",
                window.and_then(|w| w["a11y_status"].as_str())
            );
        }
    }
    let tree = get_tree(rpc, workspace);
    panic!(
        "window {window_id} never fused an a11y tree; last capture: {}",
        serde_json::to_string_pretty(&tree["windows"]).unwrap()
    );
}

/// Waits for a spawned process to leave `running`, pacing `status` polls
/// with `wait_for_idle`: every iteration is an event-gated bounded wait.
fn await_process_exit(rpc: &mut RpcClient, workspace: &str, pid: u64) -> Value {
    for _ in 0..POLL_ROUNDS {
        settle(rpc, workspace, POLL_QUIET_MS);
        let proc = status_entry(rpc, workspace)["processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pid"].as_u64() == Some(pid))
            .cloned()
            .unwrap_or_else(|| panic!("pid {pid} missing from status"));
        if proc["state"] != "running" {
            return proc;
        }
    }
    panic!("pid {pid} still running in workspace {workspace:?}")
}

/// Whether `window_id` is still mapped in `workspace`.
fn is_mapped(rpc: &mut RpcClient, workspace: &str, window_id: u64) -> bool {
    rpc.call("list_windows", json!({"workspace": workspace}))["windows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["id"].as_u64() == Some(window_id))
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

/// Decodes a capture's base64 PNG into `(width, height, RGBA pixels)`.
fn decode_capture(result: &Value) -> (u32, u32, Vec<u8>) {
    let bytes = BASE64.decode(result["data"].as_str().unwrap()).unwrap();
    let image = image::load_from_memory(&bytes).expect("capture did not decode");
    (image.width(), image.height(), image.to_rgba8().into_raw())
}

/// Asserts decoded pixels actually rendered something: at least one
/// opaque pixel and not a single flat color.
fn assert_rendered(pixels: &[u8], what: &str) {
    let (px, _rest) = pixels.as_chunks::<4>();
    assert!(
        px.iter().any(|p| p[3] > 0),
        "{what}: every pixel is fully transparent"
    );
    let first = px[0];
    assert!(
        px.iter().any(|p| *p != first),
        "{what}: capture is a single flat color — nothing rendered"
    );
}

/// Spawns `zenity --entry` with deterministic button labels into
/// `workspace` and returns its pid.
fn spawn_zenity_entry(rpc: &mut RpcClient, workspace: &str, entry_text: &str) -> u64 {
    rpc.call(
        "spawn_app",
        json!({
            "workspace": workspace,
            "program": "zenity",
            "args": [
                "--entry",
                "--text", "scry a11y e2e",
                "--entry-text", entry_text,
                "--ok-label", "SCRYOK",
                "--cancel-label", "SCRYCANCEL",
                "--width", "420",
                "--height", "200",
            ],
            // The spawn profile already injects these; pinning them keeps
            // the suite's requirements explicit.
            "env": {"GSK_RENDERER": "cairo", "GTK_A11Y": "atspi"},
        }),
    )["pid"]
        .as_u64()
        .unwrap()
}

/// Asserts a fused node's bounds sit at the placed window position:
/// origin exact (fusion anchors the root onto `place_window`'s output),
/// size within the CSD-shadow slack of the compositor geometry.
fn assert_bounds_placed(bounds: &Value, geometry: &Value, what: &str) {
    assert_eq!(
        bounds["x"].as_i64().unwrap(),
        geometry["x"].as_i64().unwrap(),
        "{what}: fused x drifted from placed position: {bounds} vs {geometry}"
    );
    assert_eq!(
        bounds["y"].as_i64().unwrap(),
        geometry["y"].as_i64().unwrap(),
        "{what}: fused y drifted from placed position: {bounds} vs {geometry}"
    );
    let dw = (bounds["width"].as_i64().unwrap() - geometry["width"].as_i64().unwrap()).abs();
    let dh = (bounds["height"].as_i64().unwrap() - geometry["height"].as_i64().unwrap()).abs();
    assert!(
        dw <= FUSED_SIZE_SLACK_PX && dh <= FUSED_SIZE_SLACK_PX,
        "{what}: fused size {bounds} vs compositor {geometry} differs by more than \
         {FUSED_SIZE_SLACK_PX}px"
    );
}

/// Asserts `point_bounds` falls inside `outer` — child widgets must fuse
/// inside their window.
fn assert_bounds_within(point_bounds: &Value, outer: &Value, what: &str) {
    let (x, y) = (
        point_bounds["x"].as_i64().unwrap(),
        point_bounds["y"].as_i64().unwrap(),
    );
    let (ox, oy, ow, oh) = (
        outer["x"].as_i64().unwrap(),
        outer["y"].as_i64().unwrap(),
        outer["width"].as_i64().unwrap(),
        outer["height"].as_i64().unwrap(),
    );
    assert!(
        x >= ox - FUSED_SIZE_SLACK_PX
            && y >= oy - FUSED_SIZE_SLACK_PX
            && x <= ox + ow + FUSED_SIZE_SLACK_PX
            && y <= oy + oh + FUSED_SIZE_SLACK_PX,
        "{what}: node bounds {point_bounds} fell outside window {outer}"
    );
}

// -- GTK4: JSON-RPC pipeline ------------------------------------------------------

/// The headline GTK4 scenario over raw JSON-RPC: an unmodified `zenity`
/// dialog maps headlessly, its real widget tree fuses onto the
/// `place_window` coordinates, `tree_find` locates the OK button,
/// `type_text` produces a real `tree_diff` transition, and `click_node`
/// drives a real click the app confirms by exiting 0.
#[test]
#[ignore = "opt-in real-app e2e: needs zenity (GTK4) on PATH"]
fn zenity_gtk4_fused_tree_find_click_node_rpc() {
    let Some(_zenity) = which("zenity") else {
        eprintln!("skipping: zenity not on PATH");
        return;
    };

    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    rpc.call("workspace_create", json!({"id": "gtk4"}));
    let ws = workspace_entry(&mut rpc, "gtk4");
    assert_eq!(
        ws["bus"]["session"]["state"], "running",
        "session bus down: {}",
        ws["bus"]
    );
    assert_eq!(
        ws["bus"]["at_spi"]["state"], "running",
        "a11y bus down: {}",
        ws["bus"]
    );

    // -- map the real GTK4 app ----------------------------------------------------
    let pid = spawn_zenity_entry(&mut rpc, "gtk4", "seed");
    let window = wait_window(&mut rpc, "gtk4", pid);
    let window_id = window["id"].as_u64().unwrap();
    rpc.call(
        "wait_for_commit",
        json!({"workspace": "gtk4", "after_seq": 0, "window": window_id, "timeout_ms": WAIT_TIMEOUT_MS}),
    );

    // -- deterministic placement the fusion must match -----------------------------
    rpc.call(
        "place_window",
        json!({"workspace": "gtk4", "window": window_id, "x": 160, "y": 90}),
    );
    let tree = await_fused_tree(&mut rpc, "gtk4", window_id);

    // The bus report shows a live registry with the GTK4 app embedded.
    assert_eq!(
        tree["a11y"]["state"], "connected",
        "bus report: {}",
        tree["a11y"]
    );
    assert!(
        tree["a11y"]["registered_apps"].as_u64().unwrap() >= 1,
        "no registered a11y apps: {}",
        tree["a11y"]
    );

    // The window fused with the zenity process's own AT-SPI peer.
    let w = fused_window(&tree, window_id);
    assert_eq!(w["a11y_status"], "ok", "window entry: {w}");
    assert_eq!(w["a11y"]["pid"].as_u64().unwrap(), pid, "correlation: {w}");

    // The fused root lands exactly on the placed window coordinates —
    // GTK4's toolkit extents translated onto compositor geometry.
    let geometry = &w["geometry"];
    assert_eq!(geometry["x"], 160);
    assert_eq!(geometry["y"], 90);
    let root = &w["root"];
    assert!(
        ["frame", "dialog", "window"].contains(&root["role"].as_str().unwrap_or("")),
        "unexpected fused root role: {root}"
    );
    assert_bounds_placed(&root["bounds"], geometry, "fused root");
    assert!(
        !root["children"].as_array().unwrap().is_empty(),
        "real GTK4 tree must have descendants: {root}"
    );

    // -- tree_find locates the real OK button ------------------------------------
    // Real GTK4 exposes the label inside the button with the same
    // accessible name — scope the search to the button role.
    let found = rpc.call(
        "tree_find",
        json!({"workspace": "gtk4", "name": "SCRYOK", "role": "button", "context": 1, "deadline_ms": TREE_DEADLINE_MS}),
    );
    assert_eq!(found["match_count"], 1, "tree_find: {found}");
    let m = &found["matches"][0];
    assert_eq!(m["window_id"].as_u64().unwrap(), window_id);
    assert!(
        m["role"].as_str().unwrap().contains("button"),
        "SCRYOK should be a button: {m}"
    );
    assert!(m["ref"].as_str().unwrap().starts_with('e'));
    assert_bounds_within(&m["bounds"], geometry, "OK button");
    let ok_ref = m["ref"].as_str().unwrap().to_string();
    eprintln!("tree_find hit: {}", m["context"]["match"]);

    // -- type_text → a real UI transition → tree_diff ------------------------------
    let baseline = get_tree(&mut rpc, "gtk4")["snapshot_id"].as_u64().unwrap();
    rpc.call(
        "type_text",
        json!({"workspace": "gtk4", "window": window_id, "text": " SCRY-TYPED"}),
    );
    settle(&mut rpc, "gtk4", 500);
    let diff = rpc.call(
        "tree_diff",
        json!({"workspace": "gtk4", "from_snapshot": baseline, "deadline_ms": TREE_DEADLINE_MS}),
    );
    assert_eq!(diff["changed"], true, "typing produced no diff: {diff}");
    let added = added_lines(&diff);
    eprintln!("tree_diff added lines: {added:?}");
    assert!(
        added.iter().any(|line| line.contains("SCRY-TYPED")),
        "typed text missing from diff hunks: {diff}"
    );

    // -- click_node drives a real click; zenity exits 0 on OK ---------------------
    let clicked = rpc.call(
        "click_node",
        json!({"workspace": "gtk4", "ref": ok_ref, "deadline_ms": TREE_DEADLINE_MS}),
    );
    assert_eq!(clicked["clicked"], 1, "{clicked}");
    assert_eq!(clicked["button"], "left");
    assert!(clicked["x"].as_f64().unwrap() >= 160.0, "{clicked}");
    assert!(clicked["y"].as_f64().unwrap() >= 90.0, "{clicked}");

    let proc = await_process_exit(&mut rpc, "gtk4", pid);
    assert_eq!(
        proc["exit_code"], 0,
        "zenity should exit 0 on the OK button: {proc}"
    );
    assert!(
        !is_mapped(&mut rpc, "gtk4", window_id),
        "zenity window survived clicking OK"
    );

    rpc.call("workspace_close", json!({"workspace": "gtk4"}));
}

// -- GTK4: CLI pipeline -------------------------------------------------------------

/// Runs `scry --socket <socket> <args>` and captures the result.
fn scry(socket: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .expect("failed to run scry")
}

/// Runs `scry --socket <socket> --json <args>`: exit 0 + parsed stdout.
fn scry_json(socket: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args(args)
        .output()
        .expect("failed to run scry");
    assert!(
        out.status.success(),
        "scry --json {args:?} failed (status {}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("scry --json stdout is not JSON")
}

/// The same GTK4 scenario through the thin `scry` CLI: `scry tree`
/// renders the real widget tree, `scry tree-find` locates the button,
/// typing produces a `scry tree-diff` hunk, and `scry click-node` clicks
/// the real button — verified by the app exiting 0.
#[test]
#[ignore = "opt-in real-app e2e: needs zenity (GTK4) on PATH"]
fn zenity_gtk4_cli_tree_find_diff_click() {
    let Some(_zenity) = which("zenity") else {
        eprintln!("skipping: zenity not on PATH");
        return;
    };

    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);
    rpc.call("workspace_create", json!({"id": "gtk4-cli"}));

    let pid = spawn_zenity_entry(&mut rpc, "gtk4-cli", "seed");
    let window = wait_window(&mut rpc, "gtk4-cli", pid);
    let window_id = window["id"].as_u64().unwrap();
    rpc.call(
        "wait_for_commit",
        json!({"workspace": "gtk4-cli", "after_seq": 0, "window": window_id, "timeout_ms": WAIT_TIMEOUT_MS}),
    );
    rpc.call(
        "place_window",
        json!({"workspace": "gtk4-cli", "window": window_id, "x": 200, "y": 140}),
    );
    await_fused_tree(&mut rpc, "gtk4-cli", window_id);

    // `scry tree` human output: real roles, names, bounds.
    let out = scry(socket, &["tree", "-w", "gtk4-cli"]);
    assert!(
        out.status.success(),
        "scry tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    eprintln!("scry tree output:\n{text}");
    assert!(text.contains("snapshot"), "human output: {text}");
    assert!(text.contains("[e1]"), "human output: {text}");
    assert!(text.contains("SCRYOK"), "human output: {text}");
    assert!(text.contains("(200,"), "placed origin in output: {text}");

    // `scry --json tree`: the raw get_tree payload carries the fused window.
    let result = scry_json(socket, &["tree", "-w", "gtk4-cli"]);
    let w = fused_window(&result, window_id);
    assert_eq!(w["a11y_status"], "ok", "cli tree: {w}");

    // `scry tree-find`: the OK button by name, exact match.
    let found = scry_json(
        socket,
        &[
            "tree-find",
            "-w",
            "gtk4-cli",
            "--name",
            "SCRYOK",
            "--role",
            "button",
            "--match",
            "exact",
        ],
    );
    assert_eq!(found["match_count"], 1, "tree-find: {found}");
    let ok_ref = found["matches"][0]["ref"].as_str().unwrap().to_string();
    assert!(ok_ref.starts_with('e'));

    // `scry tree-diff` catches the real UI transition after typing.
    scry_json(socket, &["tree", "-w", "gtk4-cli"]); // baseline snapshot
    rpc.call(
        "type_text",
        json!({"workspace": "gtk4-cli", "window": window_id, "text": " CLI-TYPED"}),
    );
    settle(&mut rpc, "gtk4-cli", 500);
    let diff = scry_json(socket, &["tree-diff", "-w", "gtk4-cli"]);
    assert_eq!(diff["changed"], true, "cli tree-diff: {diff}");
    let added = added_lines(&diff);
    assert!(
        added.iter().any(|line| line.contains("CLI-TYPED")),
        "typed text missing from cli diff: {diff}"
    );
    let out = scry(socket, &["tree-diff", "-w", "gtk4-cli"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("CLI-TYPED") || text.contains("no changes"),
        "human tree-diff: {text}"
    );

    // `scry click-node` on the button's ref: the app exits 0 on OK.
    let out = scry(socket, &["click-node", "-w", "gtk4-cli", "--ref", &ok_ref]);
    assert!(
        out.status.success(),
        "scry click-node failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&ok_ref), "click-node output: {text}");

    let proc = await_process_exit(&mut rpc, "gtk4-cli", pid);
    assert_eq!(
        proc["exit_code"], 0,
        "zenity should exit 0 after cli click-node: {proc}"
    );
    assert!(!is_mapped(&mut rpc, "gtk4-cli", window_id));

    rpc.call("workspace_close", json!({"workspace": "gtk4-cli"}));
}

// -- AT-SPI-less app fallback ---------------------------------------------------------

/// A real Wayland app that never registers an AT-SPI2 tree.
#[derive(Clone, Copy)]
struct AtspiLessApp {
    program: &'static str,
    args: &'static [&'static str],
    /// Whether its mapped window renders real pixels (a 1×1 focus-grab
    /// surface like `wl-copy`'s can't be asserted on content).
    renders: bool,
}

/// Preference order: the ticket's own candidates first, then broadly
/// packaged SDL/wl-clipboard clients that exist on more hosts.
/// A continuously painting app (`ffplay`) is a legitimate candidate:
/// every wait below is event-driven (`wait_for_window` /
/// `wait_for_commit` / `get_tree`'s own deadline) — `wait_for_idle`
/// would never resolve for a video player, and that is correct
/// behavior, not a test hack.
const ATSPI_LESS_CANDIDATES: &[AtspiLessApp] = &[
    AtspiLessApp {
        program: "weston-simple-shm",
        args: &[],
        renders: true,
    },
    AtspiLessApp {
        program: "foot",
        args: &[],
        renders: true,
    },
    AtspiLessApp {
        program: "ffplay",
        args: &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=600:size=320x240:rate=30",
            "-an",
            "-nostats",
            "-loglevel",
            "error",
            "-x",
            "320",
            "-y",
            "240",
        ],
        renders: true,
    },
];

/// An unmodified Wayland app with no AT-SPI2 tree still reports its
/// compositor window geometry in `get_tree` with an explicit fallback
/// status + `capture_window` hint — and the promised pixel capture works.
#[test]
#[ignore = "opt-in real-app e2e: needs a simple wayland app on PATH"]
fn atspi_less_app_geometry_fallback_and_capture() {
    let Some(app) = ATSPI_LESS_CANDIDATES
        .iter()
        .find(|app| which(app.program).is_some())
    else {
        eprintln!("skipping: none of weston-simple-shm/foot/ffplay on PATH");
        return;
    };
    eprintln!("driving {} as the AT-SPI-less app", app.program);

    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    rpc.call("workspace_create", json!({"id": "bare"}));

    let pid = rpc.call(
        "spawn_app",
        json!({
            "workspace": "bare",
            "program": app.program,
            "args": app.args,
        }),
    )["pid"]
        .as_u64()
        .unwrap();
    let window = wait_window(&mut rpc, "bare", pid);
    let window_id = window["id"].as_u64().unwrap();
    rpc.call(
        "wait_for_commit",
        json!({"workspace": "bare", "after_seq": 0, "window": window_id, "timeout_ms": WAIT_TIMEOUT_MS}),
    );

    // Deterministic placement: the fallback window still reports the
    // compositor's authoritative geometry. `wait_for_window` resolves on
    // the `WindowUpdated` event the move emits — no idle settle needed
    // (a painting app never idles).
    rpc.call(
        "place_window",
        json!({"workspace": "bare", "window": window_id, "x": 220, "y": 150}),
    );
    let placed = wait_window(&mut rpc, "bare", pid);
    assert_eq!(placed["geometry"]["x"], 220, "placed: {placed}");
    assert_eq!(placed["geometry"]["y"], 150, "placed: {placed}");

    // A bounded poll for the app to *not* appear on the a11y bus: the
    // absence of a peer is itself only observable after the bus settles.
    // Each round is paced by a short `wait_for_commit` — it resolves on
    // the next frame for painting apps (ffplay ~30fps) and times out for
    // static ones; either way real time passes with no `sleep`.
    let mut fused = None;
    let mut after_seq = 0;
    for _ in 0..10 {
        let _wait_id = rpc.send_request(
            "wait_for_commit",
            json!({"workspace": "bare", "after_seq": after_seq, "window": window_id, "timeout_ms": 400}),
        );
        let frame = rpc.read_frame();
        if let Some(seq) = frame["result"]["seq"].as_u64() {
            after_seq = seq;
        }
        let tree = get_tree(&mut rpc, "bare");
        let w = fused_window(&tree, window_id).clone();
        let done = w["a11y_status"] != "ok";
        fused = Some(w);
        if done {
            break;
        }
    }
    let w = fused.unwrap();

    // The window reports compositor metadata + a fallback status and a
    // pixel-capture hint — never an absent entry or a failed request.
    assert_eq!(w["window_id"].as_u64().unwrap(), window_id);
    assert_eq!(w["pid"].as_u64().unwrap(), pid);
    assert_eq!(w["geometry"]["x"], 220, "fallback window: {w}");
    assert_eq!(w["geometry"]["y"], 150, "fallback window: {w}");
    assert!(
        ["unavailable", "empty"].contains(&w["a11y_status"].as_str().unwrap_or("")),
        "expected an explicit fallback status, got: {w}"
    );
    assert!(
        w["hint"].as_str().unwrap_or("").contains("capture_window"),
        "expected a capture_window fallback hint: {w}"
    );
    assert!(w["root"].is_null() || w["root"]["children"].as_array().is_none_or(Vec::is_empty));

    // The promised fallback: real pixels off the compositor's own
    // occlusion-free capture.
    let capture = rpc.call(
        "capture_window",
        json!({"workspace": "bare", "window": window_id, "format": "png"}),
    );
    assert_eq!(capture["format"], "png");
    let (cw, ch, pixels) = decode_capture(&capture);
    assert!(cw > 0 && ch > 0, "empty capture: {cw}x{ch}");
    if app.renders {
        assert_rendered(&pixels, "AT-SPI-less app capture");
    }

    rpc.call("workspace_close", json!({"workspace": "bare"}));
    assert!(
        !pid_alive(pid),
        "{} (pid {pid}) survived workspace_close",
        app.program
    );
}

// -- Portal sanity + orphan-free teardown -----------------------------------------------

/// A received `org.freedesktop.portal.Request::Response` signal.
#[derive(Debug)]
struct PortalResponse {
    /// The request object path the signal was emitted on.
    path: String,
    /// The `response` code (0 accepted, 1 cancelled, 2 closed).
    code: u32,
    /// The `results` dict.
    results: HashMap<String, zvariant::OwnedValue>,
}

/// A real D-Bus client connection to `address` (`unix:path=...`).
fn connect_bus(address: &str) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(address)
        .unwrap_or_else(|e| panic!("invalid bus address {address:?}: {e}"))
        .build()
        .unwrap_or_else(|e| panic!("failed to connect to bus {address:?}: {e}"))
}

/// Subscribes to every `Request::Response` signal on the private session
/// bus and streams decoded responses back — a real portal client's match
/// rule, installed before the call.
fn response_listener(address: &str) -> mpsc::Receiver<PortalResponse> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let address = address.to_string();
    std::thread::Builder::new()
        .name("portal-response-listener".into())
        .spawn(move || {
            let conn = connect_bus(&address);
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.portal.Request")
                .unwrap()
                .member("Response")
                .unwrap()
                .build();
            let iter = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(32))
                .expect("failed to subscribe to Response signals");
            let _ = ready_tx.send(());
            for msg in iter.flatten() {
                let path = msg
                    .header()
                    .path()
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let Ok((code, results)) = msg
                    .body()
                    .deserialize::<(u32, HashMap<String, zvariant::OwnedValue>)>()
                else {
                    continue;
                };
                if tx
                    .send(PortalResponse {
                        path,
                        code,
                        results,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("failed to spawn response listener");
    ready_rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("portal response listener never subscribed");
    rx
}

/// Receives the next portal `Response`, bounded.
fn next_response(rx: &mpsc::Receiver<PortalResponse>) -> PortalResponse {
    rx.recv_timeout(RPC_TIMEOUT)
        .expect("timed out waiting for portal Response signal")
}

/// A decoded `a{sv}` options map.
type Options = HashMap<String, zvariant::Value<'static>>;

/// Options map containing just a `handle_token`.
fn token_options(token: &str) -> Options {
    HashMap::from([(
        "handle_token".to_string(),
        zvariant::Value::from(token.to_string()),
    )])
}

/// Calls `method` on `interface` at `org.freedesktop.portal.Desktop` on
/// the workspace's private session bus; returns the request handle path.
fn call_portal(
    conn: &zbus::blocking::Connection,
    interface: &str,
    method: &str,
    args: &(impl serde::Serialize + zvariant::DynamicType),
) -> zvariant::OwnedObjectPath {
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some(interface),
            method,
            args,
        )
        .unwrap_or_else(|e| panic!("{interface}.{method} failed: {e}"));
    reply
        .body()
        .deserialize::<zvariant::OwnedObjectPath>()
        .unwrap_or_else(|e| panic!("{interface}.{method} reply was not a handle: {e}"))
}

/// The `uris` array out of a Response's `results` dict.
fn uris_of(response: &PortalResponse) -> Vec<String> {
    match response.results.get("uris").map(zvariant::Value::try_from) {
        Some(Ok(zvariant::Value::Array(array))) => array
            .inner()
            .iter()
            .filter_map(|v| match v {
                zvariant::Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        other => panic!("missing/invalid uris in results: {other:?}"),
    }
}

/// A real D-Bus client on the workspace's private session bus hits the
/// scripted portal stub while a real Wayland app runs alongside; then
/// `workspace_close` tears down every app, both D-Bus brokers and the
/// AT-SPI2 registry — no orphans, no leftover runtime dir.
#[test]
#[ignore = "opt-in real-app e2e: needs a D-Bus broker + at-spi2-core"]
fn portal_sanity_and_teardown_leaves_no_orphans() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    rpc.call("workspace_create", json!({"id": "portal-ws"}));
    let ws = workspace_entry(&mut rpc, "portal-ws");
    let session_addr = ws["bus"]["session"]["address"]
        .as_str()
        .unwrap()
        .to_string();
    let runtime_dir = PathBuf::from(ws["runtime_dir"].as_str().unwrap());

    // A real Wayland app running alongside the portal traffic.
    let mut app_pid = None;
    let mut window_id = None;
    if which("zenity").is_some() {
        app_pid = Some(spawn_zenity_entry(&mut rpc, "portal-ws", "portal e2e"));
        let window = wait_window(&mut rpc, "portal-ws", app_pid.unwrap());
        window_id = Some(window["id"].as_u64().unwrap());
    } else {
        eprintln!("zenity not on PATH: portal test runs without a sidecar app");
    }

    // A real portal client: match rule first, then the call, on the
    // workspace's private session bus — never the host's.
    let responses = response_listener(&session_addr);
    let conn = connect_bus(&session_addr);

    // Scripted FileChooser: the accept + URIs come back on the
    // predictable request path.
    rpc.call(
        "portal_script_set",
        json!({
            "workspace": "portal-ws",
            "interface": "filechooser",
            "response": "accept",
            "uris": ["file:///tmp/scry-a.txt"],
        }),
    );
    let handle = call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "Pick one", token_options("rtok1")),
    );
    let response = next_response(&responses);
    assert_eq!(response.path, handle.as_str());
    assert_eq!(response.code, 0, "scripted OpenFile: {response:?}");
    assert_eq!(uris_of(&response), ["file:///tmp/scry-a.txt"]);

    // An unscripted OpenURI call cancels immediately — and alongside the
    // running app nothing deadlocks or spills to the host.
    call_portal(
        &conn,
        "org.freedesktop.portal.OpenURI",
        "OpenURI",
        &("", "https://scry-e2e.invalid/", token_options("rtok2")),
    );
    assert_eq!(next_response(&responses).code, 1);

    // History records both calls on *this* workspace only.
    let history = rpc.call("portal_history", json!({"workspace": "portal-ws"}));
    assert_eq!(history["count"], 2, "history: {history}");
    assert_eq!(history["invocations"][0]["method"], "OpenFile");
    assert_eq!(history["invocations"][1]["method"], "OpenURI");

    // The app is still healthy alongside the portal traffic.
    if let Some(id) = window_id {
        assert!(is_mapped(&mut rpc, "portal-ws", id));
    }

    // Capture every pid teardown must reap: the app, both brokers and
    // the AT-SPI2 registry daemon.
    let entry = status_entry(&mut rpc, "portal-ws");
    let mut pids: Vec<(String, u64)> = Vec::new();
    if let Some(pid) = app_pid {
        pids.push(("app".to_string(), pid));
    }
    for (label, pid) in [
        ("session broker", &entry["bus"]["session"]["pid"]),
        ("a11y broker", &entry["bus"]["at_spi"]["pid"]),
        (
            "at-spi2-registryd",
            &entry["bus"]["at_spi"]["registry"]["pid"],
        ),
    ] {
        if let Some(pid) = pid.as_u64() {
            pids.push((label.to_string(), pid));
        }
    }
    assert!(
        pids.len() >= 3,
        "expected app + brokers + registry pids in status: {entry}"
    );

    // Close the workspace: apps die, portal stops, brokers and
    // registryd get SIGTERM→SIGKILL, sockets + runtime dir are removed.
    rpc.call("workspace_close", json!({"workspace": "portal-ws"}));
    for (label, pid) in &pids {
        assert!(
            !pid_alive(*pid),
            "{label} (pid {pid}) survived workspace_close — orphaned process"
        );
    }
    assert!(
        !runtime_dir.exists(),
        "workspace runtime dir {} survived close",
        runtime_dir.display()
    );

    let remaining = rpc.call("workspace_list", json!({}))["workspaces"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(remaining, 0, "workspaces survived close");
    let status = rpc.call("status", json!({}));
    assert_eq!(status["healthy"], true, "daemon unhealthy: {status}");
}
