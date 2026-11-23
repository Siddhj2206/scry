//! Deterministic window placement (M2.2): `place_window` at the
//! `CompositorHandle` seam, the deterministic initial-placement policy,
//! normalized damage reporting, and the matching daemon JSON-RPC verbs
//! (`list_windows`, `place_window`, `capture_window`, `capture_workspace`,
//! `get_damage`).

mod common;

use std::{path::PathBuf, time::Duration};

use common::{TestClient, wait_for_event};
use scry::{Compositor, CompositorEvent, Rect};

#[test]
fn place_window_moves_to_exact_position_and_configures_size() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(200, 150, "org.scry.Place", "Place");

    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };
    let id = window.id;
    // First window maps at the deterministic origin slot.
    assert_eq!((window.geometry.x, window.geometry.y), (0, 0));

    handle
        .place_window(id, (50, 60), Some((300, 220)))
        .expect("place_window failed");

    // The move is applied immediately in compositor space (and reported on
    // the event stream) even before the client recommits.
    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowUpdated { window: w, .. } if w.id == id),
    );
    let CompositorEvent::WindowUpdated { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!((window.geometry.x, window.geometry.y), (50, 60));

    // The client sees a resize configure for the requested size; acking and
    // recommitting at that size produces the final WindowUpdated geometry.
    client.pump_until(|s| {
        s.toplevel_configures
            .iter()
            .any(|&(w, h)| w == 300 && h == 220)
    });
    let new_buf = client.shm_buffer(300, 220, 0xff33_6699);
    toplevel.surface.attach(Some(&new_buf), 0, 0);
    toplevel.surface.commit();
    client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowUpdated { window: w, .. }
            if w.id == id && w.geometry.width == 300 && w.geometry.height == 220)
    });
    let CompositorEvent::WindowUpdated { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        window.geometry,
        Rect {
            x: 50,
            y: 60,
            width: 300,
            height: 220
        }
    );
}

/// Maps a solid-color toplevel (like `map_toplevel` but with a chosen ARGB
/// fill) and returns its `WindowInfo` plus the live client objects.
fn map_colored(
    client: &mut TestClient,
    events: &std::sync::mpsc::Receiver<CompositorEvent>,
    argb: u32,
) -> (scry::WindowInfo, common::Toplevel) {
    let surface = client.compositor.create_surface(&client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());
    let buffer = client.shm_buffer(100, 100, argb);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip();
    let ev = wait_for_event(events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };
    (
        window,
        common::Toplevel {
            surface,
            xsurface,
            toplevel,
            buffer,
        },
    )
}

#[test]
fn place_window_move_only_preserves_size_and_z_order() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let (w1, _t1) = map_colored(&mut client, &events, 0xffff_0000); // red, bottom
    let (w2, _t2) = map_colored(&mut client, &events, 0xff00_ff00); // green, top

    // Move-only placement on the *bottom* window: no size configure is
    // sent, the committed size is kept, and the window is not raised.
    handle
        .place_window(w1.id, (50, 50), None)
        .expect("place_window failed");

    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowUpdated { window: w, .. } if w.id == w1.id),
    );
    let CompositorEvent::WindowUpdated { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        window.geometry,
        Rect {
            x: 50,
            y: 50,
            width: 100,
            height: 100
        }
    );

    // Overlap the windows: w1 spans (50,50)-(150,150); place w2 to
    // (100,100)-(200,200). Their overlap (100,100)-(150,150) must still
    // show w2's green — placement must not silently raise the moved window.
    handle
        .place_window(w2.id, (100, 100), None)
        .expect("place_window failed");
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowUpdated { window: w, .. } if w.id == w2.id),
    );
    let frame = handle.capture_output("Virtual-1").unwrap().value;
    let offset = ((125 * frame.width + 125) * 4) as usize;
    let pixel = &frame.pixels[offset..offset + 4];
    assert_eq!(pixel, &[0, 0xff, 0, 0xff]);
}

#[test]
fn newly_mapped_windows_cascade_deterministically() {
    fn map_positions() -> Vec<(i32, i32)> {
        let compositor = Compositor::spawn().unwrap();
        let handle = compositor.handle().clone();
        let events = handle.subscribe().unwrap();
        let mut client = TestClient::connect(handle.socket_path());
        let mut positions = Vec::new();
        for _ in 0..3 {
            let _t = client.map_toplevel(100, 100, "a", "a");
            let ev = wait_for_event(&events, |e| {
                matches!(e, CompositorEvent::WindowMapped { .. })
            });
            let CompositorEvent::WindowMapped { window, .. } = ev else {
                unreachable!()
            };
            positions.push((window.geometry.x, window.geometry.y));
        }
        positions
    }

    // A diagonal cascade: each subsequent window steps off the last.
    assert_eq!(map_positions(), vec![(0, 0), (32, 32), (64, 64)]);
    // Two fresh compositors produce byte-identical initial layouts.
    assert_eq!(map_positions(), map_positions());
}

#[test]
fn damage_since_reports_normalized_committed_regions() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(200, 200, "org.scry.Dmg", "Dmg");
    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };

    // Wait for the map commit's own SurfaceCommitted to know the seq base.
    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::SurfaceCommitted { window_id: Some(w), .. } if *w == window.id),
    );
    let CompositorEvent::SurfaceCommitted { seq: map_seq, .. } = ev else {
        unreachable!()
    };

    // The full damage history includes the buffer-attaching map commit.
    let entries = handle.damage_since(0).expect("damage_since failed").value;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].window_id, Some(window.id));
    assert_eq!(
        entries[0].damage,
        vec![Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 200
        }]
    );

    // A partial-damage commit appends a normalized entry at a later seq.
    let buf2 = client.shm_buffer(200, 200, 0xff00_ff00);
    toplevel.surface.attach(Some(&buf2), 0, 0);
    toplevel.surface.damage_buffer(10, 20, 100, 50);
    toplevel.surface.commit();
    client.roundtrip();

    let entries = handle
        .damage_since(map_seq)
        .expect("damage_since failed")
        .value;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].window_id, Some(window.id));
    assert!(entries[0].seq > map_seq);
    assert_eq!(
        entries[0].damage,
        vec![Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 50
        }]
    );
}

// ---------------------------------------------------------------------------
// Daemon JSON-RPC seam
// ---------------------------------------------------------------------------

use common::daemon::RpcClient;
use scry::daemon::{DaemonConfig, DaemonHandle};
use serde_json::{Value, json};
use tempfile::TempDir;

struct TestDaemon {
    #[allow(dead_code)] // kept alive so the daemon outlives each test
    handle: DaemonHandle,
    _dir: TempDir,
    socket_path: PathBuf,
}

fn spawn_daemon() -> TestDaemon {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let socket_path = dir.path().join("run/scry.sock");
    let state_dir = dir.path().join("state");
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        state_dir,
        default_idle_ttl: Duration::from_secs(300),
        reaper_interval: Duration::from_millis(25),
    };
    let handle = DaemonHandle::spawn(config).expect("daemon failed to start");
    TestDaemon {
        handle,
        _dir: dir,
        socket_path,
    }
}

/// Creates a workspace and returns `(summary, socket_path)`.
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

fn window_ids(list: &Value) -> Vec<u64> {
    list["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .collect()
}

#[test]
fn list_windows_reports_toplevel_and_popup_metadata() {
    use wayland_protocols::xdg::shell::client::xdg_positioner;

    let _daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&_daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "wins", json!({}));

    let mut client = TestClient::connect(&socket);
    let toplevel = client.map_toplevel(320, 200, "org.scry.List", "Listed");
    // The activation configure lands at map time; the client must commit
    // once more for `activated` to enter the toplevel's committed state.
    toplevel.surface.attach(Some(&toplevel.buffer), 0, 0);
    toplevel.surface.commit();
    client.roundtrip();

    let listed = rpc.call("list_windows", json!({"workspace": "wins"}));
    let windows = listed["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let w = &windows[0];
    assert_eq!(w["app_id"], "org.scry.List");
    assert_eq!(w["title"], "Listed");
    assert_eq!(
        w["geometry"],
        json!({"x": 0, "y": 0, "width": 320, "height": 200})
    );
    assert_eq!(w["buffer_size"], json!([320, 200]));
    assert_eq!(w["pid"], std::process::id() as u64);
    assert_eq!(w["activated"], true);
    assert_eq!(w["maximized"], false);
    assert_eq!(w["scale"], 1.0);
    assert!(listed["seq"].as_u64().unwrap() > 0);

    // A mapped popup shows up under `popups` anchored to its window.
    let positioner = client.wm_base.create_positioner(&client.qh, ());
    positioner.set_size(16, 16);
    positioner.set_anchor_rect(0, 0, 1, 1);
    positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
    positioner.set_offset(10, 20);
    let popup_surface = client.compositor.create_surface(&client.qh, ());
    let popup_xsurface = client
        .wm_base
        .get_xdg_surface(&popup_surface, &client.qh, ());
    let _popup = popup_xsurface.get_popup(Some(&toplevel.xsurface), &positioner, &client.qh, ());
    positioner.destroy();
    client.pump_until(|s| !s.popup_configures.is_empty());
    let popup_buffer = client.shm_buffer(16, 16, 0xffff_00ff);
    popup_surface.attach(Some(&popup_buffer), 0, 0);
    popup_surface.commit();
    client.roundtrip();

    let listed = rpc.call("list_windows", json!({"workspace": "wins"}));
    let popups = listed["popups"].as_array().unwrap();
    assert_eq!(popups.len(), 1);
    assert_eq!(popups[0]["window_id"], w["id"]);
    assert_eq!(
        popups[0]["geometry"],
        json!({"x": 10, "y": 20, "width": 16, "height": 16})
    );
    let _ = (popup_surface, popup_xsurface, popup_buffer);
}

/// Maps a window, applies `place_window` over JSON-RPC and drives the client
/// through the configure/ack so the workspace reaches its final geometry.
fn place_and_settle(
    rpc: &mut RpcClient,
    client: &mut TestClient,
    toplevel: &common::Toplevel,
    workspace: &str,
    window: u64,
    rect: (i64, i64, i64, i64),
) {
    let (x, y, w, h) = rect;
    rpc.call(
        "place_window",
        json!({"workspace": workspace, "window": window, "x": x, "y": y, "width": w, "height": h}),
    );
    client.pump_until(|s| {
        s.toplevel_configures
            .iter()
            .any(|&(cw, ch)| cw as i64 == w && ch as i64 == h)
    });
    let buf = client.shm_buffer(w as i32, h as i32, 0xff33_6699);
    toplevel.surface.attach(Some(&buf), 0, 0);
    toplevel.surface.commit();
    client.roundtrip();
}

#[test]
fn identical_place_sequences_produce_identical_geometry_and_captures() {
    let _daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&_daemon.socket_path);

    let mut results = Vec::new();
    for ws in ["det-a", "det-b"] {
        let (_summary, socket) = create_workspace(&mut rpc, ws, json!({}));
        let mut client = TestClient::connect(&socket);
        let toplevel = client.map_toplevel(200, 150, "org.scry.Det", "Det");
        client.roundtrip();

        let listed = rpc.call("list_windows", json!({"workspace": ws}));
        let id = window_ids(&listed)[0];
        place_and_settle(
            &mut rpc,
            &mut client,
            &toplevel,
            ws,
            id,
            (100, 80, 400, 300),
        );

        let listed = rpc.call("list_windows", json!({"workspace": ws}));
        let capture_window = rpc.call(
            "capture_window",
            json!({"workspace": ws, "window": id, "format": "png"}),
        );
        let capture_ws = rpc.call(
            "capture_workspace",
            json!({"workspace": ws, "format": "png"}),
        );
        let capture_ws_webp = rpc.call(
            "capture_workspace",
            json!({"workspace": ws, "format": "webp"}),
        );
        results.push((
            listed["windows"][0]["geometry"].clone(),
            capture_window["data"].clone(),
            capture_ws["data"].clone(),
            capture_ws_webp["data"].clone(),
        ));
        let _ = toplevel;
    }

    assert_eq!(
        results[0], results[1],
        "identical runs must be byte-identical"
    );
    assert_eq!(
        results[0].0,
        json!({"x": 100, "y": 80, "width": 400, "height": 300})
    );
}

#[test]
fn read_only_workspace_rejects_place_but_allows_observation() {
    let _daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&_daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "frozen", json!({"read_only": true}));

    let mut client = TestClient::connect(&socket);
    let _toplevel = client.map_toplevel(100, 100, "a", "a");
    client.roundtrip();

    let listed = rpc.call("list_windows", json!({"workspace": "frozen"}));
    let id = window_ids(&listed)[0];

    // Observation verbs succeed on a readOnly workspace.
    let png = rpc.call(
        "capture_window",
        json!({"workspace": "frozen", "window": id, "format": "png"}),
    );
    assert!(png["data"].as_str().is_some());
    let ws_cap = rpc.call(
        "capture_workspace",
        json!({"workspace": "frozen", "format": "webp"}),
    );
    assert!(ws_cap["data"].as_str().is_some());
    let dmg = rpc.call("get_damage", json!({"workspace": "frozen"}));
    assert!(dmg["damage"].as_array().is_some());

    // Placement is mutating and rejected with the read-only error.
    let err = rpc.call_err(
        "place_window",
        json!({"workspace": "frozen", "window": id, "x": 10, "y": 10}),
    );
    assert_eq!(err.code, -32003);
}

#[test]
fn get_damage_filters_by_window_and_after_seq() {
    let _daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&_daemon.socket_path);
    let (_ws, socket) = create_workspace(&mut rpc, "dmg", json!({}));

    let mut client = TestClient::connect(&socket);
    let t1 = client.map_toplevel(200, 200, "one", "one");
    let t2 = client.map_toplevel(64, 64, "two", "two");
    client.roundtrip();

    let listed = rpc.call("list_windows", json!({"workspace": "dmg"}));
    let ids = window_ids(&listed);
    assert_eq!(ids.len(), 2);

    // Baseline: the map commits already recorded damage for both windows.
    let all = rpc.call("get_damage", json!({"workspace": "dmg"}));
    assert_eq!(all["damage"].as_array().unwrap().len(), 2);
    let base_seq = all["seq"].as_u64().unwrap();

    // Partial damage on window 1 only.
    let buf = client.shm_buffer(200, 200, 0xff00_ff00);
    t1.surface.attach(Some(&buf), 0, 0);
    t1.surface.damage_buffer(10, 20, 100, 50);
    t1.surface.commit();
    client.roundtrip();

    let filtered = rpc.call(
        "get_damage",
        json!({"workspace": "dmg", "window": ids[0], "after_seq": base_seq}),
    );
    let entries = filtered["damage"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["window_id"], ids[0]);
    assert_eq!(
        entries[0]["damage"],
        json!([{"x": 10, "y": 20, "width": 100, "height": 50}])
    );
    assert!(entries[0]["seq"].as_u64().unwrap() > base_seq);

    // The other window's filter returns nothing new.
    let other = rpc.call(
        "get_damage",
        json!({"workspace": "dmg", "window": ids[1], "after_seq": base_seq}),
    );
    assert_eq!(other["damage"].as_array().unwrap().len(), 0);
    let _ = t2;
}

#[test]
fn unknown_workspace_window_and_output_are_structured_errors() {
    let _daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&_daemon.socket_path);
    let (_ws, _socket) = create_workspace(&mut rpc, "errs", json!({}));

    // Unknown workspace.
    let err = rpc.call_err("list_windows", json!({"workspace": "ghost"}));
    assert_eq!(err.code, -32001);

    // Unknown window ids on window-scoped verbs.
    let err = rpc.call_err(
        "place_window",
        json!({"workspace": "errs", "window": 9999, "x": 0, "y": 0}),
    );
    assert_eq!(err.code, -32004);
    let err = rpc.call_err(
        "capture_window",
        json!({"workspace": "errs", "window": 9999}),
    );
    assert_eq!(err.code, -32004);

    // Unknown output on workspace capture.
    let err = rpc.call_err(
        "capture_workspace",
        json!({"workspace": "errs", "output": "HDMI-nope"}),
    );
    assert_eq!(err.code, -32005);

    // Invalid formats and geometry are parameter errors.
    let err = rpc.call_err(
        "capture_window",
        json!({"workspace": "errs", "window": 1, "format": "gif"}),
    );
    assert_eq!(err.code, -32602);
}
