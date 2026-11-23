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
//! │   · shutdown()       │          │  · xdg_shell (toplevel/popup) │
//! │                      │          │  · wl_output (virtual)        │
//! │   subscribe()        │  events  │  · Space + PopupManager       │
//! │   ◀───────────────── │◀─────────│  · SO_PEERCRED pid registry   │
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

mod compositor;
mod error;
mod event;
mod frame;
mod handle;
mod render;
mod state;

pub use compositor::Compositor;
pub use error::{Error, Result};
pub use event::{CompositorEvent, PopupInfo, Rect, Seq, SurfaceId, WindowId, WindowInfo};
pub use frame::Frame;
pub use handle::{CompositorHandle, Reply};
