//! Frame capture: occlusion-free per-window and full-output compositing of
//! `wl_shm` content, plus PNG/WebP encoding round-trips.

mod common;

use common::{TestClient, Toplevel, wait_for_event};
use scry::{Compositor, CompositorEvent, Frame, WindowId};

/// Reads the RGBA8 pixel at `(x, y)` out of a captured frame.
fn pixel_at(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * frame.width + x) * 4) as usize;
    frame.pixels[offset..offset + 4].try_into().unwrap()
}

/// 0xAARRGGBB -> [R, G, B, A] bytes (the capture's straight-alpha layout).
fn rgba(argb: u32) -> [u8; 4] {
    [
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        (argb & 0xff) as u8,
        ((argb >> 24) & 0xff) as u8,
    ]
}

/// Maps a toplevel with `pixels` as its `Argb8888` buffer content and waits
/// for `WindowMapped`, returning the window id and the live client objects.
fn map_pattern_window(
    client: &mut TestClient,
    events: &std::sync::mpsc::Receiver<CompositorEvent>,
    width: i32,
    height: i32,
    pixels: &[u32],
) -> (WindowId, Toplevel) {
    let surface = client.compositor.create_surface(&client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());
    let buffer = client.shm_buffer_pixels(
        width,
        height,
        pixels,
        wayland_client::protocol::wl_shm::Format::Argb8888,
    );
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
        window.id,
        Toplevel {
            surface,
            xsurface,
            toplevel,
            buffer,
        },
    )
}

#[test]
fn capture_window_reads_back_exact_rgba_pixels() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());

    // Four quadrants of distinct opaque colors.
    let (w, h) = (32, 32);
    let mut pixels = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let argb = match (x < w / 2, y < h / 2) {
                (true, true) => 0xffff_0000,   // red
                (false, true) => 0xff00_ff00,  // green
                (true, false) => 0xff00_00ff,  // blue
                (false, false) => 0xffff_ffff, // white
            };
            pixels[(y * w + x) as usize] = argb;
        }
    }
    let (id, _toplevel) = map_pattern_window(&mut client, &events, w, h, &pixels);

    let reply = handle.capture_window(id).expect("capture_window failed");
    let frame = reply.value;
    assert_eq!((frame.width, frame.height), (w as u32, h as u32));
    assert_eq!(frame.pixels.len(), (w * h * 4) as usize);

    assert_eq!(pixel_at(&frame, 0, 0), rgba(0xffff_0000));
    assert_eq!(pixel_at(&frame, w as u32 - 1, 0), rgba(0xff00_ff00));
    assert_eq!(pixel_at(&frame, 0, h as u32 - 1), rgba(0xff00_00ff));
    assert_eq!(
        pixel_at(&frame, w as u32 - 1, h as u32 - 1),
        rgba(0xffff_ffff)
    );
}

/// Decodes an encoded image back to `(width, height, rgba8_pixels)`.
fn decode(bytes: &[u8], format: image::ImageFormat) -> (u32, u32, Vec<u8>) {
    use image::ImageDecoder as _;
    use std::io::Cursor;
    let decoder: Box<dyn image::ImageDecoder> = match format {
        image::ImageFormat::Png => Box::new(
            image::codecs::png::PngDecoder::new(Cursor::new(bytes))
                .expect("png decode init failed"),
        ),
        image::ImageFormat::WebP => Box::new(
            image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .expect("webp decode init failed"),
        ),
        other => panic!("unsupported test format {other:?}"),
    };
    let (width, height) = decoder.dimensions();
    let mut pixels = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut pixels).expect("decode failed");
    (width, height, pixels)
}

#[test]
fn png_and_webp_captures_round_trip_identical_pixels() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let (w, h) = (24, 16);
    let mut pixels = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            pixels[(y * w + x) as usize] = if (x + y) % 2 == 0 {
                0xff12_3456
            } else {
                0xfff6_5478
            };
        }
    }
    let (id, _toplevel) = map_pattern_window(&mut client, &events, w, h, &pixels);

    let expected: Vec<[u8; 4]> = pixels.iter().map(|&argb| rgba(argb)).collect();

    for (encoded, format) in [
        (
            handle.capture_window_png(id).expect("png capture failed"),
            image::ImageFormat::Png,
        ),
        (
            handle.capture_window_webp(id).expect("webp capture failed"),
            image::ImageFormat::WebP,
        ),
    ] {
        let (width, height, decoded) = decode(&encoded.value, format);
        assert_eq!((width, height), (w as u32, h as u32));
        let decoded: Vec<[u8; 4]> = decoded.as_chunks::<4>().0.to_vec();
        assert_eq!(decoded, expected);
    }
}

#[test]
fn xrgb8888_buffers_capture_as_opaque() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let surface = client.compositor.create_surface(&client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let _toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());

    // XRGB8888: the X channel is meaningless; capture must report opaque.
    let buffer = client.shm_buffer_pixels(
        16,
        16,
        &vec![0x00de_adbe; 256], // X = 0x00 must still read back as opaque
        wayland_client::protocol::wl_shm::Format::Xrgb8888,
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };

    let frame = handle
        .capture_window(window.id)
        .expect("capture_window failed")
        .value;
    for &pixel in frame.pixels.as_chunks::<4>().0 {
        assert_eq!(pixel, [0xde, 0xad, 0xbe, 0xff]);
    }
}

#[test]
fn subsurfaces_composite_at_offsets_in_stacking_order() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(32, 32, "org.scry.Subs", "Subs");
    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };
    let id = window.id;

    // The mapped toplevel buffer is opaque 0xff33_6699.
    let parent_color = rgba(0xff33_6699);

    let subcompositor = client.subcompositor.clone().expect("no subcompositor");

    // Subsurface below the parent: hidden under the opaque parent.
    let below_surface = client.compositor.create_surface(&client.qh, ());
    let below = subcompositor.get_subsurface(&below_surface, &toplevel.surface, &client.qh, ());
    below.set_desync();
    below.place_below(&toplevel.surface);
    let below_buffer = client.shm_buffer(8, 8, 0xff00_ff00);
    below_surface.attach(Some(&below_buffer), 0, 0);
    below_surface.commit();
    client.roundtrip();

    // Subsurface above the parent at an offset, opaque green.
    let above_surface = client.compositor.create_surface(&client.qh, ());
    let above = subcompositor.get_subsurface(&above_surface, &toplevel.surface, &client.qh, ());
    above.set_desync();
    above.set_position(16, 16);
    let above_buffer = client.shm_buffer(8, 8, 0xff00_ff00);
    above_surface.attach(Some(&above_buffer), 0, 0);
    above_surface.commit();
    client.roundtrip();

    let frame = handle.capture_window(id).expect("capture failed").value;

    // Above-subsurface covers (16,16)-(24,24) in green.
    assert_eq!(pixel_at(&frame, 20, 20), rgba(0xff00_ff00));
    // Everything else is the parent color: the below-subsurface stays hidden
    // under the opaque parent at (0,0)-(8,8).
    assert_eq!(pixel_at(&frame, 4, 4), parent_color);
    assert_eq!(pixel_at(&frame, 8, 8), parent_color);
    assert_eq!(pixel_at(&frame, 28, 8), parent_color);

    // A subsurface extending beyond the parent grows the captured bbox.
    let wide_surface = client.compositor.create_surface(&client.qh, ());
    let wide = subcompositor.get_subsurface(&wide_surface, &toplevel.surface, &client.qh, ());
    wide.set_desync();
    wide.set_position(-8, -8);
    let wide_buffer = client.shm_buffer(4, 4, 0xffff_ff00);
    wide_surface.attach(Some(&wide_buffer), 0, 0);
    wide_surface.commit();
    client.roundtrip();

    let frame = handle.capture_window(id).expect("capture failed").value;
    // bbox: (-8,-8,4,4) merged with (0,0,32,32) → 40x40 origin (-8,-8).
    assert_eq!((frame.width, frame.height), (40, 40));
    assert_eq!(pixel_at(&frame, 0, 0), rgba(0xffff_ff00));
    // Toplevel origin moved to (8,8) in frame coords.
    assert_eq!(pixel_at(&frame, 8, 8), parent_color);
    assert_eq!(pixel_at(&frame, 8 + 20, 8 + 20), rgba(0xff00_ff00));
}

#[test]
fn alpha_blended_subsurface_over_opaque_parent() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    // Opaque blue parent 16x16.
    let (id, _toplevel) = map_pattern_window(&mut client, &events, 16, 16, &vec![0xff00_00ff; 256]);

    let subcompositor = client.subcompositor.clone().unwrap();
    let sub_surface = client.compositor.create_surface(&client.qh, ());
    let subsurface = subcompositor.get_subsurface(&sub_surface, &_toplevel.surface, &client.qh, ());
    subsurface.set_desync();
    subsurface.set_position(4, 4);

    // Premultiplied ARGB: (64, 32, 16) at alpha 128 — channels are already
    // multiplied down, as wl_shm argb requires.
    let buffer = client.shm_buffer(8, 8, 0x8040_2010);
    sub_surface.attach(Some(&buffer), 0, 0);
    sub_surface.commit();
    client.roundtrip();

    let frame = handle.capture_window(id).expect("capture failed").value;

    // Blended: src + dst * (255 - 128)/255 over opaque blue (0,0,255):
    //   r = 64, g = 32, b = 16 + 127 = 143, a = 255.
    let blended = pixel_at(&frame, 8, 8);
    let expected = [64u8, 32, 143, 255];
    for (got, want) in blended.iter().zip(expected) {
        assert!(
            got.abs_diff(want) <= 1,
            "blended pixel {blended:?} != {expected:?}"
        );
    }
    // Outside the subsurface is still pure blue.
    assert_eq!(pixel_at(&frame, 1, 1), rgba(0xff00_00ff));
}

#[test]
fn popup_maps_composites_and_closes_with_events() {
    use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner};

    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let toplevel = client.map_toplevel(32, 32, "org.scry.Pop", "Pop");
    let mapped = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = mapped else {
        unreachable!()
    };

    // Popup: 1x1 anchor rect at the parent's top-left, popup extends
    // bottom-right, offset by (10, 20).
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
    let popup = popup_xsurface.get_popup(Some(&toplevel.xsurface), &positioner, &client.qh, ());
    positioner.destroy();

    // The compositor reports the popup geometry via xdg_popup.configure;
    // like a real client, ack it (auto) before mapping with a buffer commit.
    client.pump_until(|s| !s.popup_configures.is_empty());
    let (px, py, pw, ph) = client.state.popup_configures[0];
    assert_eq!((px, py, pw, ph), (10, 20, 16, 16));

    let popup_buffer = client.shm_buffer(16, 16, 0xffff_00ff);
    popup_surface.attach(Some(&popup_buffer), 0, 0);
    popup_surface.commit();
    client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::PopupMapped { .. })
    });
    let CompositorEvent::PopupMapped { popup: info, .. } = ev else {
        unreachable!()
    };
    assert_eq!(info.window_id, Some(window.id));
    assert_eq!(
        info.geometry,
        scry::Rect {
            x: 10,
            y: 20,
            width: 16,
            height: 16
        }
    );

    // The window capture now includes the popup at its position.
    let frame = handle.capture_window(window.id).expect("capture").value;
    assert_eq!((frame.width, frame.height), (32, 36));
    assert_eq!(pixel_at(&frame, 12, 22), rgba(0xffff_00ff));
    assert_eq!(pixel_at(&frame, 12, 19), rgba(0xff33_6699));

    // Destroying the xdg_popup emits PopupClosed.
    popup.destroy();
    client.roundtrip();
    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::PopupClosed { .. })
    });
    let CompositorEvent::PopupClosed { surface_id, .. } = ev else {
        unreachable!()
    };
    assert_eq!(surface_id, info.surface_id);

    let _ = popup_xsurface;
    let _ = popup_surface;
    let _ = popup_buffer;
    let _ = xdg_popup::Event::PopupDone; // silence unused import when API shifts
}

#[test]
fn output_capture_z_order_and_occlusion_free_window_capture() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    // Bottom window: 40x40 red; top window (mapped later): 20x20 green.
    let (red, _t1) = map_pattern_window(&mut client, &events, 40, 40, &vec![0xffff_0000; 1600]);
    let (green, _t2) = map_pattern_window(&mut client, &events, 20, 20, &vec![0xff00_ff00; 400]);

    let frame = handle
        .capture_output("Virtual-1")
        .expect("capture_output failed")
        .value;
    assert_eq!((frame.width, frame.height), (1920, 1080));
    // Overlap region shows the topmost (green) window.
    assert_eq!(pixel_at(&frame, 10, 10), rgba(0xff00_ff00));
    // Red-only region still shows red.
    assert_eq!(pixel_at(&frame, 30, 30), rgba(0xffff_0000));
    // Outside all windows: opaque black background.
    assert_eq!(pixel_at(&frame, 500, 500), [0, 0, 0, 255]);

    // Per-window capture ignores the occlusion entirely.
    let frame = handle.capture_window(red).expect("capture failed").value;
    assert_eq!((frame.width, frame.height), (40, 40));
    for &pixel in frame.pixels.as_chunks::<4>().0 {
        assert_eq!(pixel, rgba(0xffff_0000));
    }
    let frame = handle.capture_window(green).expect("capture failed").value;
    assert_eq!((frame.width, frame.height), (20, 20));
    for &pixel in frame.pixels.as_chunks::<4>().0 {
        assert_eq!(pixel, rgba(0xff00_ff00));
    }
}

#[test]
fn unknown_capture_targets_return_structured_errors() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();

    let err = handle.capture_window(scry::WindowId(4242)).unwrap_err();
    assert!(matches!(
        err,
        scry::Error::UnknownWindow(scry::WindowId(4242))
    ));

    let err = handle.capture_output("NoSuchOutput-9").unwrap_err();
    assert!(matches!(err, scry::Error::UnknownOutput(_)));
}

#[test]
fn hidpi_buffer_scale_captures_at_logical_size() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = TestClient::connect(handle.socket_path());
    let surface = client.compositor.create_surface(&client.qh, ());
    let xsurface = client.wm_base.get_xdg_surface(&surface, &client.qh, ());
    let _toplevel = xsurface.get_toplevel(&client.qh, ());
    surface.commit();
    client.pump_until(|s| !s.configure_serials.is_empty());

    // 64x64 buffer at scale 2 -> 32x32 logical surface.
    let mut pixels = vec![0u32; 64 * 64];
    for y in 0..64i32 {
        for x in 0..64i32 {
            pixels[(y * 64 + x) as usize] = if x < 32 { 0xffff_0000 } else { 0xff00_00ff };
        }
    }
    let buffer = client.shm_buffer_pixels(
        64,
        64,
        &pixels,
        wayland_client::protocol::wl_shm::Format::Argb8888,
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.set_buffer_scale(2);
    surface.commit();
    client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let CompositorEvent::WindowMapped { window, .. } = ev else {
        unreachable!()
    };

    let frame = handle.capture_window(window.id).expect("capture").value;
    assert_eq!((frame.width, frame.height), (32, 32));
    assert_eq!(pixel_at(&frame, 4, 16), rgba(0xffff_0000));
    assert_eq!(pixel_at(&frame, 28, 16), rgba(0xff00_00ff));
}

#[test]
fn frame_pixel_panics_on_out_of_bounds_coordinates() {
    let frame = Frame {
        width: 2,
        height: 2,
        pixels: vec![0u8; 2 * 2 * 4],
    };
    // In-bounds reads work.
    let _ = frame.pixel(1, 1);
    // An out-of-range x would silently wrap into the next row without the
    // bounds check; both coordinates must panic instead.
    let wrapped = std::panic::catch_unwind(|| frame.pixel(2, 0));
    assert!(wrapped.is_err(), "x == width must panic");
    let overflow = std::panic::catch_unwind(|| frame.pixel(0, 2));
    assert!(overflow.is_err(), "y == height must panic");
}
