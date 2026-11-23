//! `scry` — a headless Wayland compositor for automated UI testing and
//! computer-use agents.
//!
//! # Architecture
//!
//! ```text
//!  test/automation code              compositor thread (calloop)
//! ┌──────────────────────┐          ┌──────────────────────────────┐
//! │   CompositorHandle   │ commands │  smithay protocol state       │
//! │   · windows()        │─────────▶│  · wl_compositor/subcompositor│
//! │   · resize/close     │          │  · wl_shm                     │
//! │   · set/read         │          │  · xdg_shell (toplevel/popup) │
//! │     selection        │          │    + xdg_decoration           │
//! │   · update_output    │          │  · wl_output (virtual) +      │
//! │   · shutdown()       │          │    wp_viewporter +            │
//! │                      │          │    wp_fractional_scale_v1     │
//! │   subscribe()        │  events  │  · wl_seat + clipboard        │
//! │   ◀───────────────── │◀─────────│    (wl_data_device + primary) │
//! │                      │          │  · Space + PopupManager       │
//! │                      │          │  · SO_PEERCRED pid registry   │
//! └──────────────────────┘          └────────────▲─────────────────┘
//!                                                │ real Unix socket
//!                                          wayland clients
//! ```
//!
//! A [`Compositor`] runs entirely headless: it needs no GPU, no display
//! server, and only `libxkbcommon` as a system C dependency. Clients connect
//! to a real `WAYLAND_DISPLAY` socket inside an isolated runtime directory,
//! while automation observes and drives the instance through
//! [`CompositorHandle`].
//!
//! # Usage
//!
//! ```no_run
//! # fn main() -> scry::Result<()> {
//! let compositor = scry::Compositor::spawn()?;
//! let handle = compositor.handle().clone();
//!
//! // Point a client's environment at the socket:
//! //   XDG_RUNTIME_DIR=<handle.runtime_dir()>  WAYLAND_DISPLAY=<handle.socket_name()>
//! println!("socket: {}", handle.socket_path().display());
//!
//! let events = handle.subscribe()?;
//! // … connect wayland clients, then observe:
//! //   WindowMapped / WindowUpdated / WindowClosed / SurfaceCommitted
//! # Ok(())
//! # }
//! ```

mod clipboard;
mod compositor;
/// The control daemon: Unix-socket JSON-RPC 2.0 API and workspace lifecycle.
pub mod daemon;
mod error;
mod event;
mod frame;
mod frame_clock;
mod handle;
/// Synthetic input types (`wl_seat` pointer/keyboard/text injection).
pub mod input;
mod output;
mod render;
mod state;

pub use clipboard::SelectionKind;
pub use compositor::Compositor;
pub use error::{Error, Result};
pub use event::{CompositorEvent, PopupInfo, Rect, Seq, SurfaceId, WindowId, WindowInfo};
pub use frame::Frame;
pub use frame_clock::FrameClockMode;
pub use handle::{CompositorHandle, Reply};
pub use output::{OutputInfo, OutputSpec};
