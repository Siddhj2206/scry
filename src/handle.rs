//! The typed control/observation bridge to the compositor thread.
//!
//! A [`CompositorHandle`] is a cheap, clonable, `Send + Sync` handle: commands
//! travel over a `calloop` channel into the compositor thread; replies and
//! events travel back over `std::sync::mpsc` channels.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender, channel},
    time::Duration,
};

use smithay::reexports::calloop::channel::Sender as CommandSender;

use crate::{
    clipboard::SelectionKind,
    error::{Error, Result},
    event::{CompositorEvent, Seq, WindowId, WindowInfo},
    frame_clock::FrameClockMode,
};

/// How long a command waits for the compositor's reply before giving up.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// A successful command acknowledgement.
///
/// `seq` is the compositor's sequence number at the point the command was
/// applied: any event with `seq >= reply.seq` may already reflect the command's
/// effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply<T> {
    /// Sequence number at which the command was applied.
    pub seq: Seq,
    /// The command's return value.
    pub value: T,
}

/// The payload carried back over a reply channel.
pub(crate) type Outcome<T> = std::result::Result<Reply<T>, Error>;

/// Commands processed by the compositor thread.
///
/// Kept crate-private: the public surface is the set of typed methods on
/// [`CompositorHandle`]. New sub-issues (frame capture, input injection,
/// outputs, clipboard, frame clock) add variants here plus matching methods.
#[derive(Debug)]
pub(crate) enum Command {
    /// Stop the event loop; acknowledged just before stopping.
    Shutdown { reply: Sender<Outcome<()>> },
    /// Register a new event subscriber.
    Subscribe { sender: Sender<CompositorEvent> },
    /// Snapshot all currently mapped windows.
    Windows {
        reply: Sender<Outcome<Vec<WindowInfo>>>,
    },
    /// Ask a toplevel to resize (xdg configure/ack roundtrip).
    ResizeWindow {
        id: WindowId,
        size: (i32, i32),
        reply: Sender<Outcome<()>>,
    },
    /// Ask a toplevel to close (`xdg_toplevel.close`).
    CloseWindow {
        id: WindowId,
        reply: Sender<Outcome<()>>,
    },
    /// Switch the virtual frame clock between realtime and paused modes.
    SetFrameClockMode {
        mode: FrameClockMode,
        reply: Sender<Outcome<()>>,
    },
    /// Advance the virtual frame clock by `duration`, releasing any pending
    /// `wl_surface.frame` callbacks with the stepped timestamp.
    StepFrameClock {
        duration: Duration,
        reply: Sender<Outcome<()>>,
    },
    /// Install a compositor-provided selection (clipboard or primary).
    SetSelection {
        kind: SelectionKind,
        mime: String,
        data: Vec<u8>,
        reply: Sender<Outcome<()>>,
    },
    /// Clear a selection (clipboard or primary).
    ClearSelection {
        kind: SelectionKind,
        reply: Sender<Outcome<()>>,
    },
    /// Read a selection's contents negotiated to `mime`.
    ReadSelection {
        kind: SelectionKind,
        mime: String,
        reply: Sender<Outcome<Vec<u8>>>,
    },
    /// List the MIME types a selection currently offers.
    SelectionMimeTypes {
        kind: SelectionKind,
        reply: Sender<Outcome<Vec<String>>>,
    },
}

/// The in-process handle used to drive and observe a running [`crate::Compositor`].
///
/// All methods are safe to call from any thread; multiple handles may command
/// the same compositor concurrently.
#[derive(Debug, Clone)]
pub struct CompositorHandle {
    pub(crate) command: CommandSender<Command>,
    socket_path: PathBuf,
    socket_name: std::ffi::OsString,
    runtime_dir: PathBuf,
}

impl CompositorHandle {
    pub(crate) fn new(
        command: CommandSender<Command>,
        socket_path: PathBuf,
        socket_name: std::ffi::OsString,
        runtime_dir: PathBuf,
    ) -> Self {
        Self {
            command,
            socket_path,
            socket_name,
            runtime_dir,
        }
    }

    /// Absolute path of the bound `WAYLAND_DISPLAY` unix socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The socket's file name (e.g. `wayland-0`), for `WAYLAND_DISPLAY`.
    pub fn socket_name(&self) -> &OsStr {
        &self.socket_name
    }

    /// The isolated runtime directory containing the socket.
    ///
    /// Point a client's `XDG_RUNTIME_DIR` here to let it discover the socket
    /// via `WAYLAND_DISPLAY` the usual way.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Subscribes to the ordered compositor event stream.
    ///
    /// Each subscriber receives every event emitted after its subscription
    /// command is processed, in sequence order.
    pub fn subscribe(&self) -> Result<Receiver<CompositorEvent>> {
        let (sender, receiver) = channel();
        self.command
            .send(Command::Subscribe { sender })
            .map_err(|_| Error::Disconnected)?;
        Ok(receiver)
    }

    /// Lists all currently mapped toplevel windows.
    pub fn windows(&self) -> Result<Reply<Vec<WindowInfo>>> {
        self.request(|reply| Command::Windows { reply })
    }

    /// Requests `xdg_toplevel` resize via the configure/ack lifecycle.
    ///
    /// The returned acknowledgement correlates with the `WindowUpdated` event
    /// emitted once the client commits at the new size.
    pub fn resize_window(&self, id: WindowId, width: i32, height: i32) -> Result<Reply<()>> {
        self.request(|reply| Command::ResizeWindow {
            id,
            size: (width, height),
            reply,
        })
    }

    /// Requests `xdg_toplevel.close` for a window.
    pub fn close_window(&self, id: WindowId) -> Result<Reply<()>> {
        self.request(|reply| Command::CloseWindow { id, reply })
    }

    /// Switches the virtual frame clock between [`FrameClockMode::Realtime`]
    /// (60Hz cadence, the default) and [`FrameClockMode::Paused`] (callbacks
    /// held until [`Self::step_frame_clock`] advances the clock).
    ///
    /// The virtual presentation clock is never reset, so `wl_callback.done`
    /// timestamps stay monotonic across mode switches.
    pub fn set_frame_clock_mode(&self, mode: FrameClockMode) -> Result<Reply<()>> {
        self.request(|reply| Command::SetFrameClockMode { mode, reply })
    }

    /// Advances the virtual frame clock by `duration`, firing every pending
    /// `wl_surface.frame` callback once with the stepped presentation
    /// timestamp.
    ///
    /// This is the deterministic counterpart to realtime mode for tests:
    /// while paused, clients' animations advance exactly one step per call.
    /// Stepping while realtime is also allowed and simply injects an extra
    /// advancement on top of the regular ticks.
    pub fn step_frame_clock(&self, duration: Duration) -> Result<Reply<()>> {
        self.request(|reply| Command::StepFrameClock { duration, reply })
    }

    /// Installs a compositor-provided selection.
    ///
    /// Clients holding keyboard/selection focus are offered `mime` and receive
    /// `data` when they request it. Replaces the current `kind` selection and
    /// emits [`CompositorEvent::ClipboardUpdated`].
    pub fn set_selection(
        &self,
        kind: SelectionKind,
        mime: String,
        data: Vec<u8>,
    ) -> Result<Reply<()>> {
        self.request(|reply| Command::SetSelection {
            kind,
            mime,
            data,
            reply,
        })
    }

    /// Clears the current `kind` selection.
    ///
    /// Clients are notified their offer is gone and a
    /// [`CompositorEvent::ClipboardUpdated`] with empty `mime_types` is emitted.
    pub fn clear_selection(&self, kind: SelectionKind) -> Result<Reply<()>> {
        self.request(|reply| Command::ClearSelection { kind, reply })
    }

    /// Reads the current `kind` selection negotiated to `mime`.
    ///
    /// Returns the exact bytes the owning client (or the compositor's own
    /// store, for selections installed via [`Self::set_selection`]) supplies.
    /// Fails with [`Error::EmptySelection`] when nothing is set and
    /// [`Error::MimeTypeNotOffered`] when `mime` is not among the offered
    /// types — neither hangs the compositor event loop.
    pub fn read_selection(&self, kind: SelectionKind, mime: String) -> Result<Reply<Vec<u8>>> {
        self.request(|reply| Command::ReadSelection { kind, mime, reply })
    }

    /// The MIME types the current `kind` selection offers; empty when unset.
    pub fn selection_mime_types(&self, kind: SelectionKind) -> Result<Reply<Vec<String>>> {
        self.request(|reply| Command::SelectionMimeTypes { kind, reply })
    }

    /// Shuts the compositor down: the event loop stops, all clients are
    /// disconnected and the socket is removed.
    ///
    /// It is safe (though unnecessary) to call this before dropping the
    /// [`crate::Compositor`]; the drop performs the same shutdown.
    pub fn shutdown(&self) -> Result<Reply<()>> {
        self.request(|reply| Command::Shutdown { reply })
    }

    /// Sends a command and waits for its reply.
    fn request<T>(&self, build: impl FnOnce(Sender<Outcome<T>>) -> Command) -> Result<Reply<T>> {
        let (reply, receiver) = channel();
        self.command
            .send(build(reply))
            .map_err(|_| Error::Disconnected)?;
        match receiver.recv_timeout(COMMAND_TIMEOUT) {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Error::Disconnected),
        }
    }
}
