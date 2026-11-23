//! Types carried by the [`CompositorHandle`](crate::CompositorHandle) event stream.
//!
//! Every event and every command acknowledgement is stamped with a monotonic
//! sequence number (`seq`) so callers can correlate commands, commits and
//! lifecycle transitions deterministically.

use serde::{Deserialize, Serialize};
use smithay::utils::Rectangle;

use crate::clipboard::SelectionKind;

/// A monotonic sequence number assigned by the compositor.
///
/// Sequence numbers start at `1` and increase by one for every emitted event
/// and every applied mutating command. Replies to read-only commands report
/// the sequence number the compositor had reached when the reply was produced.
pub type Seq = u64;

/// An axis-aligned rectangle, in the coordinate space documented by the field
/// that carries it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    /// Horizontal offset of the top-left corner.
    pub x: i32,
    /// Vertical offset of the top-left corner.
    pub y: i32,
    /// Width in pixels/logical units.
    pub width: i32,
    /// Height in pixels/logical units.
    pub height: i32,
}

impl<K> From<Rectangle<i32, K>> for Rect {
    fn from(r: Rectangle<i32, K>) -> Self {
        Self {
            x: r.loc.x,
            y: r.loc.y,
            width: r.size.w,
            height: r.size.h,
        }
    }
}

/// Opaque identifier of a mapped toplevel window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub u64);

/// Opaque identifier of a `wl_surface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfaceId(pub u64);

/// Structured metadata describing one mapped toplevel.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowInfo {
    /// Stable identifier for the lifetime of the mapping.
    pub id: WindowId,
    /// Identifier of the window's root `wl_surface`.
    pub surface_id: SurfaceId,
    /// `xdg_toplevel.app_id`, if the client set one.
    pub app_id: Option<String>,
    /// `xdg_toplevel.title`, if the client set one.
    pub title: Option<String>,
    /// Window geometry in logical compositor-space coordinates.
    pub geometry: Rect,
    /// Pixel dimensions of the buffer currently attached to the window's
    /// root surface, if any.
    pub buffer_size: Option<(i32, i32)>,
    /// `SO_PEERCRED` pid of the client that owns this window.
    pub pid: Option<u32>,
    /// Whether the toplevel currently carries the `activated` state.
    pub activated: bool,
    /// Whether the toplevel currently carries the `maximized` state.
    pub maximized: bool,
    /// The scale factor advertised to this window's surfaces: the highest
    /// scale of the virtual outputs it overlaps. This is the value reported
    /// through `wp_fractional_scale_v1` / `preferred_buffer_scale`.
    pub scale: f64,
}

/// Structured metadata describing one mapped `xdg_popup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PopupInfo {
    /// Identifier of the popup's `wl_surface`.
    pub surface_id: SurfaceId,
    /// The window the popup is anchored to, if its parent surface belongs to
    /// a mapped window.
    pub window_id: Option<WindowId>,
    /// The popup's positioner geometry: position relative to the parent's
    /// window geometry, and size in surface-local coordinates.
    pub geometry: Rect,
}

/// One retained damage record: the normalized buffer-space rectangles a
/// `wl_surface` commit damaged, stamped with the seq of its
/// [`CompositorEvent::SurfaceCommitted`] event.
///
/// The compositor keeps a bounded log of these (see
/// [`CompositorHandle::damage_since`](crate::CompositorHandle::damage_since))
/// so callers can diff regions without re-capturing frames.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DamageEntry {
    /// Sequence number of the `SurfaceCommitted` event that produced it.
    pub seq: Seq,
    /// The `wl_surface` that committed the damage.
    pub surface_id: SurfaceId,
    /// The window the surface belonged to at commit time, if any.
    pub window_id: Option<WindowId>,
    /// Damaged regions, normalized to buffer coordinates.
    pub damage: Vec<Rect>,
}

/// An event emitted by the compositor on its ordered event stream.
///
/// New variants are added as scry gains capabilities (popups, clipboard,
/// outputs, …); the enum is `#[non_exhaustive]` — match with a wildcard arm.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CompositorEvent {
    /// A toplevel mapped: its first buffer commit became visible.
    WindowMapped {
        /// Sequence number of this event.
        seq: Seq,
        /// Metadata snapshot at map time.
        window: WindowInfo,
    },
    /// Metadata or geometry of a mapped window changed on commit.
    WindowUpdated {
        /// Sequence number of this event.
        seq: Seq,
        /// Updated metadata snapshot.
        window: WindowInfo,
    },
    /// A mapped toplevel closed (destroyed, or unmapped via a null buffer).
    WindowClosed {
        /// Sequence number of this event.
        seq: Seq,
        /// Identifier of the window that closed.
        window_id: WindowId,
    },
    /// Keyboard focus moved to a different window (or was cleared).
    ///
    /// Focus moves when a window maps, on click-to-focus, through
    /// `xdg_activation_v1` activation or [`CompositorHandle::focus_window`],
    /// and falls back to the topmost remaining toplevel when the focused
    /// window closes.
    ///
    /// [`CompositorHandle::focus_window`]: crate::CompositorHandle::focus_window
    WindowFocused {
        /// Sequence number of this event.
        seq: Seq,
        /// The newly focused window, or `None` when focus was cleared.
        window_id: Option<WindowId>,
    },
    /// An `xdg_popup` mapped: its first buffer commit became visible.
    PopupMapped {
        /// Sequence number of this event.
        seq: Seq,
        /// Metadata snapshot at map time.
        popup: PopupInfo,
    },
    /// A mapped `xdg_popup` closed (destroyed, dismissed, or unmapped via a
    /// null buffer).
    PopupClosed {
        /// Sequence number of this event.
        seq: Seq,
        /// Identifier of the popup's `wl_surface`.
        surface_id: SurfaceId,
    },
    /// A `wl_surface` commit was applied.
    ///
    /// `damage` is normalized to buffer coordinates, already accounting for
    /// the attached buffer's scale, transform and viewport. If a commit
    /// attached a new buffer without declaring damage, the whole buffer is
    /// reported as damaged, matching wayland semantics.
    SurfaceCommitted {
        /// Sequence number of this event.
        seq: Seq,
        /// The surface that committed.
        surface_id: SurfaceId,
        /// The window the surface belongs to, if it is part of a mapped
        /// window's surface tree at event time.
        window_id: Option<WindowId>,
        /// Damaged regions in buffer coordinates.
        damage: Vec<Rect>,
    },
    /// A clipboard or primary selection changed ownership or offered MIME
    /// types.
    ///
    /// Emitted when a client sets or clears a selection and when the
    /// compositor itself sets or clears one via
    /// [`CompositorHandle::set_selection`](crate::CompositorHandle::set_selection).
    /// An empty `mime_types` means the selection was cleared.
    ClipboardUpdated {
        /// Sequence number of this event.
        seq: Seq,
        /// Which selection changed.
        selection: SelectionKind,
        /// MIME types the new selection offers; empty when cleared.
        mime_types: Vec<String>,
    },
}

impl CompositorEvent {
    /// The sequence number assigned to this event.
    pub fn seq(&self) -> Seq {
        match self {
            Self::WindowMapped { seq, .. }
            | Self::WindowUpdated { seq, .. }
            | Self::WindowClosed { seq, .. }
            | Self::WindowFocused { seq, .. }
            | Self::PopupMapped { seq, .. }
            | Self::PopupClosed { seq, .. }
            | Self::SurfaceCommitted { seq, .. }
            | Self::ClipboardUpdated { seq, .. } => *seq,
        }
    }
}
