//! Types carried by the [`CompositorHandle`](crate::CompositorHandle) event stream.
//!
//! Every event and every command acknowledgement is stamped with a monotonic
//! sequence number (`seq`) so callers can correlate commands, commits and
//! lifecycle transitions deterministically.

use smithay::utils::Rectangle;

/// A monotonic sequence number assigned by the compositor.
///
/// Sequence numbers start at `1` and increase by one for every emitted event
/// and every applied mutating command. Replies to read-only commands report
/// the sequence number the compositor had reached when the reply was produced.
pub type Seq = u64;

/// An axis-aligned rectangle, in the coordinate space documented by the field
/// that carries it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

/// Opaque identifier of a `wl_surface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub u64);

/// Structured metadata describing one mapped toplevel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// Stable identifier for the lifetime of the mapping.
    pub id: WindowId,
    /// `xdg_toplevel.app_id`, if the client set one.
    pub app_id: Option<String>,
    /// `xdg_toplevel.title`, if the client set one.
    pub title: Option<String>,
    /// Window geometry in logical compositor-space coordinates.
    pub geometry: Rect,
    /// `SO_PEERCRED` pid of the client that owns this window.
    pub pid: Option<u32>,
    /// Whether the toplevel currently carries the `activated` state.
    pub activated: bool,
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
}

impl CompositorEvent {
    /// The sequence number assigned to this event.
    pub fn seq(&self) -> Seq {
        match self {
            Self::WindowMapped { seq, .. }
            | Self::WindowUpdated { seq, .. }
            | Self::WindowClosed { seq, .. }
            | Self::WindowFocused { seq, .. }
            | Self::SurfaceCommitted { seq, .. } => *seq,
        }
    }
}
