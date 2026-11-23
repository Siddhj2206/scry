//! Integration tests for the compiled `scry` CLI: a thin stateless proxy
//! driven against an isolated daemon control socket + state directory
//! (issue #18).
//!
//! The seam under test is the real binary (`CARGO_BIN_EXE_scry`) issuing
//! real JSON-RPC calls to a real daemon; workspace-side effects are
//! observed through a `wayland-client` test client, never through daemon
//! internals.

mod common;

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use serde_json::Value;

/// Runs `scry --socket <socket> <args>` and captures the result.
fn run(socket: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .expect("failed to run scry")
}

/// Runs `scry --socket <socket> --json <args>`, asserting exit 0 and
/// parsing stdout as JSON.
fn run_json(socket: &Path, args: &[&str]) -> Value {
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
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "scry --json {args:?} stdout is not JSON ({err}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Creates `id` through the CLI and connects a test client to the
/// workspace's own Wayland socket.
fn create_workspace_client(socket: &Path, id: &str) -> (common::TestClient, PathBuf) {
    let created = run_json(socket, &["workspace", "create", id]);
    let ws_socket = PathBuf::from(created["workspace"]["socket_path"].as_str().unwrap());
    (common::TestClient::connect(&ws_socket), ws_socket)
}

// -- lifecycle & output modes ---------------------------------------------------

/// `workspace create/list/close` work in both human and `--json` modes,
/// create is idempotent, conflicting options fail, and errors carry
/// non-zero exit codes with stderr diagnostics.
#[test]
fn cli_workspace_lifecycle_human_and_json() {
    let daemon = common::daemon::spawn_daemon();
    let socket = &daemon.socket_path;

    // Human output.
    let out = run(socket, &["workspace", "create", "alpha"]);
    assert!(out.status.success(), "create failed: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("alpha"), "human output: {text}");
    assert!(text.contains("created"), "human output: {text}");

    // Idempotent re-create via JSON: `created` flips to false.
    let result = run_json(socket, &["workspace", "create", "alpha"]);
    assert_eq!(result["created"], false);
    assert_eq!(result["workspace"]["id"], "alpha");
    assert_eq!(result["command_id"], 1);

    // Conflicting creation options are an explicit error.
    let out = run(socket, &["workspace", "create", "alpha", "--read-only"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("different creation options"),
        "stderr: {}",
        stderr(&out)
    );

    // List shows the workspace in both modes.
    let out = run(socket, &["workspace", "list"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("alpha"));
    let result = run_json(socket, &["workspace", "list"]);
    assert_eq!(result["workspaces"].as_array().unwrap().len(), 1);

    // Close, then the workspace is gone and a repeat close errors.
    let out = run(socket, &["workspace", "close", "alpha"]);
    assert!(out.status.success(), "close failed: {}", stderr(&out));
    assert!(stdout(&out).contains("closed"));
    let out = run(socket, &["workspace", "list"]);
    assert!(stdout(&out).contains("no workspaces"));
    let out = run(socket, &["workspace", "close", "alpha"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown workspace"));
}

// -- window, placement, capture, output & damage verbs ----------------------------

/// A real wayland-client window is placed, captured and inspected purely
/// through CLI verbs, with `wait idle` providing the synchronization.
#[test]
fn cli_window_placement_capture_output_damage() {
    let daemon = common::daemon::spawn_daemon();
    let socket = &daemon.socket_path;
    let dir = tempfile::tempdir().unwrap();
    let (mut client, _ws_socket) = create_workspace_client(socket, "win");

    let toplevel = client.map_toplevel(200, 150, "test-app", "Test Window");
    let pid = std::process::id();

    // wait window resolves the map event-driven through the CLI.
    let result = run_json(
        socket,
        &[
            "wait",
            "window",
            "-w",
            "win",
            "--pid",
            &pid.to_string(),
            "--timeout-ms",
            "10000",
        ],
    );
    let window_id = result["window"]["id"].as_u64().unwrap();
    assert_eq!(result["window"]["app_id"], "test-app");

    // windows lists it, human and JSON.
    let out = run(socket, &["windows", "-w", "win"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("test-app"));
    let result = run_json(socket, &["windows", "-w", "win"]);
    assert_eq!(result["windows"][0]["id"], window_id);

    // place: deterministic position + resize.
    let out = run(
        socket,
        &[
            "place",
            "-w",
            "win",
            "--window",
            &window_id.to_string(),
            "--x",
            "40",
            "--y",
            "30",
            "--width",
            "640",
            "--height",
            "400",
        ],
    );
    assert!(out.status.success(), "place failed: {}", stderr(&out));
    // The client acks the resize configure and commits a matching buffer
    // before the new geometry is observable.
    client.pump_until(|s| {
        s.toplevel_configures
            .iter()
            .any(|&(w, h)| w == 640 && h == 400)
    });
    let resized = client.shm_buffer(640, 400, 0xff33_6699);
    toplevel.surface.attach(Some(&resized), 0, 0);
    toplevel.surface.commit();
    client.roundtrip();
    run_json(socket, &["wait", "idle", "-w", "win", "--quiet-ms", "100"]);
    let result = run_json(socket, &["windows", "-w", "win"]);
    let geo = &result["windows"][0]["geometry"];
    assert_eq!(
        (
            geo["x"].as_i64(),
            geo["y"].as_i64(),
            geo["width"].as_i64(),
            geo["height"].as_i64()
        ),
        (Some(40), Some(30), Some(640), Some(400)),
        "placed geometry: {geo}"
    );

    // capture-window writes a real PNG file.
    let window_png = dir.path().join("window.png");
    let out = run(
        socket,
        &[
            "capture-window",
            "-w",
            "win",
            "--window",
            &window_id.to_string(),
            "--out",
            window_png.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "capture-window: {}", stderr(&out));
    assert!(stdout(&out).contains("captured window"));
    let png = std::fs::read(&window_png).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG: {png:?}");

    // capture-workspace on the default output.
    let ws_png = dir.path().join("workspace.png");
    let out = run(
        socket,
        &[
            "capture-workspace",
            "-w",
            "win",
            "--out",
            ws_png.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "capture-workspace: {}", stderr(&out));
    let png = std::fs::read(&ws_png).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");

    // damage reports the window's committed regions.
    let result = run_json(socket, &["damage", "-w", "win"]);
    assert!(
        !result["damage"].as_array().unwrap().is_empty(),
        "expected retained damage entries"
    );

    // output add/list/remove round-trip.
    let out = run(
        socket,
        &[
            "output", "add", "-w", "win", "--name", "EXTRA", "--width", "800", "--height", "600",
        ],
    );
    assert!(out.status.success(), "output add: {}", stderr(&out));
    let result = run_json(socket, &["output", "list", "-w", "win"]);
    let names: Vec<_> = result["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"EXTRA"), "outputs: {names:?}");
    let out = run(
        socket,
        &["output", "remove", "-w", "win", "--name", "EXTRA"],
    );
    assert!(out.status.success(), "output remove: {}", stderr(&out));

    let out = run(socket, &["workspace", "close", "win"]);
    assert!(out.status.success());
}

// -- input & clipboard verbs ------------------------------------------------------

/// `click`, `drag`, `type`, `key` deliver real seat events to a connected
/// client; `clipboard set/get` round-trips through the compositor.
#[test]
fn cli_input_and_clipboard_verbs() {
    let daemon = common::daemon::spawn_daemon();
    let socket = &daemon.socket_path;
    let (mut client, _ws_socket) = create_workspace_client(socket, "in");
    let _toplevel = client.map_toplevel(200, 150, "input-app", "Input");

    let pid = std::process::id();
    let result = run_json(
        socket,
        &[
            "wait",
            "window",
            "-w",
            "in",
            "--pid",
            &pid.to_string(),
            "--timeout-ms",
            "10000",
        ],
    );
    let window_id = result["window"]["id"].as_u64().unwrap();
    let wid = window_id.to_string();

    // click lands pointer enter + button events on the client's surface.
    let out = run(
        socket,
        &[
            "click", "-w", "in", "--window", &wid, "--x", "5", "--y", "5",
        ],
    );
    assert!(out.status.success(), "click: {}", stderr(&out));
    client.pump_until(|s| !s.pointer_buttons.is_empty());
    assert_eq!(client.state.pointer_buttons[0].1, 1, "expected a press");

    // drag emits motion between press and release.
    let out = run(
        socket,
        &[
            "drag",
            "-w",
            "in",
            "--from",
            "1,1",
            "--to",
            "40,40",
            "--from-window",
            &wid,
            "--to-window",
            &wid,
            "--steps",
            "4",
        ],
    );
    assert!(out.status.success(), "drag: {}", stderr(&out));
    client.pump_until(|s| s.pointer_motions.len() >= 2);

    // type delivers decodable text through the seat keymap.
    let out = run(socket, &["type", "-w", "in", "--window", &wid, "hi"]);
    assert!(out.status.success(), "type: {}", stderr(&out));
    client.pump_until(|s| s.typed_text().contains("hi"));

    // key delivers a named key.
    let before = client.state.keys.len();
    let out = run(socket, &["key", "-w", "in", "--window", &wid, "Tab"]);
    assert!(out.status.success(), "key: {}", stderr(&out));
    client.pump_until(|s| s.keys.len() > before);

    // clipboard set then get round-trips the payload (raw bytes on stdout).
    let out = run(socket, &["clipboard", "set", "-w", "in", "cli-clipboard"]);
    assert!(out.status.success(), "clipboard set: {}", stderr(&out));
    let out = run(socket, &["clipboard", "get", "-w", "in"]);
    assert!(out.status.success(), "clipboard get: {}", stderr(&out));
    assert_eq!(out.stdout, b"cli-clipboard");
}

// -- marks, diffs, waits & status ---------------------------------------------------

/// `mark`, `changes`, `wait` and `status` verbs behave in both output
/// modes, and a blocking `wait commit` is unblocked by a real client
/// commit rather than any polling.
#[test]
fn cli_marks_diffs_status_and_event_driven_waits() {
    let daemon = common::daemon::spawn_daemon();
    let socket = &daemon.socket_path;
    let (mut client, _ws_socket) = create_workspace_client(socket, "obs");
    let toplevel = client.map_toplevel(200, 150, "obs-app", "Observer");

    // marks
    let out = run(socket, &["mark", "set", "-w", "obs", "phase-a"]);
    assert!(out.status.success(), "mark set: {}", stderr(&out));
    assert!(stdout(&out).contains("phase-a"));
    let result = run_json(socket, &["mark", "list", "-w", "obs"]);
    assert_eq!(result["marks"][0]["name"], "phase-a");

    // changes covers the window's map + commit lifecycle.
    let result = run_json(socket, &["changes", "-w", "obs", "0"]);
    let mapped = result["windows"]["mapped"].as_array().unwrap();
    assert!(
        mapped.iter().any(|w| w["app_id"] == "obs-app"),
        "diff missing mapped window: {result}"
    );

    // status: human narrative + JSON health fields.
    let out = run(socket, &["status"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("obs"), "status: {}", stdout(&out));
    let result = run_json(socket, &["status"]);
    assert_eq!(result["healthy"], true);
    assert_eq!(result["daemon"]["workspace_count"], 1);

    // wait commit --after-seq 0 resolves immediately (commits happened).
    let out = run(socket, &["wait", "commit", "-w", "obs", "--after-seq", "0"]);
    assert!(out.status.success(), "wait commit: {}", stderr(&out));

    // A blocking `wait commit` started before the commit resolves on the
    // event — not on any timer or poll.
    let before = run_json(socket, &["windows", "-w", "obs"])["seq"]
        .as_u64()
        .unwrap();
    let waiting = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args([
            "wait",
            "commit",
            "-w",
            "obs",
            "--after-seq",
            &before.to_string(),
            "--timeout-ms",
            "5000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn scry wait");
    // The client commits a fresh buffer: the pending wait must resolve.
    client.roundtrip();
    toplevel.surface.attach(Some(&toplevel.buffer), 0, 0);
    toplevel.surface.commit();
    client.roundtrip();
    let out = waiting
        .wait_with_output()
        .expect("scry wait commit did not exit");
    assert!(out.status.success(), "wait commit: {}", stderr(&out));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(result["seq"].as_u64().unwrap() > before);

    // wait idle settles.
    let out = run(socket, &["wait", "idle", "-w", "obs", "--quiet-ms", "50"]);
    assert!(out.status.success(), "wait idle: {}", stderr(&out));

    let out = run(socket, &["workspace", "close", "obs"]);
    assert!(out.status.success());
}

// -- error paths ---------------------------------------------------------------------

/// Bad targets and bounded waits produce non-zero exits with diagnostics.
#[test]
fn cli_error_paths_have_exit_codes_and_stderr() {
    let daemon = common::daemon::spawn_daemon();
    let socket = &daemon.socket_path;

    // Unknown workspace.
    let out = run(socket, &["windows", "-w", "ghost"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("unknown workspace"));

    // The JSON-RPC error is structured in --json mode too (on stderr).
    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args(["windows", "-w", "ghost"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], -32001);

    // A bounded wait times out with a non-zero exit.
    run_json(socket, &["workspace", "create", "w"]);
    let out = run(
        socket,
        &[
            "wait",
            "window",
            "-w",
            "w",
            "--app-id",
            "never-maps",
            "--timeout-ms",
            "250",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("timed out"));

    // CLI-side validation: --width without --height.
    let out = run(
        socket,
        &[
            "place", "-w", "w", "--window", "1", "--x", "0", "--y", "0", "--width", "100",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--width and --height"));

    // Clap's own usage errors also exit non-zero.
    let out = run(socket, &["click", "-w", "w"]);
    assert_eq!(out.status.code(), Some(2));
}

// -- the daemon subcommand -------------------------------------------------------------

/// `scry daemon` serves in the foreground on explicit paths, prints a
/// readiness line, and SIGTERM stops it cleanly (socket file removed).
#[test]
fn cli_daemon_subcommand_serves_and_shuts_down() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("run/scry.sock");
    let state_dir = dir.path().join("state");

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("daemon")
        .arg("--socket")
        .arg(&socket_path)
        .arg("--state-dir")
        .arg(&state_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn scry daemon");

    // Readiness is the first stdout line — no polling needed.
    let mut line = String::new();
    BufReader::new(daemon.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .expect("daemon stdout closed before readiness");
    assert!(
        line.contains("listening on"),
        "unexpected readiness line: {line}"
    );
    assert!(socket_path.exists());

    // The daemon answers client commands on that socket.
    let result = run_json(&socket_path, &["workspace", "create", "d"]);
    assert_eq!(result["created"], true);
    let out = run(&socket_path, &["status"]);
    assert!(out.status.success());

    // SIGTERM → clean shutdown, socket file removed.
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let status = daemon.wait().expect("daemon did not exit");
    assert!(status.success(), "daemon exited with {status}");
    assert!(!socket_path.exists(), "stale control socket left behind");
}

// -- statelessness & auto-start ---------------------------------------------------------

/// Two separate CLI invocations share one daemon's state (the CLI holds
/// none), the default socket auto-starts the daemon, and an explicit
/// `--socket` fails fast instead of spawning anything.
#[test]
fn cli_is_stateless_and_autostarts_on_default_socket() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("xdg-runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    let state_dir = dir.path().join("state");

    let with_env = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_scry"))
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("SCRY_STATE_DIR", &state_dir)
            .args(args)
            .output()
            .expect("failed to run scry")
    };

    // First invocation on the default socket auto-starts the daemon.
    let out = with_env(&["workspace", "create", "persisted"]);
    assert!(out.status.success(), "autostart create: {}", stderr(&out));

    // A second, separate invocation sees the same daemon state — the CLI
    // itself remembered nothing.
    let out = with_env(&["--json", "workspace", "list"]);
    assert!(out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<_> = result["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"persisted"), "workspaces: {ids:?}");

    // The auto-started daemon is a real process we can stop via its pid.
    let out = with_env(&["--json", "status"]);
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    let daemon_pid = result["daemon"]["pid"].as_u64().unwrap() as i32;
    let socket_path = runtime_dir.join("scry/control.sock");
    unsafe { libc::kill(daemon_pid, libc::SIGTERM) };
    // Bounded wait for the socket file to disappear.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while socket_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket_path.exists(), "daemon did not shut down");

    // An explicit --socket never auto-starts: it fails with exit 3.
    let out = run(&dir.path().join("nonexistent.sock"), &["status"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).contains("cannot connect to daemon"));
}
