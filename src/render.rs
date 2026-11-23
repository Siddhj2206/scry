//! Pure-Rust compositing of smithay surface trees via `tiny-skia`.
//!
//! Everything here runs on the compositor thread, called from
//! [`crate::state`] while answering capture commands. Rendering never goes
//! through a GPU renderer: `wl_shm` buffer contents are copied into
//! [`tiny_skia::Pixmap`]s (premultiplied RGBA8) and alpha-blended with affine
//! transforms, honouring buffer scale, buffer transform, viewporter
//! crop/scale, subsurface ordering and popup placement.

use smithay::{
    backend::renderer::utils::{RendererSurfaceState, RendererSurfaceStateUserData, SurfaceView},
    desktop::{PopupManager, Space, Window},
    output::Output,
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm, wl_surface::WlSurface},
    utils::{Buffer as BufferCoord, Logical, Point, Size, Transform},
    wayland::{
        compositor::{SurfaceData, TraversalAction, with_surface_tree_upward},
        seat::WaylandFocus,
        shm::{BufferData, with_buffer_contents},
    },
};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint};
use tracing::warn;

use crate::{error::Error, frame::Frame};

/// Renders a window's complete surface tree — toplevel, subsurfaces and all
/// tracked child popups — into a fresh transparent frame.
///
/// The capture is occlusion-free: sibling windows and output edges are
/// ignored. The frame covers `window.bbox_with_popups()`, so it may extend
/// beyond the toplevel's own geometry.
pub(crate) fn render_window(window: &Window) -> Result<Frame, Error> {
    let bbox = window.bbox_with_popups();
    let Some(mut pixmap) = Pixmap::new(
        u32::try_from(bbox.size.w).unwrap_or(0),
        u32::try_from(bbox.size.h).unwrap_or(0),
    ) else {
        return Err(Error::Capture("window has no compositable content".into()));
    };

    if let Some(surface) = window.wl_surface() {
        draw_surface_tree(&mut pixmap, &surface, Point::from((0, 0)) - bbox.loc);
        for (popup, location) in PopupManager::popups_for_surface(&surface) {
            let origin = window.geometry().loc + location - popup.geometry().loc - bbox.loc;
            draw_surface_tree(&mut pixmap, popup.wl_surface(), origin);
        }
    }
    Ok(Frame::from_pixmap(pixmap))
}

/// Renders every visible window overlapping `output`, in z-order, into a
/// frame covering the output's logical geometry over opaque black.
pub(crate) fn render_output(space: &Space<Window>, output: &Output) -> Result<Frame, Error> {
    let geometry = space
        .output_geometry(output)
        .ok_or_else(|| Error::Capture("output is not mapped".into()))?;
    let Some(mut pixmap) = Pixmap::new(
        u32::try_from(geometry.size.w).unwrap_or(0),
        u32::try_from(geometry.size.h).unwrap_or(0),
    ) else {
        return Err(Error::Capture("output has no compositable area".into()));
    };
    pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255));

    // `space.elements()` iterates in z-order, back to front.
    for window in space.elements() {
        let Some(element_loc) = space.element_location(window) else {
            continue;
        };
        let render_loc = element_loc - window.geometry().loc - geometry.loc;
        if !space
            .element_bbox(window)
            .map(|bbox| bbox.overlaps(geometry))
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(surface) = window.wl_surface() {
            draw_surface_tree(&mut pixmap, &surface, render_loc);
            for (popup, location) in PopupManager::popups_for_surface(&surface) {
                let origin = element_loc + location - popup.geometry().loc - geometry.loc;
                draw_surface_tree(&mut pixmap, popup.wl_surface(), origin);
            }
        }
    }
    Ok(Frame::from_pixmap(pixmap))
}

/// Draws `root` and its subsurface tree into `target`, treating `origin` as
/// the target position of `root`'s surface-local `(0, 0)`.
fn draw_surface_tree(target: &mut Pixmap, root: &WlSurface, origin: Point<i32, Logical>) {
    let target = &mut target.as_mut();
    // `with_surface_tree_upward` iterates bottom-to-top — paint order.
    // (`with_surface_tree_downward` is top-to-bottom, used for hit-testing.)
    with_surface_tree_upward(
        root,
        origin,
        |_surface, states, loc| {
            let mut loc = *loc;
            if let Some(view) = surface_view(states) {
                // Children are positioned relative to this surface's draw
                // origin, matching `bbox_from_surface_tree`.
                loc += view.offset;
                TraversalAction::DoChildren(loc)
            } else {
                // An unmapped surface hides its whole subtree.
                TraversalAction::SkipChildren
            }
        },
        |_surface, states, loc| {
            draw_surface(target, states, *loc);
        },
        |_, _, _| true,
    );
}

/// The [`RendererSurfaceState`] view of a surface, if it has one.
fn surface_view(states: &SurfaceData) -> Option<SurfaceView> {
    states
        .data_map
        .get::<RendererSurfaceStateUserData>()
        .and_then(|data| data.lock().unwrap().view())
}

/// Draws a single surface's buffer at `loc` (its tree-relative position,
/// before `view.offset` is applied).
fn draw_surface(
    target: &mut tiny_skia::PixmapMut<'_>,
    states: &SurfaceData,
    loc: Point<i32, Logical>,
) {
    let Some(parts) = states
        .data_map
        .get::<RendererSurfaceStateUserData>()
        .and_then(|data| surface_parts(&data.lock().unwrap()))
    else {
        return;
    };
    let Some(buffer_pixmap) = buffer_pixmap(&parts.buffer) else {
        return;
    };
    let view = parts.view;
    let position = loc + view.offset;
    if view.dst.w <= 0 || view.dst.h <= 0 || view.src.size.w <= 0.0 || view.src.size.h <= 0.0 {
        return;
    }

    // Affine: buffer pixels -> logical surface space -> target.
    let buffer_size: Size<i32, BufferCoord> =
        (buffer_pixmap.width() as i32, buffer_pixmap.height() as i32).into();
    let buf_to_logical = buffer_to_logical_matrix(parts.transform, buffer_size, parts.scale);
    let logical_to_target =
        tiny_skia::Transform::from_translate(-view.src.loc.x as f32, -view.src.loc.y as f32)
            .post_concat(tiny_skia::Transform::from_scale(
                view.dst.w as f32 / view.src.size.w as f32,
                view.dst.h as f32 / view.src.size.h as f32,
            ))
            .post_concat(tiny_skia::Transform::from_translate(
                position.x as f32,
                position.y as f32,
            ));
    let transform = buf_to_logical.post_concat(logical_to_target);

    // When the view crops (`wp_viewport.set_source`), pixels outside `src`
    // map outside `dst`; clip them away so they cannot bleed.
    let cropped = view.src.loc != (0.0, 0.0).into() || view.src.size != parts.logical_size.to_f64();
    let mask = if cropped {
        clip_mask(target.width(), target.height(), position, view.dst)
    } else {
        None
    };

    target.draw_pixmap(
        0,
        0,
        buffer_pixmap.as_ref(),
        &PixmapPaint {
            // Nearest sampling keeps compositing exact: every output pixel is
            // one specific source pixel, even under scale factors.
            quality: FilterQuality::Nearest,
            ..PixmapPaint::default()
        },
        transform,
        mask.as_ref(),
    );
}

/// Everything needed to composite one surface's buffer.
struct SurfaceParts {
    buffer: WlBuffer,
    scale: i32,
    transform: Transform,
    view: SurfaceView,
    logical_size: Size<i32, Logical>,
}

fn surface_parts(rss: &RendererSurfaceState) -> Option<SurfaceParts> {
    let buffer = rss.buffer().map(|buffer| (**buffer).clone())?;
    Some(SurfaceParts {
        buffer,
        scale: rss.buffer_scale(),
        transform: rss.buffer_transform(),
        view: rss.view()?,
        logical_size: rss.buffer_size()?,
    })
}

/// Copies an `wl_shm` buffer into a tiny-skia pixmap (premultiplied RGBA8).
///
/// `Argb8888`/`Xrgb8888` are little-endian `B,G,R,A`/`B,G,R,X` in memory and
/// `argb` content is already alpha-premultiplied, so a byte swizzle is all
/// that is needed. Other formats are skipped with a warning.
fn buffer_pixmap(buffer: &WlBuffer) -> Option<Pixmap> {
    with_buffer_contents(buffer, copy_buffer)
        .map_err(|err| warn!("failed to access shm buffer contents: {err:?}"))
        .ok()
        .flatten()
}

fn copy_buffer(ptr: *const u8, len: usize, data: BufferData) -> Option<Pixmap> {
    let (w, h, stride, offset) = (
        data.width as usize,
        data.height as usize,
        data.stride as usize,
        data.offset as usize,
    );
    if w == 0 || h == 0 {
        return None;
    }
    let byte_len = (h - 1).checked_mul(stride)?.checked_add(w * 4)?;
    offset.checked_add(byte_len).filter(|end| *end <= len)?;

    let (swap_rb, opaque) = match data.format {
        wl_shm::Format::Argb8888 => (true, false),
        wl_shm::Format::Xrgb8888 => (true, true),
        wl_shm::Format::Abgr8888 => (false, false),
        wl_shm::Format::Xbgr8888 => (false, true),
        format => {
            warn!("unsupported shm buffer format for capture: {format:?}");
            return None;
        }
    };

    let mut pixmap = Pixmap::new(w as u32, h as u32)?;
    // SAFETY: `with_buffer_contents` guarantees `ptr..ptr+len` is a readable
    // shm mapping for the duration of this closure; `offset + byte_len` was
    // bounds-checked above.
    let source = unsafe { std::slice::from_raw_parts(ptr.add(offset), byte_len) };
    let dest = pixmap.data_mut();
    for row in 0..h {
        let (src_row, _) = source[row * stride..row * stride + w * 4].as_chunks::<4>();
        let (dst_row, _) = dest[row * w * 4..(row + 1) * w * 4].as_chunks_mut::<4>();
        for (src, dst) in src_row.iter().zip(dst_row.iter_mut()) {
            if swap_rb {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
            } else {
                dst[0] = src[0];
                dst[1] = src[1];
                dst[2] = src[2];
            }
            dst[3] = if opaque { 0xff } else { src[3] };
        }
    }
    Some(pixmap)
}

/// Affine mapping from buffer pixel coordinates to logical surface
/// coordinates: `invert(buffer_transform)` applied inside the buffer
/// rectangle, divided by the buffer scale — the exact inverse of smithay's
/// `to_buffer` conversion.
fn buffer_to_logical_matrix(
    transform: Transform,
    buffer_size: Size<i32, BufferCoord>,
    scale: i32,
) -> tiny_skia::Transform {
    let inverse = transform.invert();
    let area = buffer_size.to_f64();
    let map = |x: f64, y: f64| {
        let point = inverse.transform_point_in(Point::from((x, y)), &area);
        (point.x / f64::from(scale), point.y / f64::from(scale))
    };
    let (ax, ay) = map(0.0, 0.0);
    let (bx, by) = map(1.0, 0.0);
    let (cx, cy) = map(0.0, 1.0);
    tiny_skia::Transform::from_row(
        (bx - ax) as f32,
        (by - ay) as f32,
        (cx - ax) as f32,
        (cy - ay) as f32,
        ax as f32,
        ay as f32,
    )
}

/// An alpha mask restricting drawing to `rect` within a `w * h` target.
fn clip_mask(
    w: u32,
    h: u32,
    position: Point<i32, Logical>,
    size: Size<i32, Logical>,
) -> Option<tiny_skia::Mask> {
    let mut mask = tiny_skia::Mask::new(w, h)?;
    let rect = tiny_skia::Rect::from_xywh(
        position.x as f32,
        position.y as f32,
        size.w as f32,
        size.h as f32,
    )?;
    let path = tiny_skia::PathBuilder::from_rect(rect);
    mask.fill_path(
        &path,
        tiny_skia::FillRule::Winding,
        false,
        tiny_skia::Transform::default(),
    );
    Some(mask)
}
