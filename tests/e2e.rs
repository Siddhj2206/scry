//! Real-application end-to-end tests: unmodified Wayland clients driven
//! against the live `WAYLAND_DISPLAY` socket + `CompositorHandle` seam.
//!
//! Covers issue #8: a real GTK4 app (`zenity` with `GSK_RENDERER=cairo`),
//! real `wl-copy`/`wl-paste` clipboard round-trips, and `weston-simple-shm`
//! / `foot` frame-clock, input and capture checks.
//!
//! Every test is `#[ignore]`d — opt in with:
//!
//!     cargo test --test e2e -- --ignored --nocapture
//!
//! Tests skip cleanly when a client binary is absent from `PATH`, so the
//! suite runs both on developer hosts and in the `Containerfile` `e2e` stage.
//! All synchronization is event- or EOF-driven — no `sleep()` anywhere.

mod common;

use std::{
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::Receiver,
    thread,
    time::{Duration, Instant},
};

use scry::{Compositor, CompositorEvent, CompositorHandle, FrameClockMode, SelectionKind};

/// How long a real app gets to start up, map and answer. Cold font caches
/// and first-run toolkit initialization make this much looser than the
/// synthetic-client `common::TIMEOUT`.
const APP_TIMEOUT: Duration = Duration::from_secs(30);

const TEXT_MIME: &str = "text/plain";

// -- helpers -----------------------------------------------------------------

/// Finds `program` on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// Waits for a compositor event matching `pred`, bounded by `timeout`.
fn wait_event(
    events: &Receiver<CompositorEvent>,
    timeout: Duration,
    what: &str,
    pred: impl Fn(&CompositorEvent) -> bool,
) -> CompositorEvent {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for {what}"));
        let ev = events
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        if pred(&ev) {
            return ev;
        }
    }
}

/// A spawned real Wayland client.
///
/// stdout and stderr are piped and drained on background threads; the
/// receivers deliver the captured bytes once the process exits (pipe EOF),
/// which gives an event-driven way to await termination — no polling, no
/// sleeps. Dropping kills a still-running process so tests never leak apps.
struct ClientProcess {
    child: Child,
    stdout: Receiver<Vec<u8>>,
    stderr: Receiver<Vec<u8>>,
}

impl ClientProcess {
    /// Spawns `program` with `args` pointed at the compositor's socket.
    ///
    /// `XDG_RUNTIME_DIR` + `WAYLAND_DISPLAY` locate the socket; `DISPLAY` and
    /// the host session-bus address are removed so clients can only ever
    /// reach this headless instance. `extra_env` adds per-app variables such
    /// as `GSK_RENDERER=cairo`.
    fn spawn(
        handle: &CompositorHandle,
        program: PathBuf,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Self {
        let mut command = Command::new(&program);
        command
            .args(args)
            .env("XDG_RUNTIME_DIR", handle.runtime_dir())
            .env("WAYLAND_DISPLAY", handle.socket_name())
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {}: {err}", program.display()));

        fn drain(pipe: impl std::io::Read + Send + 'static) -> Receiver<Vec<u8>> {
            let (tx, rx) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let mut pipe = pipe;
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
            rx
        }
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the process to exit (pipe EOF), returning its exit status
    /// plus everything it printed. Panics on timeout after killing it.
    fn wait_exit(&mut self, timeout: Duration, what: &str) -> (ExitStatus, Vec<u8>, Vec<u8>) {
        let Ok(stderr) = self.stderr.recv_timeout(timeout) else {
            self.child.kill().ok();
            panic!("timed out waiting for {what} to exit");
        };
        // The stdout reader EOFs at the same time; a short re-wait keeps the
        // assertion total without re-sleeping.
        let stdout = self
            .stdout
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default();
        let status = self.child.wait().expect("wait failed");
        (status, stdout, stderr)
    }
}

impl Drop for ClientProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Asserts a captured frame actually rendered something: at least one
/// opaque pixel and not a single flat color.
fn assert_rendered(frame: &scry::Frame, what: &str) {
    assert_eq!(
        frame.pixels.len(),
        (frame.width * frame.height * 4) as usize,
        "{what}: pixel buffer size mismatch"
    );
    let (pixels, _rest) = frame.pixels.as_chunks::<4>();
    assert!(
        pixels.iter().any(|px| px[3] > 0),
        "{what}: every pixel is fully transparent"
    );
    let first = pixels[0];
    assert!(
        pixels.iter().any(|px| *px != first),
        "{what}: capture is a single flat color — nothing was rendered"
    );
}

// -- GTK4 (zenity) -------------------------------------------------------------

/// A real unmodified GTK4 app maps, renders actual pixels, reports its
/// `SO_PEERCRED` pid and closes cleanly through `xdg_toplevel.close`.
#[test]
#[ignore = "opt-in real-app e2e: needs zenity (GTK4) on PATH"]
fn zenity_gtk4_maps_renders_reports_pid_and_closes() {
    let Some(zenity) = which("zenity") else {
        eprintln!("skipping: zenity not on PATH");
        return;
    };

    let compositor = Compositor::spawn().expect("compositor failed to start");
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut app = ClientProcess::spawn(
        &handle,
        zenity,
        &[
            "--info",
            "--text",
            "scry e2e",
            "--width=320",
            "--height=160",
        ],
        &[("GSK_RENDERER", "cairo"), ("GDK_BACKEND", "wayland")],
    );
    let app_pid = app.id();

    // The dialog maps: WindowMapped carries the client's real pid.
    let ev = wait_event(&events, APP_TIMEOUT, "zenity WindowMapped", |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        window.pid,
        Some(app_pid),
        "SO_PEERCRED pid should be the zenity process"
    );

    // The rendered buffer commit follows the map on the same event stream.
    wait_event(
        &events,
        APP_TIMEOUT,
        "zenity SurfaceCommitted",
        |e| matches!(e, CompositorEvent::SurfaceCommitted { window_id, .. } if *window_id == Some(window.id)),
    );

    // Capture is occlusion-free and holds real rendered pixels.
    let frame = handle.capture_window(window.id).unwrap().value;
    assert_rendered(&frame, "zenity capture");

    // A protocol-level close is honored and the app exits on its own.
    handle.close_window(window.id).expect("close_window failed");
    wait_event(
        &events,
        APP_TIMEOUT,
        "zenity WindowClosed",
        |e| matches!(e, CompositorEvent::WindowClosed { window_id, .. } if *window_id == window.id),
    );
    let (status, _stdout, stderr) = app.wait_exit(APP_TIMEOUT, "zenity");
    if !status.success() {
        eprintln!(
            "zenity exited with {status} (stderr: {})",
            String::from_utf8_lossy(&stderr)
        );
    }

    // The compositor itself shuts down cleanly afterwards.
    handle.shutdown().expect("shutdown failed");
}

// -- wl-clipboard ---------------------------------------------------------------

/// `wl-copy` publishes a selection owned by a real client and
/// `handle.read_selection` reads it back; `handle.set_selection` then feeds
/// the compositor's own selection to a real `wl-paste`.
///
/// wl-clipboard has no surface to focus, so on compositors without
/// `zwlr_data_control_manager_v1` it maps a 1×1 toplevel to take keyboard
/// focus (scry focuses newly mapped toplevels) before setting/reading the
/// selection — this exercises the focus-driven offer path end to end.
#[test]
#[ignore = "opt-in real-app e2e: needs wl-copy and wl-paste on PATH"]
fn wl_copy_wl_paste_clipboard_roundtrip() {
    let (Some(wl_copy), Some(wl_paste)) = (which("wl-copy"), which("wl-paste")) else {
        eprintln!("skipping: wl-copy/wl-paste not on PATH");
        return;
    };

    let compositor = Compositor::spawn().expect("compositor failed to start");
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    // Direction 1: real client → compositor.
    const PAYLOAD: &str = "clipboard payload from wl-copy";
    let mut copy = ClientProcess::spawn(
        &handle,
        wl_copy,
        &["--foreground", "--type", TEXT_MIME, PAYLOAD],
        &[],
    );

    wait_event(&events, APP_TIMEOUT, "wl-copy ClipboardUpdated", |e| {
        matches!(
            e,
            CompositorEvent::ClipboardUpdated {
                selection: SelectionKind::Clipboard,
                mime_types,
                ..
            } if mime_types.iter().any(|m| m == TEXT_MIME)
        )
    });

    let reply = handle
        .read_selection(SelectionKind::Clipboard, TEXT_MIME.to_string())
        .expect("read_selection failed");
    assert_eq!(reply.value, PAYLOAD.as_bytes());

    // Direction 2: compositor → real client. Replacing the selection
    // cancels wl-copy's source, so its --foreground process exits.
    let compositor_payload = b"clipboard payload from scry";
    handle
        .set_selection(
            SelectionKind::Clipboard,
            TEXT_MIME.to_string(),
            compositor_payload.to_vec(),
        )
        .expect("set_selection failed");
    let _ = copy.wait_exit(APP_TIMEOUT, "wl-copy");

    let mut paste = ClientProcess::spawn(
        &handle,
        wl_paste,
        &["--no-newline", "--type", TEXT_MIME],
        &[],
    );
    let (_status, stdout, _stderr) = paste.wait_exit(APP_TIMEOUT, "wl-paste");
    assert_eq!(stdout, compositor_payload);

    handle.shutdown().expect("shutdown failed");
}

// -- shm clients (weston-simple-shm / foot) -------------------------------------

/// `weston-simple-shm` is an unmodified Wayland client that redraws
/// exclusively on `wl_surface.frame` callbacks — the perfect probe for the
/// virtual frame clock: once paused, commits stop, and each `step` produces
/// exactly the next redraw commit.
#[test]
#[ignore = "opt-in real-app e2e: needs weston-simple-shm on PATH"]
fn weston_simple_shm_commits_on_frame_clock_steps() {
    let Some(shm_client) = which("weston-simple-shm") else {
        eprintln!("skipping: weston-simple-shm not on PATH");
        return;
    };

    let compositor = Compositor::spawn().expect("compositor failed to start");
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();
    handle
        .set_frame_clock_mode(FrameClockMode::Paused)
        .expect("pausing frame clock failed");

    let mut app = ClientProcess::spawn(&handle, shm_client, &[], &[]);

    let ev = wait_event(&events, APP_TIMEOUT, "simple-shm WindowMapped", |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };

    let committed = |e: &CompositorEvent| matches!(e, CompositorEvent::SurfaceCommitted { window_id, .. } if *window_id == Some(window.id));

    // Initial draw commits without needing a frame callback.
    wait_event(&events, APP_TIMEOUT, "initial simple-shm commit", committed);

    // With the clock paused each step releases exactly the pending frame
    // callback, so the next real-app commit is step-driven — deterministic.
    for step in 0..3 {
        handle
            .step_frame_clock(Duration::from_millis(16))
            .expect("frame clock step failed");
        wait_event(&events, APP_TIMEOUT, "stepped simple-shm commit", committed);
        eprintln!("step {step} produced a simple-shm commit");
    }

    let frame = handle.capture_window(window.id).unwrap().value;
    assert_rendered(&frame, "weston-simple-shm capture");

    app.child.kill().ok();
    let _ = app.wait_exit(APP_TIMEOUT, "weston-simple-shm");
    wait_event(
        &events,
        APP_TIMEOUT,
        "simple-shm WindowClosed",
        |e| matches!(e, CompositorEvent::WindowClosed { window_id, .. } if *window_id == window.id),
    );
    handle.shutdown().expect("shutdown failed");
}

/// `foot` is a full terminal emulator: it maps, renders, and answers
/// synthetic text input with real redraw commits the test can capture.
#[test]
#[ignore = "opt-in real-app e2e: needs foot on PATH"]
fn foot_maps_captures_and_receives_synthetic_input() {
    let Some(foot) = which("foot") else {
        eprintln!("skipping: foot not on PATH");
        return;
    };

    let compositor = Compositor::spawn().expect("compositor failed to start");
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut app = ClientProcess::spawn(&handle, foot, &[], &[]);

    let ev = wait_event(&events, APP_TIMEOUT, "foot WindowMapped", |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };
    wait_event(
        &events,
        APP_TIMEOUT,
        "foot SurfaceCommitted",
        |e| matches!(e, CompositorEvent::SurfaceCommitted { window_id, .. } if *window_id == Some(window.id)),
    );

    let before = handle.capture_window(window.id).unwrap().value;
    assert_rendered(&before, "initial foot capture");

    // Synthetic input delivery, driven through the virtual frame clock:
    // with the clock paused foot can only repaint once a step releases its
    // pending frame callbacks, so every post-input commit is step-driven.
    handle
        .set_frame_clock_mode(FrameClockMode::Paused)
        .expect("pausing frame clock failed");
    // Drop any events queued before the pause so the wait below is clean.
    while events.recv_timeout(Duration::ZERO).is_ok() {}

    handle
        .type_text("echo scry-e2e-input\n")
        .expect("type_text failed");

    // The echo travels keys → pty → shell → repaint request → frame
    // callback, so the first step may land before foot armed a callback:
    // keep stepping until the window both commits and its pixels change.
    let deadline = Instant::now() + APP_TIMEOUT;
    loop {
        handle
            .step_frame_clock(Duration::from_millis(16))
            .expect("frame clock step failed");
        while let Ok(ev) = events.recv_timeout(Duration::from_secs(2)) {
            if matches!(ev, CompositorEvent::SurfaceCommitted { window_id, .. } if window_id == Some(window.id))
            {
                break;
            }
        }
        let capture = handle.capture_window(window.id).unwrap().value;
        if capture.pixels != before.pixels {
            break;
        }
        if Instant::now() >= deadline {
            app.child.kill().ok();
            let (_, _, stderr) = app.wait_exit(Duration::from_secs(5), "foot");
            panic!(
                "foot capture did not change after synthetic input (foot stderr: {})",
                String::from_utf8_lossy(&stderr)
            );
        }
    }

    app.child.kill().ok();
    let _ = app.wait_exit(APP_TIMEOUT, "foot");
    wait_event(
        &events,
        APP_TIMEOUT,
        "foot WindowClosed",
        |e| matches!(e, CompositorEvent::WindowClosed { window_id, .. } if *window_id == window.id),
    );
    handle.shutdown().expect("shutdown failed");
}
