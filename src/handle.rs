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
    error::{Error, Result},
    event::{CompositorEvent, Seq, WindowId, WindowInfo},
    frame::Frame,
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

impl<T> Reply<T> {
    /// Maps the reply value, keeping `seq` and propagating errors.
    pub(crate) fn try_map<U, E>(
        self,
        f: impl FnOnce(T) -> std::result::Result<U, E>,
    ) -> Result<Reply<U>>
    where
        Error: From<E>,
    {
        Ok(Reply {
            seq: self.seq,
            value: f(self.value)?,
        })
    }
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
    /// Composite a window's complete surface tree (root, subsurfaces and
    /// popups) into an occlusion-free frame.
    CaptureWindow {
        id: WindowId,
        reply: Sender<Outcome<Frame>>,
    },
    /// Composite all visible windows in z-order over a named output.
    CaptureOutput {
        name: String,
        reply: Sender<Outcome<Frame>>,
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

    /// Captures a window's complete surface tree into an RGBA frame.
    ///
    /// The capture is occlusion-free: the window is composited on its own,
    /// including subsurfaces and child popups, regardless of overlap or
    /// off-screen placement on the virtual output.
    pub fn capture_window(&self, id: WindowId) -> Result<Reply<Frame>> {
        self.request(|reply| Command::CaptureWindow { id, reply })
    }

    /// [`capture_window`](Self::capture_window) encoded as PNG.
    pub fn capture_window_png(&self, id: WindowId) -> Result<Reply<Vec<u8>>> {
        self.capture_window(id)?.try_map(|frame| frame.encode_png())
    }

    /// [`capture_window`](Self::capture_window) encoded as lossless WebP.
    pub fn capture_window_webp(&self, id: WindowId) -> Result<Reply<Vec<u8>>> {
        self.capture_window(id)?
            .try_map(|frame| frame.encode_webp())
    }

    /// Captures a whole virtual output: all visible windows composited in
    /// z-order over an opaque black background.
    ///
    /// `name` is the `wl_output` name (the built-in virtual output is
    /// `"Virtual-1"`); unknown names return [`Error::UnknownOutput`].
    pub fn capture_output(&self, name: &str) -> Result<Reply<Frame>> {
        self.request(|reply| Command::CaptureOutput {
            name: name.to_string(),
            reply,
        })
    }

    /// [`capture_output`](Self::capture_output) encoded as PNG.
    pub fn capture_output_png(&self, name: &str) -> Result<Reply<Vec<u8>>> {
        self.capture_output(name)?
            .try_map(|frame| frame.encode_png())
    }

    /// [`capture_output`](Self::capture_output) encoded as lossless WebP.
    pub fn capture_output_webp(&self, name: &str) -> Result<Reply<Vec<u8>>> {
        self.capture_output(name)?
            .try_map(|frame| frame.encode_webp())
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
