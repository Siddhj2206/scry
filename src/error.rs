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
