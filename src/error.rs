//! Error type shared by the compositor and its control handle.

use std::io;

use smithay::reexports::wayland_server::BindError;
use thiserror::Error;

use crate::{clipboard::SelectionKind, event::WindowId};

/// Result alias for fallible `scry` operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by [`crate::Compositor`] and [`crate::CompositorHandle`].
#[derive(Debug, Error)]
pub enum Error {
    /// An operating-system level I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    /// Binding the `WAYLAND_DISPLAY` listening socket failed.
    #[error("failed to bind wayland socket: {0}")]
    SocketBind(#[from] BindError),

    /// Setting up the calloop event loop or wayland display failed.
    #[error("compositor initialization failed: {0}")]
    Backend(String),

    /// The compositor thread is not running (already shut down or crashed).
    #[error("compositor thread is not running")]
    Disconnected,

    /// The compositor did not answer a command in time.
    #[error("timed out waiting for a reply from the compositor")]
    Timeout,

    /// A command targeted a window id that no longer exists.
    #[error("unknown window id: {0:?}")]
    UnknownWindow(WindowId),

    /// A key name did not resolve to a keysym, or the keymap has no key
    /// producing that keysym.
    #[error("unknown key name or unmapped keysym: {0:?}")]
    UnknownKey(String),

    /// A modifier name was not recognised.
    #[error("unknown modifier: {0:?}")]
    UnknownModifier(String),

    /// A pointer button code outside the evdev button range (0x100–0x2FF).
    #[error("invalid pointer button code: {0:#x}")]
    InvalidButton(u32),

    /// A pointer motion or scroll carried a non-finite coordinate
    /// (`NaN`/`±∞`), which the protocol cannot represent.
    #[error("non-finite pointer coordinate: ({0}, {1})")]
    InvalidCoordinates(f64, f64),

    /// A window-local pointer target fell outside the window's surfaces
    /// (including its subsurfaces and popups).
    #[error("point is outside window {0:?}")]
    OutsideWindow(WindowId),

    /// Key or text input was requested while no surface has keyboard focus.
    #[error("no focused surface")]
    NoFocusedSurface,

    /// A character could not be typed through XKB because no key on the
    /// keymap produces its keysym (only the `zwp_text_input_v3` path can
    /// deliver arbitrary Unicode).
    #[error("character is not typeable through the xkb fallback: {0:?}")]
    UnmappableCharacter(char),

    /// A command targeted an output name that does not exist.
    #[error("unknown output: {0}")]
    UnknownOutput(String),

    /// An [`crate::OutputSpec`] failed validation.
    #[error("invalid output spec: {0}")]
    InvalidOutputSpec(String),

    /// A frame capture could not be produced.
    #[error("capture failed: {0}")]
    Capture(String),

    /// Encoding a captured frame failed.
    #[error("frame encoding failed: {0}")]
    Encode(#[from] image::ImageError),

    /// A read was requested on a selection that is currently empty.
    #[error("the {0} selection is empty")]
    EmptySelection(SelectionKind),

    /// The current selection does not offer the requested MIME type.
    #[error("the {kind} selection does not offer mime type {requested:?} (offers: {offered:?})")]
    MimeTypeNotOffered {
        /// Which selection was read.
        kind: SelectionKind,
        /// The MIME type that was requested.
        requested: String,
        /// MIME types the selection actually offers.
        offered: Vec<String>,
    },
}
