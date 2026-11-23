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
    input::{PointerButton, PointerTarget},
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

    // -- input ---------------------------------------------------------------
    /// Move the pointer to a position in the target's coordinate space,
    /// sending enter/motion/leave as needed.
    PointerMotion {
        target: PointerTarget,
        x: f64,
        y: f64,
        reply: Sender<Outcome<()>>,
    },
    /// Press or release an evdev pointer button at the current location.
    PointerButton {
        /// evdev button code (e.g. `BTN_LEFT` = 0x110).
        button: u32,
        pressed: bool,
        reply: Sender<Outcome<()>>,
    },
    /// Emit one scroll axis frame (wheel source) at the current location.
    PointerScroll {
        /// Horizontal delta; negative scrolls left. 15.0 = one detent.
        x: f64,
        /// Vertical delta; negative scrolls up. 15.0 = one detent.
        y: f64,
        reply: Sender<Outcome<()>>,
    },
    /// Press or release a physical key by name.
    KeyEvent {
        name: String,
        pressed: bool,
        reply: Sender<Outcome<()>>,
    },
    /// Hold `modifiers`, tap `name`, release the modifiers.
    KeyCombo {
        name: String,
        modifiers: Vec<String>,
        reply: Sender<Outcome<()>>,
    },
    /// Inject text, preferring `zwp_text_input_v3` with an XKB fallback.
    TypeText {
        text: String,
        reply: Sender<Outcome<()>>,
    },
    /// Raise and keyboard-focus a window.
    FocusWindow {
        id: WindowId,
        reply: Sender<Outcome<()>>,
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

    // -- synthetic input -----------------------------------------------------

    /// Moves the virtual pointer, delivering `wl_pointer` enter/motion/leave
    /// to whatever surface ends up underneath.
    ///
    /// - [`PointerTarget::Output`]: global logical output coordinates; the
    ///   topmost surface under the point is entered.
    /// - [`PointerTarget::Window`]: coordinates relative to that window's
    ///   logical top-left corner — the named window is targeted even when
    ///   covered by others; points outside it fail with
    ///   [`Error::OutsideWindow`].
    pub fn move_pointer(&self, target: PointerTarget, x: f64, y: f64) -> Result<Reply<()>> {
        self.request(|reply| Command::PointerMotion {
            target,
            x,
            y,
            reply,
        })
    }

    /// Presses `button` at the current pointer position.
    ///
    /// A press is also the click-to-focus trigger: the toplevel owning the
    /// pressed surface is raised and focused (unless a popup grab reroutes
    /// the press, in which case normal grab semantics apply).
    pub fn press_pointer_button(&self, button: PointerButton) -> Result<Reply<()>> {
        let button = button.code()?;
        self.request(|reply| Command::PointerButton {
            button,
            pressed: true,
            reply,
        })
    }

    /// Releases `button` at the current pointer position.
    pub fn release_pointer_button(&self, button: PointerButton) -> Result<Reply<()>> {
        let button = button.code()?;
        self.request(|reply| Command::PointerButton {
            button,
            pressed: false,
            reply,
        })
    }

    /// Emits a single wheel-axis scroll frame at the current pointer
    /// position.
    ///
    /// `x`/`y` are continuous deltas in compositor axis units; `±15.0`
    /// corresponds to one wheel detent (also reported as `±120` through
    /// `wl_pointer.axis_v120` for clients that want discrete steps).
    pub fn scroll_pointer(&self, x: f64, y: f64) -> Result<Reply<()>> {
        self.request(|reply| Command::PointerScroll { x, y, reply })
    }

    /// Presses the physical key named `key`.
    ///
    /// Names are XKB keysym names resolved case-insensitively (`a`, `F5`,
    /// `Return`, `space`, `Left`, …) plus convenience aliases (`ctrl`, `alt`,
    /// `super`, `enter`, `esc`, `pgup`, …). Keysyms living on shifted keymap
    /// levels hold Shift (or AltGr) implicitly until the matching release.
    /// Unknown names fail with [`Error::UnknownKey`].
    pub fn press_key(&self, key: &str) -> Result<Reply<()>> {
        self.request(|reply| Command::KeyEvent {
            name: key.to_string(),
            pressed: true,
            reply,
        })
    }

    /// Releases the physical key named `key` (see [`Self::press_key`]).
    pub fn release_key(&self, key: &str) -> Result<Reply<()>> {
        self.request(|reply| Command::KeyEvent {
            name: key.to_string(),
            pressed: false,
            reply,
        })
    }

    /// Taps `key` while holding `modifiers` — e.g. `key("c", &["ctrl"])`
    /// sends a Ctrl+C. Modifier names: `ctrl`, `shift`, `alt`, `super`
    /// (aliases `logo`/`meta`/`win`/`cmd`), `altgr`, `capslock`, `numlock`.
    /// Unrecognised modifier names fail with [`Error::UnknownModifier`].
    pub fn key(&self, key: &str, modifiers: &[&str]) -> Result<Reply<()>> {
        self.request(|reply| Command::KeyCombo {
            name: key.to_string(),
            modifiers: modifiers.iter().map(|m| m.to_string()).collect(),
            reply,
        })
    }

    /// Injects text into the focused window.
    ///
    /// When the focused client owns an enabled `zwp_text_input_v3` instance
    /// the whole string is committed through it (arbitrary UTF-8, including
    /// accents and emoji). Otherwise each character is typed as physical XKB
    /// key events; characters no keymap keysym produces fail with
    /// [`Error::UnmappableCharacter`] rather than being dropped silently.
    /// Fails with [`Error::NoFocusedSurface`] when nothing is focused.
    pub fn type_text(&self, text: &str) -> Result<Reply<()>> {
        self.request(|reply| Command::TypeText {
            text: text.to_string(),
            reply,
        })
    }

    /// Raises `id` to the top of the stacking order and moves keyboard and
    /// `zwp_text_input_v3` focus to it, toggling `Activated` on the affected
    /// toplevels.
    pub fn focus_window(&self, id: WindowId) -> Result<Reply<()>> {
        self.request(|reply| Command::FocusWindow { id, reply })
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
