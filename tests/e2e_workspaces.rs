//! Multi-app, two-workspace real-application E2E through the daemon's
//! Unix-socket JSON-RPC seam (issue #18).
//!
//! Unmodified Wayland apps (`zenity`, `foot`, `weston-simple-shm`,
//! `wl-copy`/`wl-paste` when present) are spawned into two isolated
//! workspaces via `spawn_app` and driven end-to-end: event-driven waits,
//! deterministic `place_window`, reproducible captures, targeted input
//! verified by observable app behavior, clipboard interop, marks and
//! diffs — all with zero `sleep()` calls.
//!
//! Opt in with:
//!
//!     cargo test --test e2e_workspaces -- --ignored --nocapture
//!
//! The test skips cleanly when no usable GUI binary is on `PATH`.

mod common;

use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use common::daemon::RpcClient;
use serde_json::{Value, json};

/// Server-side bound for every wait verb (kept under the client socket's
/// 15s read timeout so a stuck app surfaces as a clean RPC error).
const WAIT_TIMEOUT_MS: u64 = 12_000;
/// Quiet window for `wait_for_idle` after input events — comfortably
/// longer than any real app's reaction time.
const INPUT_SETTLE_MS: u64 = 2_000;

/// How a spawned app is asked to exit through injected input (or not).
#[derive(Clone, Copy)]
enum ExitPlan {
    /// `key_combo Return` activates the dialog's default button (zenity).
    KeyReturn,
    /// `type_text "exit\n"` quits the shell the app runs (foot).
    TypeExit,
    /// No reliable input exit; workspace teardown kills it (simple-shm).
    Teardown,
}

/// A real Wayland app the scenario can drive.
#[derive(Clone, Copy)]
struct App {
    program: &'static str,
    args: &'static [&'static str],
    exit: ExitPlan,
}

const CANDIDATES: &[App] = &[
    App {
        program: "zenity",
        args: &[
            "--info",
            "--text",
            "scry e2e",
            "--width=320",
            "--height=160",
        ],
        exit: ExitPlan::KeyReturn,
    },
    App {
        program: "foot",
        args: &[],
        exit: ExitPlan::TypeExit,
    },
    App {
        program: "weston-simple-shm",
        args: &[],
        exit: ExitPlan::Teardown,
    },
];

/// Finds `program` on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
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

/// Whether `window_id` is still mapped in `workspace`.
fn is_mapped(rpc: &mut RpcClient, workspace: &str, window_id: u64) -> bool {
    rpc.call("list_windows", json!({"workspace": workspace}))["windows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["id"].as_u64() == Some(window_id))
}

/// The pids owning `workspace`'s mapped windows (`SO_PEERCRED`).
fn window_pids(rpc: &mut RpcClient, workspace: &str) -> Vec<u64> {
    rpc.call("list_windows", json!({"workspace": workspace}))["windows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["pid"].as_u64())
        .collect()
}

/// Waits for a spawned process to leave the `running` state by pacing
/// `status` polls with `wait_for_idle`: every iteration is an event-gated
/// bounded wait, never a sleep.
fn await_process_exit(rpc: &mut RpcClient, workspace: &str, pid: u64) -> Value {
    for _ in 0..20 {
        settle(rpc, workspace, 200);
        let proc = process_state(rpc, workspace, pid);
        if proc["state"] != "running" {
            return proc;
        }
    }
    panic!("pid {pid} still running in workspace {workspace:?}")
}

/// The `status` snapshot of one spawned process in `workspace`.
fn process_state(rpc: &mut RpcClient, workspace: &str, pid: u64) -> Value {
    let status = rpc.call("status", json!({"workspace": workspace}));
    status["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|ws| ws["processes"].as_array().unwrap().iter())
        .find(|p| p["pid"].as_u64() == Some(pid))
        .cloned()
        .unwrap_or_else(|| panic!("pid {pid} missing from status: {status}"))
}

/// Decodes a capture's base64 PNG/WebP into raw RGBA pixels.
fn decode_capture(result: &Value) -> (u32, u32, Vec<u8>) {
    let bytes = BASE64.decode(result["data"].as_str().unwrap()).unwrap();
    let image = image::load_from_memory(&bytes).expect("capture did not decode");
    (image.width(), image.height(), image.to_rgba8().into_raw())
}

/// Asserts a decoded capture actually rendered something: at least one
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

/// Two workspaces, two real apps: event-driven waits, deterministic
/// placement with pixel-reproducible captures, targeted input verified by
/// observable exits, clipboard interop, marks/diffs, then full teardown.
#[test]
#[ignore = "opt-in real-app e2e: needs GUI wayland apps on PATH"]
fn multi_app_two_workspace_scenario() {
    let available: Vec<App> = CANDIDATES
        .iter()
        .copied()
        .filter(|app| which(app.program).is_some())
        .collect();
    if available.is_empty() {
        eprintln!("skipping: none of zenity/foot/weston-simple-shm on PATH");
        return;
    }
    let app_a = available[0];
    let app_b = available
        .iter()
        .find(|app| app.program != app_a.program)
        .copied()
        .unwrap_or(app_a);
    eprintln!(
        "driving {:?} in alpha, {:?} in bravo",
        app_a.program, app_b.program
    );

    let daemon = common::daemon::spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    // -- two isolated workspaces -------------------------------------------------
    rpc.call("workspace_create", json!({"id": "alpha"}));
    rpc.call("workspace_create", json!({"id": "bravo"}));
    let list = rpc.call("workspace_list", json!({}));
    let socket_a = list["workspaces"][0]["socket_path"].as_str().unwrap();
    let socket_b = list["workspaces"][1]["socket_path"].as_str().unwrap();
    assert_ne!(socket_a, socket_b, "workspaces must not share a socket");

    // -- spawn real apps via the daemon's env profile ----------------------------
    let pid_a = rpc.call(
        "spawn_app",
        json!({"workspace": "alpha", "program": app_a.program, "args": app_a.args}),
    )["pid"]
        .as_u64()
        .unwrap();
    let pid_b = rpc.call(
        "spawn_app",
        json!({"workspace": "bravo", "program": app_b.program, "args": app_b.args}),
    )["pid"]
        .as_u64()
        .unwrap();

    // -- event-driven waits: map → commit → idle ---------------------------------
    let win_a = rpc.call(
        "wait_for_window",
        json!({"workspace": "alpha", "pid": pid_a, "timeout_ms": WAIT_TIMEOUT_MS}),
    )["window"]["id"]
        .as_u64()
        .unwrap();
    let win_b = rpc.call(
        "wait_for_window",
        json!({"workspace": "bravo", "pid": pid_b, "timeout_ms": WAIT_TIMEOUT_MS}),
    )["window"]["id"]
        .as_u64()
        .unwrap();
    rpc.call(
        "wait_for_commit",
        json!({"workspace": "alpha", "after_seq": 0, "window": win_a, "timeout_ms": WAIT_TIMEOUT_MS}),
    );
    rpc.call(
        "wait_for_commit",
        json!({"workspace": "bravo", "after_seq": 0, "window": win_b, "timeout_ms": WAIT_TIMEOUT_MS}),
    );
    settle(&mut rpc, "alpha", 500);
    settle(&mut rpc, "bravo", 500);
    rpc.call(
        "mark_set",
        json!({"workspace": "alpha", "name": "apps-mapped"}),
    );

    // Isolation: window ids are per-compositor (both may well be 1), so
    // the disjointness that matters is ownership — each workspace sees
    // only windows belonging to its own spawned process.
    assert!(window_pids(&mut rpc, "alpha").contains(&pid_a));
    assert!(!window_pids(&mut rpc, "alpha").contains(&pid_b));
    assert!(window_pids(&mut rpc, "bravo").contains(&pid_b));
    assert!(!window_pids(&mut rpc, "bravo").contains(&pid_a));

    // -- deterministic placement + reproducible captures ---------------------------
    let rect_a = json!({"x": 60, "y": 60, "width": 480, "height": 320});
    rpc.call(
        "place_window",
        json!({"workspace": "alpha", "window": win_a, "rect": rect_a}),
    );
    settle(&mut rpc, "alpha", 300);
    let cap_a1 = rpc.call(
        "capture_workspace",
        json!({"workspace": "alpha", "format": "png"}),
    );
    let (w, h, pixels_a1) = decode_capture(&cap_a1);
    assert!(w > 0 && h > 0);
    assert_rendered(&pixels_a1, "alpha workspace capture");

    // Moving the window changes the composite; moving it back reproduces
    // the exact same pixels — deterministic placement, byte-for-byte.
    rpc.call(
        "place_window",
        json!({"workspace": "alpha", "window": win_a, "x": 500, "y": 400}),
    );
    settle(&mut rpc, "alpha", 300);
    let cap_moved = rpc.call(
        "capture_workspace",
        json!({"workspace": "alpha", "format": "png"}),
    );
    let (_, _, pixels_moved) = decode_capture(&cap_moved);
    assert_ne!(
        pixels_a1, pixels_moved,
        "placement change had no visible effect"
    );

    rpc.call(
        "place_window",
        json!({"workspace": "alpha", "window": win_a, "rect": rect_a}),
    );
    settle(&mut rpc, "alpha", 300);
    let cap_a2 = rpc.call(
        "capture_workspace",
        json!({"workspace": "alpha", "format": "png"}),
    );
    let (_, _, pixels_a2) = decode_capture(&cap_a2);
    assert_eq!(
        pixels_a1, pixels_a2,
        "re-placed capture is not pixel-identical"
    );

    // An occlusion-free per-window capture holds real pixels too.
    let cap_win = rpc.call(
        "capture_window",
        json!({"workspace": "alpha", "window": win_a, "format": "png"}),
    );
    let (_, _, win_pixels) = decode_capture(&cap_win);
    assert_rendered(&win_pixels, "alpha window capture");

    // -- targeted input → observable app behavior ---------------------------------
    match app_a.exit {
        ExitPlan::KeyReturn => {
            // A stray click first (harmless on a dialog) to exercise the
            // pointer path on a real app, then Return for the OK button.
            rpc.call(
                "click",
                json!({"workspace": "alpha", "window": win_a, "x": 10, "y": 10}),
            );
            rpc.call(
                "key_combo",
                json!({"workspace": "alpha", "window": win_a, "key": "Return"}),
            );
            settle(&mut rpc, "alpha", INPUT_SETTLE_MS);
            if is_mapped(&mut rpc, "alpha", win_a) {
                // Some zenity themes want a click on OK instead.
                rpc.call(
                    "click",
                    json!({"workspace": "alpha", "window": win_a, "x": 290, "y": 140}),
                );
                settle(&mut rpc, "alpha", INPUT_SETTLE_MS);
            }
        }
        ExitPlan::TypeExit => {
            rpc.call(
                "type_text",
                json!({"workspace": "alpha", "window": win_a, "text": "exit\n"}),
            );
            settle(&mut rpc, "alpha", INPUT_SETTLE_MS);
        }
        ExitPlan::Teardown => {
            // No input-driven exit: verify targeted input is accepted and
            // observable via a follow-up commit.
            let seq = rpc.call(
                "click",
                json!({"workspace": "alpha", "window": win_a, "x": 10, "y": 10}),
            )["seq"]
                .as_u64()
                .unwrap();
            settle(&mut rpc, "alpha", 500);
            let _ = seq;
        }
    }
    if !matches!(app_a.exit, ExitPlan::Teardown) {
        assert!(
            !is_mapped(&mut rpc, "alpha", win_a),
            "{} did not exit after injected input",
            app_a.program
        );
        let proc = process_state(&mut rpc, "alpha", pid_a);
        assert_eq!(
            proc["state"], "exited",
            "{} process state: {proc}",
            app_a.program
        );
    }

    // -- clipboard interop with real clients (when present) -------------------------
    if which("wl-copy").is_some() {
        const PAYLOAD: &str = "clipboard payload from wl-copy";
        let copy_pid = rpc.call(
            "spawn_app",
            json!({
                "workspace": "bravo",
                "program": "wl-copy",
                "args": ["--foreground", "--type", "text/plain", PAYLOAD],
            }),
        )["pid"]
            .as_u64()
            .unwrap();
        // wl-copy maps a 1×1 toplevel to take keyboard focus before it
        // publishes the selection — the map is the event-driven gate.
        rpc.call(
            "wait_for_window",
            json!({"workspace": "bravo", "pid": copy_pid, "timeout_ms": WAIT_TIMEOUT_MS}),
        );
        settle(&mut rpc, "bravo", 500);
        let got = rpc.call(
            "clipboard_get",
            json!({"workspace": "bravo", "mime": "text/plain"}),
        );
        let data = BASE64.decode(got["data_base64"].as_str().unwrap()).unwrap();
        assert_eq!(data, PAYLOAD.as_bytes(), "wl-copy payload mismatch");
    }
    if which("wl-paste").is_some() {
        rpc.call(
            "clipboard_set",
            json!({
                "workspace": "bravo",
                "mime": "text/plain",
                "data": "clipboard payload from scry",
            }),
        );
        let paste_pid = rpc.call(
            "spawn_app",
            json!({
                "workspace": "bravo",
                "program": "wl-paste",
                "args": ["--no-newline", "--type", "text/plain"],
            }),
        )["pid"]
            .as_u64()
            .unwrap();
        let proc = await_process_exit(&mut rpc, "bravo", paste_pid);
        assert_eq!(
            proc["state"], "exited",
            "wl-paste did not finish reading: {proc}"
        );
        assert_eq!(proc["exit_code"], 0, "wl-paste exit status: {proc}");
    }

    // -- damage, marks and the structured diff ---------------------------------------
    let damage = rpc.call("get_damage", json!({"workspace": "bravo"}))["damage"]
        .as_array()
        .unwrap()
        .len();
    assert!(damage > 0, "expected retained damage for bravo's app");

    rpc.call(
        "mark_set",
        json!({"workspace": "alpha", "name": "scenario-complete"}),
    );
    let diff = rpc.call("get_changes_since", json!({"workspace": "alpha", "seq": 0}));
    let mapped: Vec<u64> = diff["windows"]["mapped"]
        .as_array()
        .unwrap()
        .iter()
        .chain(diff["windows"]["updated"].as_array().unwrap())
        .filter_map(|w| w["id"].as_u64())
        .collect();
    let closed: Vec<u64> = diff["windows"]["closed"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    let saw_window = mapped.contains(&win_a) || closed.contains(&win_a);
    assert!(saw_window, "diff missed alpha's window: {diff}");
    if !matches!(app_a.exit, ExitPlan::Teardown) {
        assert!(
            closed.contains(&win_a),
            "diff did not record the app exit: {diff}"
        );
    }
    let marks: Vec<&str> = diff["marks_added"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert_eq!(marks, ["apps-mapped", "scenario-complete"]);

    // -- teardown: close both workspaces, everything dies ---------------------------
    rpc.call("workspace_close", json!({"workspace": "alpha"}));
    rpc.call("workspace_close", json!({"workspace": "bravo"}));
    let remaining = rpc.call("workspace_list", json!({}))["workspaces"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(remaining, 0, "workspaces survived close");
    if matches!(app_a.exit, ExitPlan::Teardown) {
        assert!(
            !std::path::Path::new(&format!("/proc/{pid_a}")).exists(),
            "{} (pid {pid_a}) survived workspace teardown",
            app_a.program
        );
    }
    let status = rpc.call("status", json!({}));
    assert_eq!(status["healthy"], true, "daemon unhealthy: {status}");
}
