//! M1.5: synthetic pointer, XKB keyboard, zwp_text_input_v3 Unicode input,
//! popup grabs, and focus/activation — exercised through a real wayland
//! client against a live `Compositor`.

mod common;

use common::{TestClient, wait_for_event};
use scry::input::PointerTarget;
use scry::{Compositor, CompositorEvent, WindowId};

/// evdev key/button codes used throughout the tests.
const KEY_A: u32 = 30;
const KEY_C: u32 = 46;
const KEY_LEFTCTRL: u32 = 29;
const BTN_LEFT: u32 = 0x110;

/// Waits until `client`'s keyboard focus events show `surface` entered.
fn pump_kb_enter(
    client: &mut TestClient,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) {
    client.pump_until(|s| s.kb_enters.iter().any(|(surf, _)| surf == surface));
}

#[test]
fn seat_and_input_globals_are_advertised() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();

    let mut client = TestClient::connect(handle.socket_path());
    let names = client.global_names();
    for interface in ["wl_seat", "zwp_text_input_manager_v3", "xdg_activation_v1"] {
        assert!(
            names.contains(&interface.to_string()),
            "missing {interface}"
        );
    }
    // Pointer + keyboard capabilities are advertised on the seat.
    client.pump_until(|s| s.seat_caps & 0b011 == 0b011);
    assert!(client.state.pointer.is_some());
    assert!(client.state.keyboard.is_some());
}

#[test]
fn pointer_motion_button_scroll_in_output_coordinates() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(100, 100, "org.scry.Ptr", "Ptr");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    // Motion onto the surface: wl_pointer.enter carrying the
    // surface-local coordinates (no separate motion event on focus change).
    handle
        .move_pointer(PointerTarget::Output, 30.0, 40.0)
        .unwrap();
    client.pump_until(|s| s.pointer_enters.len() == 1);
    let (surface, x, y) = &client.state.pointer_enters[0];
    assert_eq!(*surface, toplevel.surface);
    assert_eq!((*x, *y), (30.0, 40.0));

    // Relative motion on the same surface delivers only motion.
    client.state.pointer_enters.clear();
    handle
        .move_pointer(PointerTarget::Output, 60.0, 70.0)
        .unwrap();
    client.pump_until(|s| {
        s.pointer_motions
            .iter()
            .any(|&(x, y)| x == 60.0 && y == 70.0)
    });
    assert!(
        client.state.pointer_enters.is_empty(),
        "no extra enter for same surface"
    );

    // Leaving the surface produces a leave event.
    handle
        .move_pointer(PointerTarget::Output, 500.0, 500.0)
        .unwrap();
    client.pump_until(|s| !s.pointer_leaves.is_empty());
    assert_eq!(client.state.pointer_leaves[0], toplevel.surface);

    // Button press + release over the surface.
    handle
        .move_pointer(PointerTarget::Output, 30.0, 40.0)
        .unwrap();
    client.roundtrip();
    handle
        .press_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    handle
        .release_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    client.pump_until(|s| s.pointer_buttons.len() >= 2);
    assert_eq!(client.state.pointer_buttons[0], (BTN_LEFT, 1));
    assert_eq!(client.state.pointer_buttons[1], (BTN_LEFT, 0));

    // Scroll delivers an axis event (vertical, negative = up).
    handle.scroll_pointer(0.0, -15.0).unwrap();
    client.pump_until(|s| s.pointer_axis.iter().any(|&(a, _)| a == 0));
    assert_eq!(client.state.pointer_axis[0], (0, -15.0));
}

#[test]
fn pointer_motion_in_window_local_coordinates() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let bottom = client.map_toplevel(100, 100, "org.scry.Bottom", "Bottom");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let _top = client.map_toplevel(100, 100, "org.scry.Top", "Top");
    let top_mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window: top, .. } = top_mapped else {
        unreachable!()
    };
    let bottom_id = handle
        .windows()
        .unwrap()
        .value
        .iter()
        .find(|w| w.id != top.id)
        .unwrap()
        .id;

    // Window-local targeting hits the *named* window even though another
    // window is stacked above it at the same space coordinates.
    handle
        .move_pointer(PointerTarget::Window(bottom_id), 10.0, 20.0)
        .unwrap();
    client.pump_until(|s| {
        s.pointer_enters
            .iter()
            .any(|(surf, _, _)| *surf == bottom.surface)
    });
    let (_, x, y) = client
        .state
        .pointer_enters
        .iter()
        .find(|(surf, _, _)| *surf == bottom.surface)
        .unwrap();
    assert_eq!((*x, *y), (10.0, 20.0));

    // A point outside the window is an explicit error, not a silent drop.
    let err = handle
        .move_pointer(PointerTarget::Window(bottom_id), 500.0, 500.0)
        .unwrap_err();
    assert!(matches!(err, scry::Error::OutsideWindow(..)), "got {err:?}");
}

#[test]
fn window_local_motion_honours_the_window_geometry_origin() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    // A CSD-style toplevel: the buffer is larger than the declared window
    // geometry, whose origin sits at (20, 30) inside the surface (like
    // GTK4 shadow margins).
    let surface = client.compositor.create_surface(&client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let _toplevel = xsurface.get_toplevel(&client.qh, ());
    xsurface.set_window_geometry(20, 30, 100, 80);
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());
    let buffer = client.shm_buffer(140, 140, 0xff33_6699);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip();
    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };

    // (10, 15) relative to the geometry origin lands at surface-local
    // (30, 45) once the geometry offset is taken into account.
    handle
        .move_pointer(PointerTarget::Window(window.id), 10.0, 15.0)
        .unwrap();
    client.pump_until(|s| s.pointer_enters.iter().any(|(surf, _, _)| *surf == surface));
    let (_, x, y) = client
        .state
        .pointer_enters
        .iter()
        .find(|(surf, _, _)| *surf == surface)
        .unwrap();
    assert_eq!((*x, *y), (30.0, 45.0));
}

#[test]
fn type_text_with_an_unmappable_character_is_atomic() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(100, 100, "org.scry.Atom", "Atom");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    pump_kb_enter(&mut client, &toplevel.surface);

    // "🚀" has no keysym on the keymap: the whole command must fail without
    // typing the "ok" prefix first.
    let err = handle.type_text("ok\u{1f680}").unwrap_err();
    assert!(
        matches!(err, scry::Error::UnmappableCharacter('\u{1f680}')),
        "got {err:?}"
    );
    client.roundtrip();
    assert!(
        client.state.keys.is_empty(),
        "failed type_text must not emit key events: {:?}",
        client.state.keys
    );
}

#[test]
fn clicking_an_unfocused_toplevel_raises_and_focuses_it() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let bottom = client.map_toplevel(100, 100, "org.scry.A", "A");
    let a_mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window: a, .. } = a_mapped else {
        unreachable!()
    };
    let _top = client.map_toplevel(100, 100, "org.scry.B", "B");
    let b_mapped = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowMapped { window: w, .. } if w.id != a.id),
    );
    let CompositorEvent::WindowMapped { window: b, .. } = b_mapped else {
        unreachable!()
    };

    // The most recently mapped window holds keyboard focus.
    pump_kb_enter(&mut client, &_top.surface);
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowFocused { window_id, .. } if *window_id == Some(b.id)),
    );

    // Click the covered window via window-local targeting.
    handle
        .move_pointer(PointerTarget::Window(a.id), 10.0, 10.0)
        .unwrap();
    client.roundtrip();
    handle
        .press_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    client.pump_until(|s| !s.pointer_buttons.is_empty());
    handle
        .release_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    client.roundtrip();

    // Focus moved: B got keyboard.leave, A got keyboard.enter, and a
    // WindowFocused event names A.
    client.pump_until(|s| s.kb_leaves.contains(&_top.surface));
    pump_kb_enter(&mut client, &bottom.surface);
    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowFocused { window_id, .. } if *window_id == Some(a.id)),
    );
    assert!(matches!(ev, CompositorEvent::WindowFocused { window_id: Some(id), .. } if id == a.id));

    // Activation state follows: once the client acks + commits, A is
    // reported activated and B is not.
    bottom.surface.attach(Some(&bottom.buffer), 0, 0);
    bottom.surface.commit();
    _top.surface.attach(Some(&_top.buffer), 0, 0);
    _top.surface.commit();
    client.roundtrip();
    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowUpdated { window: w, .. } if w.id == a.id && w.activated),
    );
    assert!(matches!(ev, CompositorEvent::WindowUpdated { .. }));
    let windows = handle.windows().unwrap().value;
    let a_info = windows.iter().find(|w| w.id == a.id).unwrap();
    let b_info = windows.iter().find(|w| w.id == b.id).unwrap();
    assert!(a_info.activated);
    assert!(!b_info.activated);
}

#[test]
fn key_events_and_modifiers_reach_the_focused_window() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(100, 100, "org.scry.Key", "Key");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    pump_kb_enter(&mut client, &toplevel.surface);

    // The client received a compilable XKB keymap.
    let keymap = client.state.keymap.as_ref().expect("no keymap delivered");
    let text = String::from_utf8_lossy(keymap);
    assert!(text.contains("xkb_keymap"), "not an xkb keymap: {text:.80}");

    // Physical press/release by key name.
    handle.press_key("a").unwrap();
    handle.release_key("a").unwrap();
    client.pump_until(|s| s.keys.len() >= 2);
    assert_eq!(client.state.keys[0], (KEY_A, 1));
    assert_eq!(client.state.keys[1], (KEY_A, 0));

    // Modifier combo: Ctrl+C presses ctrl, taps c, releases ctrl.
    handle.key("c", &["ctrl"]).unwrap();
    client.pump_until(|s| s.keys.len() >= 4 + 2);
    let tail = &client.state.keys[2..];
    assert_eq!(tail[0], (KEY_LEFTCTRL, 1));
    assert_eq!(tail[1], (KEY_C, 1));
    assert_eq!(tail[2], (KEY_C, 0));
    assert_eq!(tail[3], (KEY_LEFTCTRL, 0));

    // Modifier state was delivered (non-zero while ctrl held) and then
    // cleared again once the combo finished.
    assert!(
        client.state.mods_log.iter().any(|m| m.0 != 0),
        "no modifiers event delivered"
    );
    assert_eq!(client.state.mods, (0, 0, 0, 0));
}

#[test]
fn type_text_uses_text_input_v3_when_bound() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Txt", "Txt");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    let _text_input = client.new_text_input();
    client.roundtrip();

    handle.type_text("héllo 🦀 wörld").unwrap();
    client.pump_until(|s| s.ti_strings.iter().any(|t| t == "héllo 🦀 wörld"));
    client.pump_until(|s| !s.ti_dones.is_empty());

    // No raw key events were produced for the typed text.
    assert!(
        client.state.keys.is_empty(),
        "text input path must not emit key events: {:?}",
        client.state.keys
    );
}

#[test]
fn type_text_falls_back_to_xkb_when_text_input_not_bound() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Xkb", "Xkb");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    pump_kb_enter(&mut client, &_toplevel.surface);

    // Characters needing shift exercise implicit modifier handling.
    handle.type_text("Hi! a1").unwrap();
    client.pump_until(|s| s.keys.len() >= 8);
    let typed = client.state.typed_text();
    assert_eq!(typed, "Hi! a1", "decoded keysyms: {typed:?}");

    // Clients without zwp_text_input_v3 get no text input events at all.
    assert_eq!(client.state.ti_strings.len(), 0);
}

#[test]
fn type_text_errors_when_nothing_is_focused() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let err = handle.type_text("hello").unwrap_err();
    assert!(matches!(err, scry::Error::NoFocusedSurface), "got {err:?}");
    assert!(handle.press_key("a").is_err());
    assert!(handle.key("c", &["ctrl"]).is_err());
}

#[test]
fn closing_the_focused_window_moves_focus_to_the_topmost_remaining() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let bottom = client.map_toplevel(100, 100, "org.scry.F", "First");
    let a_mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window: a, .. } = a_mapped else {
        unreachable!()
    };
    let top = client.map_toplevel(100, 100, "org.scry.G", "Second");
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowMapped { window: w, .. } if w.id != a.id),
    );
    pump_kb_enter(&mut client, &top.surface);

    // Close the focused window: the other toplevel gains focus.
    top.toplevel.destroy();
    top.xsurface.destroy();
    client.pump_until(|s| s.kb_leaves.contains(&top.surface));
    pump_kb_enter(&mut client, &bottom.surface);
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowFocused { window_id, .. } if *window_id == Some(a.id)),
    );

    // With no windows left, keyboard focus is cleared (reported as None).
    bottom.toplevel.destroy();
    bottom.xsurface.destroy();
    client.roundtrip(); // flush the destroy requests to the server
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowFocused { window_id, .. } if window_id.is_none()),
    );
}

#[test]
fn xdg_activation_request_focuses_and_activates_the_target_window() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let a = client.map_toplevel(100, 100, "org.scry.Act", "Act");
    let a_mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window: aw, .. } = a_mapped else {
        unreachable!()
    };
    let _b = client.map_toplevel(100, 100, "org.scry.B", "B");
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowMapped { window: w, .. } if w.id != aw.id),
    );
    pump_kb_enter(&mut client, &_b.surface);

    // A asks to be activated through the protocol.
    client.activate(&a.surface);

    pump_kb_enter(&mut client, &a.surface);
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowFocused { window_id, .. } if *window_id == Some(aw.id)),
    );

    // Ack + commit refreshes the activated flag.
    a.surface.attach(Some(&a.buffer), 0, 0);
    a.surface.commit();
    client.roundtrip();
    wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::WindowUpdated { window: w, .. } if w.id == aw.id && w.activated),
    );
}

#[test]
fn popup_grab_routes_pointer_and_dismisses_on_outside_click() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let parent = client.map_toplevel(200, 200, "org.scry.Pop", "Pop");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    // Click on the parent to get a valid serial for the popup grab.
    handle
        .move_pointer(PointerTarget::Output, 50.0, 50.0)
        .unwrap();
    client.pump_until(|s| !s.pointer_enters.is_empty());
    handle
        .press_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    client.pump_until(|s| s.last_button_serial != 0);
    let serial = client.state.last_button_serial;
    handle
        .release_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    client.roundtrip();

    // Create an xdg popup + grab on the seat with that serial.
    let configures_before = client.state.configure_serials.len();
    let positioner = client.wm_base.create_positioner(&client.qh, ());
    positioner.set_size(60, 60);
    positioner.set_anchor_rect(10, 10, 40, 40);
    let popup_surface = client.compositor.create_surface(&client.qh, ());
    let popup_xsurf = client
        .wm_base
        .get_xdg_surface(&popup_surface, &client.qh, ());
    let popup = popup_xsurf.get_popup(Some(&parent.xsurface), &positioner, &client.qh, ());
    popup.grab(client.seat.as_ref().unwrap(), serial);
    let buf = client.shm_buffer(60, 60, 0xffaa_bbcc);
    popup_surface.attach(Some(&buf), 0, 0);
    popup_surface.commit();
    // Wait for the popup's xdg_surface configure (auto-acked by dispatch).
    client.pump_until(|s| s.configure_serials.len() > configures_before);
    client.roundtrip();

    // Moving over the popup enters *its* surface.
    handle
        .move_pointer(PointerTarget::Output, 20.0, 20.0)
        .unwrap();
    client.pump_until(|s| {
        s.pointer_enters
            .iter()
            .any(|(surf, _, _)| *surf == popup_surface)
    });

    // Clicking outside every surface dismisses the popup grab (popup_done);
    // the grab forwards events only to its own client while active.
    handle
        .move_pointer(PointerTarget::Output, 900.0, 900.0)
        .unwrap();
    client.roundtrip();
    handle
        .press_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    client.pump_until(|s| s.popup_done > 0);

    // After the grab ended, a subsequent click reaches the parent surface.
    handle
        .move_pointer(PointerTarget::Output, 50.0, 50.0)
        .unwrap();
    client.pump_until(|s| {
        s.pointer_enters
            .iter()
            .any(|(surf, _, _)| *surf == parent.surface)
    });
    handle
        .press_pointer_button(scry::input::PointerButton::Left)
        .unwrap();
    client.pump_until(|s| s.pointer_buttons.len() >= 2);
}

#[test]
fn invalid_input_returns_explicit_errors() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Err", "Err");
    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    // Unknown key names fail loudly.
    let err = handle.key("definitely_not_a_key", &[]).unwrap_err();
    assert!(matches!(err, scry::Error::UnknownKey(_)), "got {err:?}");
    let err = handle.press_key("nope_no_such_key").unwrap_err();
    assert!(matches!(err, scry::Error::UnknownKey(_)), "got {err:?}");

    // Unknown modifiers fail loudly.
    let err = handle.key("c", &["bogus-modifier"]).unwrap_err();
    assert!(
        matches!(err, scry::Error::UnknownModifier(_)),
        "got {err:?}"
    );

    // Unknown window targets fail loudly.
    let err = handle
        .move_pointer(PointerTarget::Window(WindowId(999_999)), 1.0, 1.0)
        .unwrap_err();
    assert!(matches!(err, scry::Error::UnknownWindow(_)), "got {err:?}");
    let err = handle.focus_window(WindowId(999_999)).unwrap_err();
    assert!(matches!(err, scry::Error::UnknownWindow(_)), "got {err:?}");

    // Non-finite pointer coordinates fail loudly, for both motion targets
    // and scroll deltas.
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = handle
            .move_pointer(PointerTarget::Output, bad, 0.0)
            .unwrap_err();
        assert!(
            matches!(err, scry::Error::InvalidCoordinates(..)),
            "got {err:?}"
        );
        let err = handle
            .move_pointer(PointerTarget::Output, 0.0, bad)
            .unwrap_err();
        assert!(
            matches!(err, scry::Error::InvalidCoordinates(..)),
            "got {err:?}"
        );
        let err = handle.scroll_pointer(bad, 0.0).unwrap_err();
        assert!(
            matches!(err, scry::Error::InvalidCoordinates(..)),
            "got {err:?}"
        );
    }
    // And a failed command must not have delivered any pointer events.
    client.roundtrip();
    assert!(client.state.pointer_axis.is_empty());
}
