//! Virtual frame clock: realtime cadence, paused hold, deterministic stepping.
//!
//! Covers issue #5: `wl_surface.frame` callbacks driven by a controllable
//! virtual clock with realtime (60Hz) and paused/stepped modes. All
//! synchronization is protocol-driven — no `sleep()` anywhere.

mod common;

use std::time::Duration;

use common::TestClient;
use scry::{Compositor, FrameClockMode};

/// In realtime mode (the default) a client's `wl_surface.frame` requests are
/// answered on a steady cadence with monotonically increasing timestamps.
#[test]
fn realtime_mode_fires_frame_callbacks_with_bounded_monotonic_cadence() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(320, 240, "org.scry.FrameClock", "frames");

    // Request a frame, commit to make it pending, and wait for the
    // compositor's next tick. `pump_until` bounds the wait to the test
    // timeout, so a stalled clock fails rather than hangs.
    toplevel.surface.frame(&client.qh, 0);
    toplevel.surface.commit();
    client.pump_until(|s| !s.frame_times.is_empty());
    let first = client.state.frame_times[0];

    toplevel.surface.frame(&client.qh, 0);
    toplevel.surface.commit();
    client.pump_until(|s| s.frame_times.len() >= 2);
    let second = client.state.frame_times[1];

    assert!(
        second > first,
        "frame timestamps must increase monotonically ({first} -> {second})"
    );
    // Bounded cadence: consecutive callbacks are one frame interval apart,
    // never a free spin and never a stall.
    assert!(
        second - first <= 100,
        "cadence must be bounded (~16.7ms), got {}ms",
        second - first
    );
}

/// In paused mode pending `wl_surface.frame` callbacks are held indefinitely
/// until `step` releases them.
#[test]
fn paused_mode_holds_frame_callbacks_until_step() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    handle
        .set_frame_clock_mode(FrameClockMode::Paused)
        .expect("pause failed");

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(320, 240, "org.scry.FrameClock", "frames");

    toplevel.surface.frame(&client.qh, 0);
    toplevel.surface.commit();
    // Each roundtrip is a full server-processing barrier: if the compositor
    // still fired frame callbacks while paused they would be observed here.
    for _ in 0..3 {
        client.roundtrip();
    }
    assert!(
        client.state.frame_times.is_empty(),
        "paused clock must hold pending frame callbacks"
    );

    // Stepping advances the clock and releases the held callback.
    handle
        .step_frame_clock(Duration::from_millis(100))
        .expect("step failed");
    client.pump_until(|s| !s.frame_times.is_empty());
}

/// `step(duration)` stamps released callbacks with a virtual presentation
/// time advanced by exactly the requested duration.
#[test]
fn step_advances_frame_timestamps_by_the_requested_duration() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    handle
        .set_frame_clock_mode(FrameClockMode::Paused)
        .expect("pause failed");

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(320, 240, "org.scry.FrameClock", "frames");

    toplevel.surface.frame(&client.qh, 0);
    toplevel.surface.commit();
    client.roundtrip();
    handle
        .step_frame_clock(Duration::from_millis(100))
        .expect("step failed");
    client.pump_until(|s| !s.frame_times.is_empty());
    let first = client.state.frame_times[0];

    toplevel.surface.frame(&client.qh, 0);
    toplevel.surface.commit();
    client.roundtrip();
    handle
        .step_frame_clock(Duration::from_millis(50))
        .expect("step failed");
    client.pump_until(|s| s.frame_times.len() >= 2);
    let second = client.state.frame_times[1];

    // The delta is exact: the virtual clock advanced by precisely 50ms.
    assert_eq!(second - first, 50);
}

/// Switching back to realtime mode resumes firing held and subsequent frame
/// callbacks without any `step` calls.
#[test]
fn resuming_realtime_mode_fires_held_callbacks_again() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    handle
        .set_frame_clock_mode(FrameClockMode::Paused)
        .expect("pause failed");

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(320, 240, "org.scry.FrameClock", "frames");

    toplevel.surface.frame(&client.qh, 0);
    toplevel.surface.commit();
    for _ in 0..3 {
        client.roundtrip();
    }
    assert!(client.state.frame_times.is_empty());

    handle
        .set_frame_clock_mode(FrameClockMode::Realtime)
        .expect("resume failed");
    client.pump_until(|s| !s.frame_times.is_empty());
}

/// `wl_surface.frame` requested on a mapped `xdg_popup` follows the same
/// virtual clock rules as toplevels: held while paused, released by `step`.
#[test]
fn paused_clock_holds_and_releases_popup_frame_callbacks() {
    use wayland_protocols::xdg::shell::client::xdg_positioner;

    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    handle
        .set_frame_clock_mode(FrameClockMode::Paused)
        .expect("pause failed");

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(64, 64, "org.scry.FramePopup", "popup frames");

    // Map a small popup anchored to the toplevel.
    let positioner = client.wm_base.create_positioner(&client.qh, ());
    positioner.set_size(16, 16);
    positioner.set_anchor_rect(0, 0, 1, 1);
    positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
    let popup_surface = client.compositor.create_surface(&client.qh, ());
    let popup_xsurface = client
        .wm_base
        .get_xdg_surface(&popup_surface, &client.qh, ());
    let _popup = popup_xsurface.get_popup(Some(&toplevel.xsurface), &positioner, &client.qh, ());
    positioner.destroy();
    client.pump_until(|s| !s.popup_configures.is_empty());
    let popup_buffer = client.shm_buffer(16, 16, 0xffaa_bbcc);
    popup_surface.attach(Some(&popup_buffer), 0, 0);
    popup_surface.commit();
    client.roundtrip();

    // A frame request on the popup's surface is held while the clock is
    // paused; roundtrips prove nothing arrives early.
    popup_surface.frame(&client.qh, 0);
    popup_surface.commit();
    for _ in 0..3 {
        client.roundtrip();
    }
    assert!(
        client.state.frame_times.is_empty(),
        "paused clock must hold popup frame callbacks"
    );

    // Stepping advances the clock and releases the held popup callback.
    handle
        .step_frame_clock(Duration::from_millis(16))
        .expect("step failed");
    client.pump_until(|s| !s.frame_times.is_empty());
}
