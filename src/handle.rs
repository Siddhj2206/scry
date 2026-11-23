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
    event::{CompositorEvent, DamageEntry, PopupInfo, Seq, WindowId, WindowInfo},
    frame::Frame,
    frame_clock::FrameClockMode,
    input::{PointerButton, PointerTarget},
    output::{OutputInfo, OutputSpec},
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
    /// Snapshot all currently mapped popups.
    Popups {
        reply: Sender<Outcome<Vec<PopupInfo>>>,
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
    /// Move a toplevel to exact logical coordinates and optionally request
    /// a resize through the configure/ack lifecycle.
    PlaceWindow {
        id: WindowId,
        position: (i32, i32),
        size: Option<(i32, i32)>,
        reply: Sender<Outcome<()>>,
    },
    /// Snapshot the retained damage log: every recorded commit's damage
    /// with `seq` strictly greater than `since`.
    Damage {
        since: Seq,
        reply: Sender<Outcome<Vec<DamageEntry>>>,
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
    /// Snapshot all registered virtual outputs.
    Outputs {
        reply: Sender<Outcome<Vec<OutputInfo>>>,
    },
    /// Add or reconfigure a virtual output.
    UpdateOutput {
        spec: OutputSpec,
        reply: Sender<Outcome<()>>,
    },
    /// Remove a virtual output by `wl_output` name.
    RemoveOutput {
        name: String,
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
    /// Reserve a fresh sequence point on the compositor's timeline.
    ///
    /// Daemon-side constructs that occupy the shared seq timeline (named
    /// marks) allocate through this command so their seqs interleave
    /// correctly with compositor events and command replies.
    BumpSeq { reply: Sender<Outcome<()>> },
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

    /// Lists all currently mapped `xdg_popup` surfaces, with the window
    /// each is anchored to and its positioner geometry.
    pub fn popups(&self) -> Result<Reply<Vec<PopupInfo>>> {
        self.request(|reply| Command::Popups { reply })
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

    /// Moves a window to exact logical coordinates and, when `size` is
    /// given, requests that size through the `xdg_toplevel` configure/ack
    /// lifecycle.
    ///
    /// The position applies immediately in compositor space (a
    /// [`CompositorEvent::WindowUpdated`] reports it right away); the
    /// requested size lands once the client acks the configure and commits
    /// at the new size, emitting a second `WindowUpdated`. Moving preserves
    /// the window's stacking position. `size: None` is a pure move and sends
    /// no configure.
    pub fn place_window(
        &self,
        id: WindowId,
        position: (i32, i32),
        size: Option<(i32, i32)>,
    ) -> Result<Reply<()>> {
        self.request(|reply| Command::PlaceWindow {
            id,
            position,
            size,
            reply,
        })
    }

    /// Damage records for surface commits with `seq` strictly greater
    /// than `since` (`0` returns the whole retained log).
    ///
    /// Entries carry normalized buffer-space rectangles and the window the
    /// committing surface belonged to — exactly what `get_damage` exposes
    /// over the control API. The log is bounded; entries older than its
    /// retention window are dropped silently.
    pub fn damage_since(&self, since: Seq) -> Result<Reply<Vec<DamageEntry>>> {
        self.request(|reply| Command::Damage { since, reply })
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

    /// Presses an evdev [`PointerButton`] (e.g. `PointerButton::Left`) at the
    /// pointer's current location. Pressing on an unfocused toplevel raises
    /// and focuses it (click-to-focus).
    pub fn press_pointer_button(&self, button: PointerButton) -> Result<Reply<()>> {
        let button = button.code()?;
        self.request(|reply| Command::PointerButton {
            button,
            pressed: true,
            reply,
        })
    }

    /// Releases a previously pressed [`PointerButton`].
    pub fn release_pointer_button(&self, button: PointerButton) -> Result<Reply<()>> {
        let button = button.code()?;
        self.request(|reply| Command::PointerButton {
            button,
            pressed: false,
            reply,
        })
    }

    /// Emits a complete pointer scroll frame at the current location: both
    /// axis deltas (15.0 = one wheel detent, negatives scroll up/left) plus
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

    /// Lists all registered virtual outputs with their advertised mode,
    /// scale and logical geometry.
    pub fn outputs(&self) -> Result<Reply<Vec<OutputInfo>>> {
        self.request(|reply| Command::Outputs { reply })
    }

    /// Adds a virtual output, or reconfigures the existing one named
    /// `spec.name`.
    ///
    /// New outputs advertise a `wl_output` global (mode, scale and
    /// `zxdg_output_v1` logical geometry) and are mapped into compositor
    /// space at `spec.position`. Updates push the changed attributes to
    /// every bound `wl_output` and re-publish scale hints to surfaces
    /// overlapping the output. Invalid specs are rejected with
    /// [`Error::InvalidOutputSpec`].
    pub fn update_output(&self, spec: OutputSpec) -> Result<Reply<()>> {
        self.request(|reply| Command::UpdateOutput { spec, reply })
    }

    /// Removes the virtual output named `name`, withdrawing its
    /// `wl_output` global (`wl_registry.global_remove`) and unmapping it
    /// from compositor space.
    ///
    /// Unknown names return [`Error::UnknownOutput`]; removing the last
    /// remaining output is rejected with [`Error::InvalidOutputSpec`].
    pub fn remove_output(&self, name: &str) -> Result<Reply<()>> {
        self.request(|reply| Command::RemoveOutput {
            name: name.to_string(),
            reply,
        })
    }

    /// Captures a whole virtual output: all visible windows composited in
    /// z-order over an opaque black background at the output's physical
    /// (mode) pixel resolution.
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

    /// Reserves and returns the next sequence number on the compositor's
    /// timeline, in `reply.seq`.
    ///
    /// The returned seq is a real, spent timeline point: later events and
    /// command replies are strictly greater, so a seq allocated here is a
    /// stable boundary for `get_changes_since`/`wait_for_commit` queries.
    pub fn bump_seq(&self) -> Result<Reply<()>> {
        self.request(|reply| Command::BumpSeq { reply })
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
