//! xdg toplevel lifecycle: mapping, metadata + SO_PEERCRED pid, commit/damage
//! events, resize, close, and sequence-correlated command replies.

mod common;

use common::{TestClient, wait_for_event};
use scry::{Compositor, CompositorEvent, Rect};

#[test]
fn toplevel_map_emits_window_mapped_with_metadata_and_pid() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let _toplevel = client.map_toplevel(640, 480, "org.scry.Test", "Test Window");

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { seq, window } = ev else {
        unreachable!()
    };
    assert!(seq >= 1);
    assert_eq!(window.app_id.as_deref(), Some("org.scry.Test"));
    assert_eq!(window.title.as_deref(), Some("Test Window"));
    assert_eq!(window.pid, Some(std::process::id()));
    assert_eq!(
        window.geometry,
        Rect {
            x: 0,
            y: 0,
            width: 640,
            height: 480
        }
    );

    // The window also shows up in the queryable window list.
    let reply = handle.windows().expect("windows query failed");
    assert!(reply.seq >= seq, "reply seq must be >= last event seq");
    assert_eq!(reply.value.len(), 1);
    assert_eq!(reply.value[0].id, window.id);
    assert_eq!(reply.value[0].app_id.as_deref(), Some("org.scry.Test"));
}

#[test]
fn shm_commit_emits_surface_committed_with_normalized_damage() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(200, 200, "org.scry.Damage", "Damage");

    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };

    // The buffer-attaching commit produced a SurfaceCommitted belonging to
    // the mapped window; attaching a buffer without declaring damage reports
    // the whole buffer as damaged.
    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::SurfaceCommitted { window_id: Some(id), .. } if *id == window.id),
    );
    let CompositorEvent::SurfaceCommitted {
        seq,
        window_id,
        damage,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(window_id, Some(window.id));
    assert_eq!(
        damage,
        vec![Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 200
        }]
    );

    // Partial buffer damage is reported verbatim (buffer coordinates at
    // scale 1 equal surface coordinates).
    let buf2 = client.shm_buffer(200, 200, 0xff00_ff00);
    toplevel.surface.attach(Some(&buf2), 0, 0);
    toplevel.surface.damage_buffer(10, 20, 100, 50);
    toplevel.surface.commit();
    client.roundtrip();

    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::SurfaceCommitted { seq: s, .. } if *s > seq),
    );
    let CompositorEvent::SurfaceCommitted {
        seq: seq2, damage, ..
    } = ev
    else {
        unreachable!()
    };
    assert!(seq2 > seq, "sequence numbers must be monotonic");
    assert_eq!(
        damage,
        vec![Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 50
        }]
    );

    // Surface-coordinate damage normalizes identically at scale 1.
    toplevel.surface.attach(Some(&buf2), 0, 0);
    toplevel.surface.damage(30, 40, 20, 20);
    toplevel.surface.commit();
    client.roundtrip();

    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::SurfaceCommitted { seq: s, .. } if *s > seq2),
    );
    let CompositorEvent::SurfaceCommitted { damage, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        damage,
        vec![Rect {
            x: 30,
            y: 40,
            width: 20,
            height: 20
        }]
    );
}

#[test]
fn metadata_and_geometry_changes_emit_window_updated() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(100, 100, "org.scry.Upd", "Original");

    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };
    let id = window.id;

    // Change the title, then commit: the update event carries the new title.
    toplevel.toplevel.set_title("Renamed".to_string());
    toplevel.surface.attach(Some(&toplevel.buffer), 0, 0);
    toplevel.surface.commit();
    client.roundtrip();

    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowUpdated { window: w, .. } if w.id == id),
    );
    let CompositorEvent::WindowUpdated { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!(window.title.as_deref(), Some("Renamed"));

    // Compositor-initiated resize: configure roundtrip ends in WindowUpdated
    // carrying the negotiated geometry.
    handle.resize_window(id, 320, 240).expect("resize failed");
    // The client sees the configure, acks it (auto), and we drive it to
    // recommit at the new size.
    client.pump_until(|s| {
        s.toplevel_configures
            .iter()
            .any(|&(w, h)| w == 320 && h == 240)
    });
    let new_buf = client.shm_buffer(320, 240, 0xff11_2233);
    toplevel.surface.attach(Some(&new_buf), 0, 0);
    toplevel.surface.commit();
    client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowUpdated { window: w, .. }
            if w.id == id && w.geometry.width == 320 && w.geometry.height == 240)
    });
    let CompositorEvent::WindowUpdated { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        window.geometry,
        Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 240
        }
    );
}

#[test]
fn close_paths_emit_window_closed() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(64, 64, "org.scry.Close", "Close");

    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };
    let id = window.id;

    // 1. Compositor asks the client to close; the client complies by
    //    destroying its xdg objects.
    handle.close_window(id).expect("close_window failed");
    client.pump_until(|s| s.close_requested);
    toplevel.toplevel.destroy();
    toplevel.xsurface.destroy();
    client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowClosed { .. })
    });
    let CompositorEvent::WindowClosed { window_id, .. } = ev else {
        unreachable!()
    };
    assert_eq!(window_id, id);

    // 2. A second window removed by attaching a null buffer unmaps as well.
    let t2 = client.map_toplevel(50, 50, "org.scry.Close2", "Close2");
    let mapped2 = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowMapped { window: w, .. } if w.id != id),
    );
    let CompositorEvent::WindowMapped { window: w2, .. } = mapped2 else {
        unreachable!()
    };
    t2.surface.attach(None, 0, 0);
    t2.surface.commit();
    client.roundtrip();

    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowClosed { window_id: w, .. } if *w == w2.id),
    );
    let CompositorEvent::WindowClosed { window_id, .. } = ev else {
        unreachable!()
    };
    assert_eq!(window_id, w2.id);

    // Unknown ids are loud errors, not silent drops.
    assert!(handle.close_window(id).is_err());
    assert!(handle.resize_window(id, 10, 10).is_err());
}

#[test]
fn command_replies_carry_sequence_numbers() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let _t1 = client.map_toplevel(10, 10, "a", "a");
    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped {
        seq: ev_seq,
        window,
    } = mapped
    else {
        unreachable!()
    };

    let r1 = handle.windows().unwrap();
    let r2 = handle.resize_window(window.id, 20, 20).unwrap();
    let r3 = handle.windows().unwrap();
    assert!(r1.seq >= ev_seq);
    assert!(
        r2.seq >= r1.seq,
        "mutating command applied at seq >= last known"
    );
    assert!(r3.seq >= r2.seq);
}
