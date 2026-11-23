//! Virtual `wl_output` management, HiDPI scale negotiation
//! (`wp_fractional_scale_v1` + `preferred_buffer_scale`), `xdg_decoration`
//! mode negotiation and `wp_viewporter` crop/destination scaling.

mod common;

use common::{TestClient, wait_for_event};
use scry::{Compositor, CompositorEvent, OutputSpec};
use wayland_client::protocol::wl_shm;
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        viewporter::client::wp_viewporter::WpViewporter,
    },
    xdg::{
        decoration::zv1::client::{
            zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, zxdg_toplevel_decoration_v1,
        },
        xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1,
    },
};

const SCALE_1_5: f64 = 1.5;

fn spec(name: &str, size: (i32, i32), scale: f64, position: (i32, i32)) -> OutputSpec {
    OutputSpec {
        name: name.to_string(),
        size,
        refresh_millihertz: 60_000,
        scale,
        position,
    }
}

#[test]
fn advertises_output_protocols() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    let client = TestClient::connect(handle.socket_path());
    let names = client.global_names();
    for interface in [
        "wp_viewporter",
        "wp_fractional_scale_manager_v1",
        "zxdg_decoration_manager_v1",
        "zxdg_output_manager_v1",
    ] {
        assert!(
            names.contains(&interface.to_string()),
            "missing global {interface} in {names:?}"
        );
    }
}

#[test]
fn default_output_advertises_mode_geometry_and_scale() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    let mut client = TestClient::connect(handle.socket_path());
    client.pump_until(|s| s.output_details[0].dones > 0);
    let detail = &client.state.output_details[0];

    assert_eq!(detail.name.as_deref(), Some("Virtual-1"));
    assert_eq!(detail.mode, Some((1920, 1080, 60_000)));
    assert_eq!(detail.scale, Some(1));
    assert_eq!(detail.make_model, Some(("scry".into(), "virtual".into())));

    // zxdg_output_v1 reports the logical geometry.
    let manager = client
        .globals
        .bind::<ZxdgOutputManagerV1, _, _>(&client.qh, 1..=3, ())
        .expect("zxdg_output_manager_v1 not advertised");
    let wl_output = client.output.clone().unwrap();
    manager.get_xdg_output(&wl_output, &client.qh, 0usize);
    client.pump_until(|s| s.output_details[0].xdg_name.is_some());
    let detail = &client.state.output_details[0];
    assert_eq!(detail.xdg_logical_position, Some((0, 0)));
    assert_eq!(detail.xdg_logical_size, Some((1920, 1080)));
    assert_eq!(detail.xdg_name.as_deref(), Some("Virtual-1"));
}

#[test]
fn update_output_adds_virtual_output() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    handle
        .update_output(spec("Virtual-2", (3840, 2160), 2.0, (1920, 0)))
        .unwrap();

    // The compositor-side listing reports it immediately.
    let outputs = handle.outputs().unwrap().value;
    assert_eq!(outputs.len(), 2);
    let added = outputs.iter().find(|o| o.name == "Virtual-2").unwrap();
    assert_eq!(added.size, (3840, 2160));
    assert_eq!(added.scale, 2.0);
    // Logical geometry: 3840x2160 physical at 2x = 1920x1080 at (1920, 0).
    assert_eq!(
        added.geometry,
        scry::Rect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080
        }
    );

    // A client bound before sees the new global advertised on the registry.
    let mut client = TestClient::connect(handle.socket_path());
    client.roundtrip();
    let second = client.bind_output(1);
    let manager = client
        .globals
        .bind::<ZxdgOutputManagerV1, _, _>(&client.qh, 1..=3, ())
        .unwrap();
    manager.get_xdg_output(&second, &client.qh, 1usize);
    client.pump_until(|s| s.output_details[1].dones > 0);
    let detail = &client.state.output_details[1];
    assert_eq!(detail.name.as_deref(), Some("Virtual-2"));
    assert_eq!(detail.mode, Some((3840, 2160, 60_000)));
    assert_eq!(detail.scale, Some(2));
    client.pump_until(|s| s.output_details[1].xdg_logical_size.is_some());
    let detail = &client.state.output_details[1];
    assert_eq!(detail.xdg_logical_position, Some((1920, 0)));
    assert_eq!(detail.xdg_logical_size, Some((1920, 1080)));
}

#[test]
fn update_output_reconfigures_existing_output() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    let mut client = TestClient::connect(handle.socket_path());
    client.pump_until(|s| s.output_details[0].dones > 0);

    // Scale Virtual-1 up to 1.5 at a 2560x1440 physical mode.
    handle
        .update_output(spec("Virtual-1", (2560, 1440), SCALE_1_5, (0, 0)))
        .unwrap();

    // wl_output only carries the integer scale (rounded up to 2); the
    // fractional resolution shows up through xdg_output's logical size.
    client.pump_until(|s| s.output_details[0].scale == Some(2));
    let detail = &client.state.output_details[0];
    assert_eq!(detail.mode, Some((2560, 1440, 60_000)));

    let outputs = handle.outputs().unwrap().value;
    let v1 = outputs.iter().find(|o| o.name == "Virtual-1").unwrap();
    assert_eq!(v1.scale, SCALE_1_5);
    assert_eq!(
        v1.geometry,
        scry::Rect {
            x: 0,
            y: 0,
            width: 1707,
            height: 960
        }
    );
}

#[test]
fn remove_output_withdraws_the_global() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    handle
        .update_output(spec("Virtual-2", (1280, 720), 1.0, (1920, 0)))
        .unwrap();
    assert_eq!(handle.outputs().unwrap().value.len(), 2);

    handle.remove_output("Virtual-2").unwrap();
    let outputs = handle.outputs().unwrap().value;
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].name, "Virtual-1");

    // Removing twice and removing the last output both fail loudly.
    assert!(handle.remove_output("Virtual-2").is_err());
    assert!(handle.remove_output("Virtual-1").is_err());
}

#[test]
fn update_output_rejects_invalid_specs() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    for spec in [
        spec("", (100, 100), 1.0, (0, 0)),
        spec("bad", (0, 100), 1.0, (0, 0)),
        spec("bad", (100, -5), 1.0, (0, 0)),
        spec("bad", (100, 100), 0.0, (0, 0)),
        spec("bad", (100, 100), -1.0, (0, 0)),
        spec("bad", (100, 100), f64::NAN, (0, 0)),
        spec("bad", (100, 100), f64::INFINITY, (0, 0)),
        OutputSpec {
            refresh_millihertz: 0,
            ..spec("bad", (100, 100), 1.0, (0, 0))
        },
    ] {
        assert!(
            handle.update_output(spec).is_err(),
            "spec should be rejected"
        );
    }
    assert_eq!(handle.outputs().unwrap().value.len(), 1);
}

#[test]
fn fractional_scale_reports_preferred_scale_on_map() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();
    let _events = handle.subscribe().unwrap();

    // Virtual-1 at a fractional scale.
    handle
        .update_output(spec("Virtual-1", (2560, 1440), SCALE_1_5, (0, 0)))
        .unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let manager = client
        .globals
        .bind::<WpFractionalScaleManagerV1, _, _>(&client.qh, 1..=1, ())
        .expect("wp_fractional_scale_manager_v1 not advertised");

    let surface = client.compositor.create_surface(&client.qh, ());
    let _fs = manager.get_fractional_scale(&surface, &client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let _toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());
    let buffer = client.shm_buffer(100, 100, 0xffaa_bbcc);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();

    // 1.5 in fixed-point n/120 units is 180.
    client.pump_until(|s| s.preferred_scales.back() == Some(&180));
}

#[test]
fn fractional_scale_updates_when_output_scale_changes() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();
    let _events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let manager = client
        .globals
        .bind::<WpFractionalScaleManagerV1, _, _>(&client.qh, 1..=1, ())
        .unwrap();

    let surface = client.compositor.create_surface(&client.qh, ());
    let _fs = manager.get_fractional_scale(&surface, &client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let _toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());
    let buffer = client.shm_buffer(64, 64, 0xff11_2233);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.pump_until(|s| s.preferred_scales.back() == Some(&120));

    // Scaling the output the window sits on re-publishes the hint.
    handle
        .update_output(spec("Virtual-1", (1920, 1080), 2.0, (0, 0)))
        .unwrap();
    client.pump_until(|s| s.preferred_scales.back() == Some(&240));
}

#[test]
fn xdg_decoration_negotiates_modes() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    let mut client = TestClient::connect(handle.socket_path());
    let manager = client
        .globals
        .bind::<ZxdgDecorationManagerV1, _, _>(&client.qh, 1..=1, ())
        .expect("zxdg_decoration_manager_v1 not advertised");

    let surface = client.compositor.create_surface(&client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let toplevel = xsurface.get_toplevel(&client.qh, ());
    let decoration = manager.get_toplevel_decoration(&toplevel, &client.qh, ());
    surface.commit();

    // The compositor's standing preference is server-side decoration.
    client.pump_until(|s| {
        s.decoration_modes.last() == Some(&zxdg_toplevel_decoration_v1::Mode::ServerSide)
    });

    // Requesting client-side is honored and echoed back.
    decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
    client.pump_until(|s| {
        s.decoration_modes.last() == Some(&zxdg_toplevel_decoration_v1::Mode::ClientSide)
    });

    // Unsetting restores the compositor preference.
    decoration.unset_mode();
    client.pump_until(|s| {
        s.decoration_modes.last() == Some(&zxdg_toplevel_decoration_v1::Mode::ServerSide)
    });
}

#[test]
fn viewporter_source_crop_and_dst_scale_capture_exactly() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let viewporter = client
        .globals
        .bind::<WpViewporter, _, _>(&client.qh, 1..=1, ())
        .expect("wp_viewporter not advertised");

    let surface = client.compositor.create_surface(&client.qh, ());
    let viewport = viewporter.get_viewport(&surface, &client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let _toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());

    // A 64x64 buffer whose four 32x32 quadrants are distinct colors.
    let (w, h) = (64, 64);
    let quadrant = |x: i32, y: i32| match (x < 32, y < 32) {
        (true, true) => 0xffff_0000u32,   // red
        (false, true) => 0xff00_ff00u32,  // green
        (true, false) => 0xff00_00ffu32,  // blue
        (false, false) => 0xffff_ff00u32, // yellow
    };
    let pixels: Vec<u32> = (0..h)
        .flat_map(|y| (0..w).map(move |x| quadrant(x, y)))
        .collect();
    let buffer = client.shm_buffer_pixels(w, h, &pixels, wl_shm::Format::Argb8888);

    // Crop to the top-right 32x32 quadrant (all green), stretch it to 16x16.
    viewport.set_source(32.0, 0.0, 32.0, 32.0);
    viewport.set_destination(16, 16);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip();

    wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    // The window's logical size is the viewport destination size.
    let windows = handle.windows().unwrap().value;
    assert_eq!(windows.len(), 1);
    let window = &windows[0];
    assert_eq!(
        window.geometry,
        scry::Rect {
            x: 0,
            y: 0,
            width: 16,
            height: 16
        }
    );

    // Per-window capture: exact 16x16 of the cropped quadrant color.
    let frame = handle.capture_window(window.id).unwrap().value;
    assert_eq!((frame.width, frame.height), (16, 16));
    for y in 0..frame.height {
        for x in 0..frame.width {
            assert_eq!(frame.pixel(x, y), [0, 255, 0, 255], "pixel {x},{y}");
        }
    }

    // Full-output capture shows the same 16x16 block at the top-left corner.
    let output = handle.capture_output("Virtual-1").unwrap().value;
    for y in 0..16u32 {
        for x in 0..16u32 {
            assert_eq!(output.pixel(x, y), [0, 255, 0, 255], "output {x},{y}");
        }
    }
    assert_eq!(output.pixel(17, 17), [0, 0, 0, 255]);
}

#[test]
fn hidpi_output_capture_renders_at_physical_resolution() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    // A HiDPI output: 2x scale over a 3840x2160 physical mode (1920x1080
    // logical) overlapping the default output's position.
    handle
        .update_output(spec("Virtual-2", (3840, 2160), 2.0, (0, 0)))
        .unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let surface = client.compositor.create_surface(&client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let _toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());

    // A scale-2 buffer: 200x200 physical pixels covering 100x100 logical.
    let mut pixels = vec![0u32; (200 * 200) as usize];
    for y in 0..200i32 {
        for x in 0..200i32 {
            pixels[(y * 200 + x) as usize] = if x < 100 { 0xffff_0000 } else { 0xff00_00ff };
        }
    }
    let buffer = client.shm_buffer_pixels(200, 200, &pixels, wl_shm::Format::Argb8888);
    surface.set_buffer_scale(2);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!(window.geometry.width, 100);
    assert_eq!(window.geometry.height, 100);

    // The output capture is at the mode's physical resolution.
    let output = handle.capture_output("Virtual-2").unwrap().value;
    assert_eq!((output.width, output.height), (3840, 2160));
    for x in 0..200u32 {
        let expected = if x < 100 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        };
        assert_eq!(output.pixel(x, 50), expected, "output pixel x={x}");
    }
    assert_eq!(output.pixel(10, 210), [0, 0, 0, 255]);

    // The per-window capture renders at the output scale too: buffer pixels
    // land 1:1.
    let frame = handle.capture_window(window.id).unwrap().value;
    assert_eq!((frame.width, frame.height), (200, 200));
    assert_eq!(frame.pixel(50, 50), [255, 0, 0, 255]);
    assert_eq!(frame.pixel(150, 50), [0, 0, 255, 255]);
}

#[test]
fn capture_output_rejects_unknown_outputs() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    assert!(handle.capture_output("nope").is_err());
    assert!(handle.remove_output("nope").is_err());
}

#[test]
fn output_updates_send_wl_output_done_and_registry_remove() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();

    let mut client = TestClient::connect(handle.socket_path());
    client.pump_until(|s| s.output_details[0].dones > 0);

    handle
        .update_output(spec("Virtual-2", (800, 600), 1.0, (1920, 0)))
        .unwrap();
    client.roundtrip();
    let _second = client.bind_output(1);
    client.pump_until(|s| s.output_details[1].dones > 0);
    assert_eq!(client.state.removed_globals, Vec::<u32>::new());

    // Removing it produces a registry global_remove for the wl_output name.
    let wl_output_name = client
        .globals
        .contents()
        .clone_list()
        .iter()
        .filter(|g| g.interface == "wl_output")
        .nth(1)
        .unwrap()
        .name;
    handle.remove_output("Virtual-2").unwrap();
    client.pump_until(|s| s.removed_globals.contains(&wl_output_name));
}

#[test]
fn window_info_reports_preferred_scale() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    handle
        .update_output(spec("Virtual-1", (1920, 1080), 2.0, (0, 0)))
        .unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let _toplevel = client.map_toplevel(50, 50, "scaled.app", "scaled");
    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };
    assert_eq!(window.scale, 2.0);

    // A later scale change is reflected in WindowUpdated.
    handle
        .update_output(spec("Virtual-1", (1920, 1080), SCALE_1_5, (0, 0)))
        .unwrap();
    let buffer = client.shm_buffer(50, 50, 0xff00_0000);
    _toplevel.surface.attach(Some(&buffer), 0, 0);
    _toplevel.surface.commit();
    client.roundtrip();
    let ev = wait_for_event(&events, |e| {
        matches!(
            e,
            CompositorEvent::WindowUpdated { window, .. } if window.scale == SCALE_1_5
        )
    });
    let CompositorEvent::WindowUpdated { .. } = ev else {
        unreachable!()
    };
}
