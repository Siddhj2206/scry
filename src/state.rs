//! The smithay compositor state and the calloop event loop driving it.
//!
//! Everything in this module runs on the dedicated compositor thread spawned
//! by [`crate::Compositor::spawn`]. Outside callers never touch it directly;
//! they send [`Command`]s over a calloop channel and receive
//! [`CompositorEvent`]s over subscription channels.

use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, OnceLock, mpsc::Sender as ReplySender},
    time::Instant,
};

use smithay::{
    backend::renderer::utils::{
        CommitCounter, on_commit_buffer_handler, with_renderer_surface_state,
    },
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Space,
        Window, find_popup_root_surface,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{KeyboardHandle, Keycode, Keysym, XkbConfig},
        pointer::{Focus, PointerHandle},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopSignal, Mode, PostAction,
            channel::{Channel, Event as ChannelEvent},
            generic::Generic,
        },
        wayland_protocols::{
            wp::text_input::zv3::server::zwp_text_input_manager_v3::ZwpTextInputManagerV3,
            xdg::shell::server::xdg_toplevel,
        },
        wayland_server::{
            Client, Display, DisplayHandle, ListeningSocket, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{
                wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface,
            },
        },
    },
    utils::{Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, get_parent, get_role, with_states,
        },
        output::{OutputHandler, OutputManagerState},
        seat::WaylandFocus,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XDG_POPUP_ROLE, XDG_TOPLEVEL_ROLE,
            XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        text_input::TextInputManagerState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};
use tracing::{debug, error, info, warn};

use crate::{
    error::Error,
    event::{CompositorEvent, Rect, Seq, SurfaceId, WindowId, WindowInfo},
    handle::{Command, Reply},
    input::TextInputTracker,
};

/// Size of the single virtual output every `Compositor` starts with.
const DEFAULT_OUTPUT_SIZE: (i32, i32) = (1920, 1080);
/// Refresh rate of the default virtual output, in mHz.
const DEFAULT_OUTPUT_REFRESH: i32 = 60_000;

/// Marker stored in every `wl_surface`'s `data_map`, carrying its stable
/// [`SurfaceId`] and the commit counter up to which its damage was reported.
struct SurfaceMarker {
    id: SurfaceId,
    /// Shared with commit handling so it can be read/updated outside the
    /// short-lived `with_states` borrow.
    last_reported_commit: Arc<Mutex<Option<CommitCounter>>>,
}

/// Per-client protocol state, created in [`ClientData`] and looked up from
/// surfaces to resolve the owning client's `SO_PEERCRED` pid.
#[derive(Default)]
pub(crate) struct ClientState {
    /// Required by [`CompositorHandler::client_compositor_state`].
    pub compositor_state: CompositorClientState,
    /// Pid of the connected process, if `SO_PEERCRED` reported one.
    pub pid: OnceLock<u32>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// A mapped toplevel plus the metadata snapshot last reported for it.
pub(crate) struct WindowRecord {
    pub id: WindowId,
    pub window: Window,
    last_info: WindowInfo,
}

/// The compositor state owned by the event-loop thread.
pub(crate) struct ScryState {
    // -- wayland protocol states -------------------------------------------
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<ScryState>,
    /// Retained so the `zwp_text_input_manager_v3` global stays advertised.
    #[allow(dead_code)]
    text_input_manager_state: TextInputManagerState,
    xdg_activation_state: XdgActivationState,
    #[allow(dead_code)] // retained so the xdg_output_manager global lives
    output_manager_state: OutputManagerState,

    // -- input ----------------------------------------------------------------
    /// The single virtual seat every instance runs.
    pub(crate) seat: Seat<ScryState>,
    /// The seat's keyboard (XKB keymap + state + wl_keyboard forwarding).
    pub(crate) keyboard: KeyboardHandle<ScryState>,
    /// The seat's pointer (wl_pointer forwarding + grabs).
    pub(crate) pointer: PointerHandle<ScryState>,
    /// The currently keyboard-focused window, if any.
    pub(crate) focused: Option<WindowId>,
    /// Times each keycode is logically held (explicit + implicit presses).
    pub(crate) key_holds: HashMap<Keycode, u32>,
    /// Modifier keycodes pressed implicitly for a held level>0 keysym.
    pub(crate) implicit_mods: HashMap<Keycode, Vec<Keycode>>,
    /// keysym → resolved (keycode, implicit modifiers) cache.
    pub(crate) keymap_cache: HashMap<Keysym, Option<crate::input::keymap::ResolvedKey>>,
    /// zwp_text_input_v3 instance enable/commit-serial tracking.
    pub(crate) text_input_tracker: TextInputTracker,
    /// Monotonic base for wayland event timestamps.
    pub(crate) clock: Instant,

    // -- desktop model ------------------------------------------------------
    pub(crate) space: Space<Window>,
    pub(crate) popup_manager: PopupManager,
    /// The always-present virtual output. Used by later sub-issues for frame
    /// callbacks, captures and output management; kept alive so its global
    /// stays advertised.
    #[allow(dead_code)]
    default_output: Output,

    // -- bookkeeping ---------------------------------------------------------
    /// Toplevels that were created but have not committed a buffer yet.
    pending_toplevels: Vec<ToplevelSurface>,
    /// Currently mapped toplevel windows.
    pub(crate) windows: Vec<WindowRecord>,
    /// Monotonic event/ack sequence counter.
    seq: Seq,
    next_window_id: u64,
    next_surface_id: u64,
    /// Subscriber channels receiving every [`CompositorEvent`].
    subscribers: Vec<ReplySender<CompositorEvent>>,

    // -- infrastructure ------------------------------------------------------
    display_handle: DisplayHandle,
    loop_signal: LoopSignal,
}

impl ScryState {
    fn new(dh: &DisplayHandle, loop_signal: LoopSignal) -> Self {
        let default_output = Output::new(
            "Virtual-1".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "scry".to_string(),
                model: "virtual".to_string(),
            },
        );
        let mode = OutputMode {
            size: DEFAULT_OUTPUT_SIZE.into(),
            refresh: DEFAULT_OUTPUT_REFRESH,
        };
        default_output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        default_output.set_preferred(mode);
        let _global = default_output.create_global::<Self>(dh);

        let mut space = Space::default();
        space.map_output(&default_output, (0, 0));

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "seat0");
        // An explicit keymap keeps input deterministic regardless of
        // XKB_DEFAULT_* environment variables on the host.
        let keyboard = seat
            .add_keyboard(
                XkbConfig {
                    rules: "evdev",
                    model: "pc105",
                    layout: "us",
                    variant: "",
                    options: None,
                },
                200,
                25,
            )
            .expect("failed to load default xkb keymap");
        let pointer = seat.add_pointer();

        Self {
            compositor_state: CompositorState::new_v6::<Self>(dh),
            xdg_shell_state: XdgShellState::new::<Self>(dh),
            shm_state: ShmState::new::<Self>(dh, vec![]),
            seat_state,
            text_input_manager_state: TextInputManagerState::new::<Self>(dh),
            xdg_activation_state: XdgActivationState::new::<Self>(dh),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat,
            keyboard,
            pointer,
            focused: None,
            key_holds: HashMap::new(),
            implicit_mods: HashMap::new(),
            keymap_cache: HashMap::new(),
            text_input_tracker: TextInputTracker::default(),
            clock: Instant::now(),
            space,
            popup_manager: PopupManager::default(),
            default_output,
            pending_toplevels: Vec::new(),
            windows: Vec::new(),
            seq: 0,
            next_window_id: 1,
            next_surface_id: 1,
            subscribers: Vec::new(),
            display_handle: dh.clone(),
            loop_signal,
        }
    }

    /// Registers a newly accepted wayland client, recording its pid.
    fn accept_client(&mut self, stream: UnixStream) {
        match self
            .display_handle
            .insert_client(stream, Arc::new(ClientState::default()))
        {
            Ok(client) => match client.get_credentials(&self.display_handle) {
                Ok(creds) => {
                    if let Some(data) = client.get_data::<ClientState>() {
                        let pid = u32::try_from(creds.pid).ok();
                        if let Some(pid) = pid {
                            let _ = data.pid.set(pid);
                        }
                        debug!(?pid, "client connected");
                    }
                }
                Err(err) => warn!("could not read client credentials: {err}"),
            },
            Err(err) => warn!("failed to accept wayland client: {err}"),
        }
    }

    /// Assigns the next sequence number to `event` and broadcasts it.
    pub(crate) fn emit(&mut self, event: impl FnOnce(Seq) -> CompositorEvent) {
        self.seq += 1;
        let event = event(self.seq);
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }

    /// Recomputes the externally visible metadata of a mapped window.
    fn window_info(&self, id: WindowId, window: &Window) -> WindowInfo {
        let toplevel = window.toplevel();
        let (title, app_id, activated) = toplevel
            .map(|toplevel| {
                let (title, app_id) = with_states(toplevel.wl_surface(), |states| {
                    let attributes = states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .expect("toplevel without role data")
                        .lock()
                        .unwrap();
                    (attributes.title.clone(), attributes.app_id.clone())
                });
                let activated = toplevel
                    .current_state()
                    .states
                    .contains(xdg_toplevel::State::Activated);
                (title, app_id, activated)
            })
            .unwrap_or((None, None, false));

        let pid = toplevel
            .and_then(|t| t.wl_surface().client())
            .and_then(|client| {
                client
                    .get_data::<ClientState>()
                    .and_then(|d| d.pid.get().copied())
            });

        let geometry = self
            .space
            .element_geometry(window)
            .map(Rect::from)
            .unwrap_or_default();

        WindowInfo {
            id,
            app_id,
            title,
            geometry,
            pid,
            activated,
        }
    }

    /// Resolves the mapped window owning `surface`, following subsurface
    /// and popup parents up to their root surface.
    pub(crate) fn window_id_for_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        let mut root = surface.clone();
        loop {
            if let Some(parent) = get_parent(&root) {
                root = parent;
                continue;
            }
            if get_role(&root) == Some(XDG_POPUP_ROLE) {
                let Some(popup) = self.popup_manager.find_popup(&root) else {
                    break;
                };
                let Ok(parent) = find_popup_root_surface(&popup) else {
                    break;
                };
                root = parent;
                continue;
            }
            break;
        }
        self.windows
            .iter()
            .find(|record| {
                record
                    .window
                    .wl_surface()
                    .map(|s| *s == root)
                    .unwrap_or(false)
            })
            .map(|record| record.id)
    }

    /// `wl_surface.commit` was applied to `surface`.
    fn on_commit(&mut self, surface: &WlSurface) {
        // Whether this commit attached a buffer; read before smithay consumes it.
        let attached_buffer = with_states(surface, |states| {
            matches!(
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer,
                Some(BufferAssignment::NewBuffer(_))
            )
        });

        on_commit_buffer_handler::<Self>(surface);
        self.popup_manager.commit(surface);
        for window in self.space.elements() {
            window.on_commit();
        }

        let is_toplevel = get_role(surface) == Some(XDG_TOPLEVEL_ROLE);
        if is_toplevel {
            self.on_toplevel_commit(surface);
        }

        // Damage committed with this commit, normalized to buffer coordinates.
        let (damage, surface_id) = self.normalized_damage(surface, attached_buffer);
        if let Some(surface_id) = surface_id {
            let window_id = self.window_id_for_surface(surface);
            self.emit(|seq| CompositorEvent::SurfaceCommitted {
                seq,
                surface_id,
                window_id,
                damage,
            });
        }
    }

    /// Reads this commit's damage in buffer coordinates, falling back to the
    /// whole buffer when a newly attached buffer declares no damage.
    fn normalized_damage(
        &self,
        surface: &WlSurface,
        attached_buffer: bool,
    ) -> (Vec<Rect>, Option<SurfaceId>) {
        let Some((surface_id, last_commit)) = with_states(surface, |states| {
            states
                .data_map
                .get::<SurfaceMarker>()
                .map(|marker| (marker.id, marker.last_reported_commit.clone()))
        }) else {
            return (Vec::new(), None);
        };

        let damage = with_renderer_surface_state(surface, |rss| {
            let previous = last_commit.lock().unwrap().replace(rss.current_commit());
            let set = rss.damage_since(previous);
            let mut damage: Vec<Rect> = set.iter().copied().map(Rect::from).collect();
            if damage.is_empty() && attached_buffer {
                // A newly attached buffer that declares no damage is fully
                // damaged; `damage_since(None)` yields the whole buffer.
                damage = rss
                    .damage_since(None)
                    .iter()
                    .copied()
                    .map(Rect::from)
                    .collect();
            }
            damage
        })
        .unwrap_or_default();
        (damage, Some(surface_id))
    }

    /// Maps / updates / unmaps a toplevel window following a commit.
    fn on_toplevel_commit(&mut self, surface: &WlSurface) {
        let has_buffer =
            with_renderer_surface_state(surface, |rss| rss.buffer().is_some()).unwrap_or(false);

        if let Some(pos) = self.windows.iter().position(|record| {
            record
                .window
                .wl_surface()
                .map(|s| *s == *surface)
                .unwrap_or(false)
        }) {
            if !has_buffer {
                // Per the xdg_shell spec, attaching a null buffer unmaps.
                let record = self.windows.remove(pos);
                self.space.unmap_elem(&record.window);
                self.emit(|seq| CompositorEvent::WindowClosed {
                    seq,
                    window_id: record.id,
                });
                if self.focused == Some(record.id) {
                    self.focus_fallback();
                }
                return;
            }
            let info = self.window_info(self.windows[pos].id, &self.windows[pos].window);
            if info != self.windows[pos].last_info {
                self.windows[pos].last_info = info.clone();
                self.emit(|seq| CompositorEvent::WindowUpdated { seq, window: info });
            }
            return;
        }

        if !has_buffer {
            return;
        }

        // First commit with a buffer: map the toplevel as a window.
        let Some(pos) = self
            .pending_toplevels
            .iter()
            .position(|t| *t.wl_surface() == *surface)
        else {
            return;
        };
        let toplevel = self.pending_toplevels.remove(pos);
        let window = Window::new_wayland_window(toplevel);
        self.space.map_element(window.clone(), (0, 0), true);
        window.on_commit();

        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        let info = self.window_info(id, &window);
        self.windows.push(WindowRecord {
            id,
            window: window.clone(),
            last_info: info.clone(),
        });
        self.emit(|seq| CompositorEvent::WindowMapped { seq, window: info });
        // Newly mapped toplevels take focus, matching stacking-WM behavior.
        self.focus_window(&window);
    }

    /// A toplevel is going away, whether it was mapped or still pending.
    fn on_toplevel_gone(&mut self, surface: &WlSurface) {
        self.pending_toplevels
            .retain(|t| *t.wl_surface() != *surface);
        if let Some(pos) = self.windows.iter().position(|record| {
            record
                .window
                .wl_surface()
                .map(|s| *s == *surface)
                .unwrap_or(false)
        }) {
            let record = self.windows.remove(pos);
            self.space.unmap_elem(&record.window);
            self.emit(|seq| CompositorEvent::WindowClosed {
                seq,
                window_id: record.id,
            });
            if self.focused == Some(record.id) {
                self.focus_fallback();
            }
        }
    }

    /// A surface owned by a toplevel role disappeared (defensive cleanup for
    /// client disconnects; [`XdgShellHandler::toplevel_destroyed`] covers the
    /// orderly path).
    fn on_surface_destroyed(&mut self, surface: &WlSurface) {
        if get_role(surface) == Some(XDG_TOPLEVEL_ROLE) {
            self.on_toplevel_gone(surface);
        }
    }

    /// Executes one [`Command`] coming from a [`crate::CompositorHandle`].
    fn handle_command(&mut self, command: Command) {
        match command {
            Command::PointerMotion {
                target,
                x,
                y,
                reply,
            } => {
                let outcome = self.cmd_pointer_motion(target, x, y);
                self.seq += 1;
                let _ = reply.send(outcome.map(|value| Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::PointerButton {
                button,
                pressed,
                reply,
            } => {
                let outcome = self.cmd_pointer_button(button, pressed);
                self.seq += 1;
                let _ = reply.send(outcome.map(|value| Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::PointerScroll { x, y, reply } => {
                let outcome = self.cmd_pointer_scroll(x, y);
                self.seq += 1;
                let _ = reply.send(outcome.map(|value| Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::KeyEvent {
                name,
                pressed,
                reply,
            } => {
                let outcome = self.cmd_key_event(&name, pressed);
                self.seq += 1;
                let _ = reply.send(outcome.map(|value| Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::KeyCombo {
                name,
                modifiers,
                reply,
            } => {
                let outcome = self.cmd_key_combo(&name, &modifiers);
                self.seq += 1;
                let _ = reply.send(outcome.map(|value| Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::TypeText { text, reply } => {
                let outcome = self.cmd_type_text(&text);
                self.seq += 1;
                let _ = reply.send(outcome.map(|value| Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::FocusWindow { id, reply } => {
                let outcome = self.cmd_focus_window(id);
                self.seq += 1;
                let _ = reply.send(outcome.map(|value| Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::Shutdown { reply } => {
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value: (),
                }));
                self.loop_signal.stop();
            }
            Command::Subscribe { sender } => {
                self.subscribers.push(sender);
            }
            Command::Windows { reply } => {
                let windows = self
                    .windows
                    .iter()
                    .map(|record| self.window_info(record.id, &record.window))
                    .collect();
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value: windows,
                }));
            }
            Command::ResizeWindow { id, size, reply } => {
                let found = self.windows.iter().find(|record| record.id == id);
                match found.and_then(|record| record.window.toplevel()) {
                    Some(toplevel) => {
                        toplevel.with_pending_state(|state| {
                            state.size = Some(Size::from(size));
                        });
                        toplevel.send_configure();
                        self.seq += 1;
                        let _ = reply.send(Ok(Reply {
                            seq: self.seq,
                            value: (),
                        }));
                    }
                    None => {
                        let _ = reply.send(Err(Error::UnknownWindow(id)));
                    }
                }
            }
            Command::CloseWindow { id, reply } => {
                let found = self.windows.iter().find(|record| record.id == id);
                match found.and_then(|record| record.window.toplevel()) {
                    Some(toplevel) => {
                        toplevel.send_close();
                        self.seq += 1;
                        let _ = reply.send(Ok(Reply {
                            seq: self.seq,
                            value: (),
                        }));
                    }
                    None => {
                        let _ = reply.send(Err(Error::UnknownWindow(id)));
                    }
                }
            }
        }
        // Commands may have produced protocol traffic (configures, closes).
        if let Err(err) = self.display_handle.flush_clients() {
            debug!("failed to flush clients after command: {err}");
        }
    }

    /// Bookkeeping run after every event-loop dispatch.
    fn post_tick(&mut self) {
        self.space.refresh();
        self.popup_manager.cleanup();
        if let Err(err) = self.display_handle.flush_clients() {
            debug!("failed to flush clients: {err}");
        }
    }
}

// -- smithay handler implementations ----------------------------------------

impl CompositorHandler for ScryState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client without ClientState")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        let id = SurfaceId(self.next_surface_id);
        self.next_surface_id += 1;
        with_states(surface, |states| {
            states.data_map.insert_if_missing(|| SurfaceMarker {
                id,
                last_reported_commit: Arc::new(Mutex::new(None)),
            });
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.on_commit(surface);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.on_surface_destroyed(surface);
    }
}

impl BufferHandler for ScryState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for ScryState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for ScryState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // The initial configure carries no size hint: the client chooses.
        surface.send_configure();
        self.pending_toplevels.push(surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = surface.send_configure();
        if let Err(err) = self.popup_manager.track_popup(PopupKind::Xdg(surface)) {
            warn!("failed to track popup: {err}");
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            warn!("popup grab for unknown seat");
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            warn!("popup grab without a live parent");
            return;
        };
        let Ok(mut grab) = self.popup_manager.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
        let _ = surface.send_configure();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.on_toplevel_gone(surface.wl_surface());
    }
}

impl OutputHandler for ScryState {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

impl SeatHandler for ScryState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let id = focused.and_then(|surface| self.window_id_for_surface(surface));
        if self.focused != id {
            self.focused = id;
            self.emit(|seq| CompositorEvent::WindowFocused { seq, window_id: id });
        }
    }
}

impl XdgActivationHandler for ScryState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, _data: XdgActivationTokenData) -> bool {
        true
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if let Some(id) = self.window_id_for_surface(&surface) {
            if let Err(err) = self.focus_window_id(id) {
                debug!("activation for unmapped window: {err}");
            }
        } else {
            debug!("activation request for an unknown surface");
        }
        self.xdg_activation_state.remove_token(&token);
    }
}

smithay::delegate_compositor!(ScryState);
smithay::delegate_shm!(ScryState);
smithay::delegate_xdg_shell!(ScryState);
smithay::delegate_output!(ScryState);
smithay::delegate_seat!(ScryState);
smithay::delegate_xdg_activation!(ScryState);

// zwp_text_input_v3: the manager global + its dispatch are delegated to
// smithay's state, while ZwpTextInputV3 requests are dispatched to ScryState's
// own impl (src/input/text_input.rs), which additionally tracks enable state
// and commit serials for IME-less text injection.
smithay::reexports::wayland_server::delegate_global_dispatch!(ScryState: [
    ZwpTextInputManagerV3: ()
] => TextInputManagerState);
smithay::reexports::wayland_server::delegate_dispatch!(ScryState: [
    ZwpTextInputManagerV3: ()
] => TextInputManagerState);

// -- event loop entry point ---------------------------------------------------

/// Drives the compositor on the current thread until shutdown.
///
/// Reports `Ok(())` over `init` once all event sources are registered, or the
/// failure reason. Returns when the loop stops; the socket and lockfile are
/// removed when the [`ListeningSocket`] is dropped.
pub(crate) fn run(
    socket: ListeningSocket,
    command_rx: Channel<Command>,
    init: ReplySender<std::result::Result<(), String>>,
) {
    let mut event_loop: EventLoop<ScryState> = match EventLoop::try_new().map_err(|e| e.to_string())
    {
        Ok(event_loop) => event_loop,
        Err(err) => {
            let _ = init.send(Err(err));
            return;
        }
    };
    let loop_handle = event_loop.handle();

    let display: Display<ScryState> = match Display::new().map_err(|e| e.to_string()) {
        Ok(display) => display,
        Err(err) => {
            let _ = init.send(Err(err));
            return;
        }
    };
    let display_handle = display.handle();
    let mut state = ScryState::new(&display_handle, event_loop.get_signal());

    let setup = (|| -> std::result::Result<(), String> {
        // Commands arriving from CompositorHandle instances.
        loop_handle
            .insert_source(command_rx, |event, _, state| match event {
                ChannelEvent::Msg(command) => {
                    state.handle_command(command);
                }
                ChannelEvent::Closed => {
                    state.loop_signal.stop();
                }
            })
            .map_err(|e| e.to_string())?;

        // New wayland clients connecting to our listening socket.
        loop_handle
            .insert_source(
                Generic::new(socket, Interest::READ, Mode::Level),
                |_, socket, state| {
                    loop {
                        match socket.accept() {
                            Ok(Some(stream)) => state.accept_client(stream),
                            Ok(None) => break,
                            Err(err) => {
                                warn!("error accepting wayland client: {err}");
                                break;
                            }
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| e.to_string())?;

        // The wayland display itself: dispatch client requests when readable.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // SAFETY: the display is only used, never dropped, while
                    // it is registered in the event loop.
                    let display = unsafe { display.get_mut() };
                    if let Err(err) = display.dispatch_clients(state) {
                        error!("error dispatching wayland clients: {err}");
                    }
                    if let Err(err) = display.flush_clients() {
                        debug!("failed to flush clients: {err}");
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    })();

    if let Err(err) = setup {
        let _ = init.send(Err(err));
        return;
    }
    let _ = init.send(Ok(()));
    info!("scry compositor event loop starting");

    if let Err(err) = event_loop.run(None, &mut state, |state| state.post_tick()) {
        error!("compositor event loop failed: {err}");
    }
    info!("scry compositor event loop stopped");
}
