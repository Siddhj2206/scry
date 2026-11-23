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
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        buffer_dimensions,
        utils::{CommitCounter, on_commit_buffer_handler, with_renderer_surface_state},
    },
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Space,
        Window, find_popup_root_surface, utils::send_frames_surface_tree,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{KeyboardHandle, Keycode, Keysym, XkbConfig},
        pointer::{Focus, PointerHandle},
    },
    output::Output,
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
            channel::{Channel, Event as ChannelEvent},
            generic::Generic,
            timer::{TimeoutAction, Timer},
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
    utils::{Logical, Point, Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, TraversalAction, get_parent, get_role, with_states,
            with_surface_tree_downward,
        },
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        output::{OutputHandler, OutputManagerState},
        seat::WaylandFocus,
        selection::{
            data_device::{DataDeviceState, set_data_device_focus},
            primary_selection::{PrimarySelectionState, set_primary_focus},
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XDG_POPUP_ROLE, XDG_TOPLEVEL_ROLE,
            XdgPopupSurfaceData, XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
            decoration::{XdgDecorationHandler, XdgDecorationState},
        },
        shm::{ShmHandler, ShmState},
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};
use tracing::{debug, error, info, warn};

use crate::{
    clipboard::SelectionOffers,
    error::Error,
    event::{CompositorEvent, DamageEntry, PopupInfo, Rect, Seq, SurfaceId, WindowId, WindowInfo},
    frame_clock::{FRAME_INTERVAL, FrameClock},
    handle::{Command, Outcome, Reply},
    input::TextInputTracker,
    output::{VirtualOutputs, apply_scale_hints, default_output_spec},
    render,
};

use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

/// Maximum number of committed-damage records retained for
/// [`Command::Damage`] queries.
const DAMAGE_LOG_CAPACITY: usize = 512;

/// Diagonal step between successive newly mapped windows' initial slots.
const CASCADE_STEP: i32 = 32;

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

/// A currently mapped `xdg_popup`, keyed by its surface.
struct PopupRecord {
    surface_id: SurfaceId,
    surface: WlSurface,
}

/// The compositor state owned by the event-loop thread.
pub(crate) struct ScryState {
    // -- wayland protocol states -------------------------------------------
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<ScryState>,
    pub(crate) data_device_state: DataDeviceState,
    pub(crate) primary_selection_state: PrimarySelectionState,
    /// Retained so the `zwp_text_input_manager_v3` global stays advertised.
    #[allow(dead_code)]
    text_input_manager_state: TextInputManagerState,
    xdg_activation_state: XdgActivationState,
    #[allow(dead_code)] // retained so the xdg_output_manager global lives
    output_manager_state: OutputManagerState,
    #[allow(dead_code)] // retained so the wp_viewporter global lives
    viewporter_state: ViewporterState,
    #[allow(dead_code)] // state lives inside surface data_maps
    fractional_scale_state: FractionalScaleManagerState,
    #[allow(dead_code)] // retained so the zxdg_decoration_manager global lives
    xdg_decoration_state: XdgDecorationState,

    // -- input ----------------------------------------------------------------
    /// The single virtual seat every instance runs. Keyboard focus tracks the
    /// active toplevel and drives `wl_data_device` / primary selection focus.
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
    /// The registered virtual outputs (`Virtual-1` and any added via
    /// `update_output`).
    outputs: VirtualOutputs,

    // -- bookkeeping ---------------------------------------------------------
    /// Toplevels that were created but have not committed a buffer yet.
    pending_toplevels: Vec<ToplevelSurface>,
    /// Currently mapped toplevel windows.
    pub(crate) windows: Vec<WindowRecord>,
    /// Currently mapped popups (any window's).
    mapped_popups: Vec<PopupRecord>,
    /// Bounded log of committed surface damage, newest last.
    damage_log: std::collections::VecDeque<DamageEntry>,
    /// Monotonic event/ack sequence counter.
    pub(crate) seq: Seq,
    next_window_id: u64,
    next_surface_id: u64,
    /// What each seat selection currently offers (see [`crate::clipboard`]).
    pub(crate) selection_offers: SelectionOffers,
    /// Subscriber channels receiving every [`CompositorEvent`].
    subscribers: Vec<ReplySender<CompositorEvent>>,
    /// Virtual frame clock driving `wl_surface.frame` callbacks.
    frame_clock: FrameClock,

    // -- infrastructure ------------------------------------------------------
    pub(crate) display_handle: DisplayHandle,
    /// Used to register asynchronous event sources mid-flight (selection
    /// reads streaming back from clients).
    pub(crate) loop_handle: LoopHandle<'static, ScryState>,
    loop_signal: LoopSignal,
}

impl ScryState {
    fn new(
        dh: &DisplayHandle,
        loop_handle: LoopHandle<'static, ScryState>,
        loop_signal: LoopSignal,
    ) -> Self {
        let mut space = Space::default();
        let mut outputs = VirtualOutputs::new();
        outputs
            .update(dh, &mut space, default_output_spec())
            .expect("default output spec must be valid");

        // The seat needs a keyboard for clients to own selections: smithay
        // only honors `set_selection` from the client holding keyboard focus.
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
            data_device_state: DataDeviceState::new::<Self>(dh),
            primary_selection_state: PrimarySelectionState::new::<Self>(dh),
            text_input_manager_state: TextInputManagerState::new::<Self>(dh),
            xdg_activation_state: XdgActivationState::new::<Self>(dh),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            viewporter_state: ViewporterState::new::<Self>(dh),
            fractional_scale_state: FractionalScaleManagerState::new::<Self>(dh),
            xdg_decoration_state: XdgDecorationState::new::<Self>(dh),
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
            outputs,
            pending_toplevels: Vec::new(),
            windows: Vec::new(),
            mapped_popups: Vec::new(),
            damage_log: std::collections::VecDeque::new(),
            seq: 0,
            next_window_id: 1,
            next_surface_id: 1,
            selection_offers: SelectionOffers::default(),
            subscribers: Vec::new(),
            frame_clock: FrameClock::new(),
            display_handle: dh.clone(),
            loop_handle,
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

    /// Answers a mutating command: a successful outcome is stamped with a
    /// fresh sequence number; errors travel back unmodified.
    fn send_mutating_reply<T>(
        &mut self,
        reply: &ReplySender<Outcome<T>>,
        outcome: crate::Result<T>,
    ) {
        let _ = reply.send(outcome.map(|value| {
            self.seq += 1;
            Reply {
                seq: self.seq,
                value,
            }
        }));
    }

    /// Recomputes the externally visible metadata of a mapped window.
    fn window_info(&self, id: WindowId, window: &Window) -> WindowInfo {
        let toplevel = window.toplevel();
        let (title, app_id, activated, maximized) = toplevel
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
                let states = toplevel.current_state().states;
                (
                    title,
                    app_id,
                    states.contains(xdg_toplevel::State::Activated),
                    states.contains(xdg_toplevel::State::Maximized),
                )
            })
            .unwrap_or((None, None, false, false));

        // Every `wl_surface` is stamped with a `SurfaceId` at creation.
        let surface_id = toplevel
            .and_then(|t| self.surface_id_for(t.wl_surface()))
            .expect("toplevel surface without a SurfaceId");

        let buffer_size = toplevel
            .and_then(|t| {
                with_renderer_surface_state(t.wl_surface(), |rss| {
                    rss.buffer().and_then(|buffer| buffer_dimensions(buffer))
                })
                .flatten()
            })
            .map(|size| (size.w, size.h));

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
            surface_id,
            app_id,
            title,
            geometry,
            buffer_size,
            pid,
            activated,
            maximized,
            scale: self.outputs.preferred_scale_for(&self.space, window),
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

        match get_role(surface) {
            Some(XDG_TOPLEVEL_ROLE) => self.on_toplevel_commit(surface),
            Some(XDG_POPUP_ROLE) => self.on_popup_commit(surface),
            _ => {}
        }

        // Damage committed with this commit, normalized to buffer coordinates.
        let (damage, surface_id) = self.normalized_damage(surface, attached_buffer);
        if let Some(surface_id) = surface_id {
            let window_id = self.window_id_for_surface(surface);
            if !damage.is_empty() {
                // `emit` is about to assign `self.seq + 1` to this event.
                self.damage_log.push_back(DamageEntry {
                    seq: self.seq + 1,
                    surface_id,
                    window_id,
                    damage: damage.clone(),
                });
                while self.damage_log.len() > DAMAGE_LOG_CAPACITY {
                    self.damage_log.pop_front();
                }
            }
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

    /// The deterministic position a newly mapped window starts at: a
    /// diagonal cascade stepping [`CASCADE_STEP`] per already-mapped window,
    /// wrapped so the window stays fully inside the primary output whenever
    /// it fits. Determinism across runs is the point — the same map order
    /// always produces the same initial layout before any `place_window`.
    fn initial_location(&self, window: &Window) -> Point<i32, Logical> {
        let index = self.windows.len() as i32;
        let Some(primary) = self
            .outputs
            .primary()
            .and_then(|output| self.space.output_geometry(output))
        else {
            let offset = index * CASCADE_STEP;
            return (offset, offset).into();
        };
        let size = window.geometry().size;
        let span_x = (primary.size.w - size.w).max(0) + 1;
        let span_y = (primary.size.h - size.h).max(0) + 1;
        let step = index * CASCADE_STEP;
        (primary.loc.x + step % span_x, primary.loc.y + step % span_y).into()
    }

    /// Moves a mapped window to exact logical coordinates and, when `size`
    /// is given, requests it through the `xdg_toplevel` configure/ack
    /// lifecycle.
    ///
    /// `Space::map_element` re-inserts an element at the top of the stack,
    /// so the windows that were above the target are re-mapped afterwards
    /// to preserve the existing z-order — placement moves, it does not
    /// raise. The new position is emitted as `WindowUpdated` immediately:
    /// a client that never recommits would otherwise leave stale geometry
    /// on the event stream.
    fn cmd_place_window(
        &mut self,
        id: WindowId,
        position: (i32, i32),
        size: Option<(i32, i32)>,
    ) -> crate::Result<()> {
        let Some(index) = self.windows.iter().position(|r| r.id == id) else {
            return Err(Error::UnknownWindow(id));
        };
        let window = self.windows[index].window.clone();

        let above: Vec<Window> = self
            .space
            .elements()
            .skip_while(|elem| **elem != window)
            .skip(1)
            .cloned()
            .collect();
        self.space.map_element(window.clone(), position, false);
        for upper in above {
            let Some(location) = self.space.element_location(&upper) else {
                continue;
            };
            self.space.map_element(upper, location, false);
        }

        if let Some(size) = size
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.with_pending_state(|state| {
                state.size = Some(Size::from(size));
            });
            toplevel.send_configure();
        }

        let info = self.window_info(id, &window);
        if self.windows[index].last_info != info {
            self.windows[index].last_info = info.clone();
            self.emit(|seq| CompositorEvent::WindowUpdated { seq, window: info });
        }
        Ok(())
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
        window.on_commit();
        let location = self.initial_location(&window);
        self.space.map_element(window.clone(), location, true);

        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        let info = self.window_info(id, &window);
        self.windows.push(WindowRecord {
            id,
            window: window.clone(),
            last_info: info.clone(),
        });
        self.emit(|seq| CompositorEvent::WindowMapped { seq, window: info });
        // Newly mapped toplevels take focus (raise + activate + keyboard),
        // and `focus_changed` mirrors the client into the seat's selections.
        self.focus_window(&window);
    }

    /// Maps / unmaps an `xdg_popup` following a commit.
    fn on_popup_commit(&mut self, surface: &WlSurface) {
        let has_buffer =
            with_renderer_surface_state(surface, |rss| rss.buffer().is_some()).unwrap_or(false);
        let mapped = self
            .mapped_popups
            .iter()
            .position(|record| record.surface == *surface);

        match (has_buffer, mapped) {
            (true, None) => {
                let Some(surface_id) = self.surface_id_for(surface) else {
                    return;
                };
                let popup = self.popup_info(surface_id, surface);
                self.mapped_popups.push(PopupRecord {
                    surface_id,
                    surface: surface.clone(),
                });
                self.emit(|seq| CompositorEvent::PopupMapped { seq, popup });
            }
            (false, Some(pos)) => {
                let record = self.mapped_popups.remove(pos);
                self.emit(|seq| CompositorEvent::PopupClosed {
                    seq,
                    surface_id: record.surface_id,
                });
            }
            _ => {}
        }
    }

    /// The [`SurfaceId`] assigned to `surface` at creation, if any.
    fn surface_id_for(&self, surface: &WlSurface) -> Option<SurfaceId> {
        with_states(surface, |states| {
            states
                .data_map
                .get::<SurfaceMarker>()
                .map(|marker| marker.id)
        })
    }

    /// The external metadata snapshot for a mapped popup.
    fn popup_info(&self, surface_id: SurfaceId, surface: &WlSurface) -> PopupInfo {
        let (geometry, parent) = with_states(surface, |states| {
            let attributes = states
                .data_map
                .get::<XdgPopupSurfaceData>()?
                .lock()
                .unwrap();
            Some((attributes.current.geometry, attributes.parent.clone()))
        })
        .unwrap_or_default();
        let window_id = parent.and_then(|parent| self.window_id_for_surface(&parent));
        PopupInfo {
            surface_id,
            window_id,
            geometry: Rect::from(geometry),
        }
    }

    /// A popup is going away; emit `PopupClosed` if it was mapped.
    fn on_popup_gone(&mut self, surface: &WlSurface) {
        if let Some(pos) = self
            .mapped_popups
            .iter()
            .position(|record| record.surface == *surface)
        {
            let record = self.mapped_popups.remove(pos);
            self.emit(|seq| CompositorEvent::PopupClosed {
                seq,
                surface_id: record.surface_id,
            });
        }
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
        match get_role(surface) {
            Some(XDG_TOPLEVEL_ROLE) => self.on_toplevel_gone(surface),
            Some(XDG_POPUP_ROLE) => self.on_popup_gone(surface),
            _ => {}
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
                self.send_mutating_reply(&reply, outcome);
            }
            Command::PointerButton {
                button,
                pressed,
                reply,
            } => {
                let outcome = self.cmd_pointer_button(button, pressed);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::PointerScroll { x, y, reply } => {
                let outcome = self.cmd_pointer_scroll(x, y);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::KeyEvent {
                name,
                pressed,
                reply,
            } => {
                let outcome = self.cmd_key_event(&name, pressed);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::KeyCombo {
                name,
                modifiers,
                reply,
            } => {
                let outcome = self.cmd_key_combo(&name, &modifiers);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::TypeText { text, reply } => {
                let outcome = self.cmd_type_text(&text);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::FocusWindow { id, reply } => {
                let outcome = self.cmd_focus_window(id);
                self.send_mutating_reply(&reply, outcome);
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
            Command::Popups { reply } => {
                let popups = self
                    .mapped_popups
                    .iter()
                    .map(|record| self.popup_info(record.surface_id, &record.surface))
                    .collect();
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value: popups,
                }));
            }
            Command::ResizeWindow { id, size, reply } => {
                let outcome = self
                    .windows
                    .iter()
                    .find(|record| record.id == id)
                    .and_then(|record| record.window.toplevel())
                    .ok_or(Error::UnknownWindow(id))
                    .map(|toplevel| {
                        toplevel.with_pending_state(|state| {
                            state.size = Some(Size::from(size));
                        });
                        toplevel.send_configure();
                    });
                self.send_mutating_reply(&reply, outcome);
            }
            Command::PlaceWindow {
                id,
                position,
                size,
                reply,
            } => {
                let outcome = self.cmd_place_window(id, position, size);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::Damage { since, reply } => {
                let damage = self
                    .damage_log
                    .iter()
                    .filter(|entry| entry.seq > since)
                    .cloned()
                    .collect();
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value: damage,
                }));
            }
            Command::CloseWindow { id, reply } => {
                let outcome = self
                    .windows
                    .iter()
                    .find(|record| record.id == id)
                    .and_then(|record| record.window.toplevel())
                    .ok_or(Error::UnknownWindow(id))
                    .map(|toplevel| toplevel.send_close());
                self.send_mutating_reply(&reply, outcome);
            }
            Command::CaptureWindow { id, reply } => {
                let outcome = self
                    .windows
                    .iter()
                    .find(|record| record.id == id)
                    .ok_or(Error::UnknownWindow(id))
                    .and_then(|record| {
                        let scale = self
                            .outputs
                            .preferred_scale_for(&self.space, &record.window);
                        render::render_window(&record.window, scale)
                    });
                let _ = reply.send(outcome.map(|frame| Reply {
                    seq: self.seq,
                    value: frame,
                }));
            }
            Command::CaptureOutput { name, reply } => {
                let outcome = self
                    .outputs
                    .get(&name)
                    .ok_or_else(|| Error::UnknownOutput(name.clone()))
                    .and_then(|output| render::render_output(&self.space, output));
                let _ = reply.send(outcome.map(|frame| Reply {
                    seq: self.seq,
                    value: frame,
                }));
            }
            Command::Outputs { reply } => {
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value: self.outputs.info(&self.space),
                }));
            }
            Command::UpdateOutput { spec, reply } => {
                let outcome = self
                    .outputs
                    .update(&self.display_handle, &mut self.space, spec);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::RemoveOutput { name, reply } => {
                let outcome = self
                    .outputs
                    .remove(&self.display_handle, &mut self.space, &name);
                self.send_mutating_reply(&reply, outcome);
            }
            Command::SetFrameClockMode { mode, reply } => {
                self.frame_clock.set_mode(mode);
                self.send_mutating_reply(&reply, Ok(()));
            }
            Command::SetSelection {
                kind,
                mime,
                data,
                reply,
            } => {
                // `set_selection` emits the ClipboardUpdated event, which
                // already bumps `seq`; the reply reports the applied state.
                self.set_selection(kind, mime, data);
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value: (),
                }));
            }
            Command::StepFrameClock { duration, reply } => {
                let time = self.frame_clock.step(duration);
                self.send_frames(time);
                self.send_mutating_reply(&reply, Ok(()));
            }
            Command::ClearSelection { kind, reply } => {
                self.clear_selection(kind);
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value: (),
                }));
            }
            Command::ReadSelection { kind, mime, reply } => {
                self.read_selection(kind, mime, reply);
            }
            Command::SelectionMimeTypes { kind, reply } => {
                let value = self.selection_mime_types(kind);
                let _ = reply.send(Ok(Reply {
                    seq: self.seq,
                    value,
                }));
            }
            Command::BumpSeq { reply } => {
                // A pure seq reservation: the mutating reply allocates a
                // fresh point so daemon-side marks share this timeline.
                self.send_mutating_reply(&reply, Ok(()));
            }
        }
        // Commands may have produced protocol traffic (configures, closes).
        if let Err(err) = self.display_handle.flush_clients() {
            debug!("failed to flush clients after command: {err}");
        }
    }

    /// Fires every pending `wl_surface.frame` callback on all mapped
    /// windows' surface trees, stamped with virtual presentation `time`.
    ///
    /// The throttle of [`Duration::ZERO`] and a scanout closure reporting
    /// our one virtual output mean callbacks fire unconditionally, whatever
    /// the surface's occlusion state — the right semantics for a headless
    /// compositor.
    fn send_frames(&mut self, time: Duration) {
        let Some(primary) = self.outputs.primary().cloned() else {
            return;
        };
        for record in &self.windows {
            record
                .window
                .send_frame(&primary, time, Some(Duration::ZERO), |_, _| {
                    Some(primary.clone())
                });
            // `Window::send_frame` covers the toplevel's surface tree;
            // popups anchored to it are separate trees needing their own
            // traversal so `wl_surface.frame` on popups fires too. (The
            // second pass is idempotent: pending callbacks are drained.)
            if let Some(root) = record.window.wl_surface() {
                for (popup, _) in PopupManager::popups_for_surface(&root) {
                    send_frames_surface_tree(
                        popup.wl_surface(),
                        &primary,
                        time,
                        Some(Duration::ZERO),
                        |_, _| Some(primary.clone()),
                    );
                }
            }
        }
        // Frame callbacks are protocol traffic; flush so clients observe
        // them promptly rather than on the next unrelated dispatch.
        if let Err(err) = self.display_handle.flush_clients() {
            debug!("failed to flush clients after frame callbacks: {err}");
        }
    }

    /// One frame-clock tick: in realtime mode advances the virtual clock and
    /// fires pending frame callbacks; while paused, holds them.
    fn frame_clock_tick(&mut self) {
        if let Some(time) = self.frame_clock.tick() {
            self.send_frames(time);
        }
    }

    /// Republishes each mapped surface's scale hints (the
    /// `wp_fractional_scale_v1` preferred scale and the `wl_surface` v6
    /// `preferred_buffer_scale`) from the outputs its window overlaps.
    ///
    /// Both smithay helpers deduplicate internally, so this is a no-op
    /// unless an overlap or a scale changed since the last tick.
    fn update_scale_hints(&mut self) {
        for record in &self.windows {
            let scale = self
                .outputs
                .preferred_scale_for(&self.space, &record.window);
            let Some(root) = record.window.wl_surface() else {
                continue;
            };
            set_surface_tree_hints(&root, scale);
            for (popup, _) in PopupManager::popups_for_surface(&root) {
                set_surface_tree_hints(popup.wl_surface(), scale);
            }
        }
    }

    /// The scale hint appropriate for `surface`, resolved via its owning
    /// window (or a popup's anchor window). Unmapped surfaces get the
    /// primary output's scale.
    fn scale_for_surface(&self, surface: &WlSurface) -> f64 {
        let Some(window_id) = self.window_id_for_surface(surface) else {
            return self.outputs.primary_scale();
        };
        self.windows
            .iter()
            .find(|record| record.id == window_id)
            .map(|record| {
                self.outputs
                    .preferred_scale_for(&self.space, &record.window)
            })
            .unwrap_or_else(|| self.outputs.primary_scale())
    }

    /// Bookkeeping run after every event-loop dispatch.
    fn post_tick(&mut self) {
        self.space.refresh();
        self.update_scale_hints();
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

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // Fill the output the window overlaps, falling back to the primary
        // output when it overlaps none.
        let output = self
            .windows
            .iter()
            .find(|record| {
                record
                    .window
                    .toplevel()
                    .map(|toplevel| *toplevel.wl_surface() == *surface.wl_surface())
                    .unwrap_or(false)
            })
            .and_then(|record| {
                self.space
                    .outputs_for_element(&record.window)
                    .into_iter()
                    .next()
            })
            .or_else(|| self.outputs.primary().cloned());
        let size = output.and_then(|output| {
            self.space
                .output_geometry(&output)
                .map(|geometry| geometry.size)
        });
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
            state.size = size;
        });
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        // Restore client-controlled sizing: clear the state and drop the
        // size constraint so the client picks its own dimensions again.
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
            state.size = None;
        });
        surface.send_configure();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.on_toplevel_gone(surface.wl_surface());
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.on_popup_gone(surface.wl_surface());
    }
}

impl OutputHandler for ScryState {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

impl FractionalScaleHandler for ScryState {
    /// A client bound `wp_fractional_scale_v1` for `surface`: publish the
    /// scale the surface's window already prefers. (smithay also replays any
    /// pre-filled preferred scale to the fresh object, so a surface that
    /// received hints before binding is covered both ways.)
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.scale_for_surface(&surface);
        with_states(&surface, |states| {
            crate::output::apply_scale_hints(&surface, states, scale);
        });
    }
}

impl XdgDecorationHandler for ScryState {
    /// A client created a `zxdg_toplevel_decoration_v1`: report the
    /// compositor's standing preference. We draw no window chrome of our
    /// own, so `ServerSide` merely means "the client should not render
    /// decorations" — the standard headless answer.
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    /// The client expressed a mode preference; honor it so negotiation
    /// always converges.
    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();
    }

    /// The client retracted its preference; fall back to the compositor's.
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}

impl SeatHandler for ScryState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    /// Selection focus follows keyboard focus: the client owning the focused
    /// surface is offered both the clipboard and the primary selection. The
    /// owning [`WindowId`] is tracked in `self.focused` and reported via
    /// [`CompositorEvent::WindowFocused`].
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| surface.client());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
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
smithay::delegate_xdg_decoration!(ScryState);
smithay::delegate_output!(ScryState);
smithay::delegate_seat!(ScryState);
smithay::delegate_xdg_activation!(ScryState);
smithay::delegate_viewporter!(ScryState);
smithay::delegate_fractional_scale!(ScryState);

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

/// Applies `scale` hints to every surface in `root`'s subsurface tree.
fn set_surface_tree_hints(root: &WlSurface, scale: f64) {
    with_surface_tree_downward(
        root,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |surface, states, _| {
            apply_scale_hints(surface, states, scale);
        },
        |_, _, _| true,
    );
}

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
    let mut event_loop: EventLoop<'static, ScryState> =
        match EventLoop::try_new().map_err(|e| e.to_string()) {
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
    let mut state = ScryState::new(
        &display_handle,
        loop_handle.clone(),
        event_loop.get_signal(),
    );

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

        // The virtual frame clock: fires every FRAME_INTERVAL (~60Hz). In
        // realtime mode each tick fires pending wl_surface.frame callbacks;
        // while paused the tick is a no-op, keeping mode switches cheap.
        loop_handle
            .insert_source(Timer::from_duration(FRAME_INTERVAL), |_, _, state| {
                state.frame_clock_tick();
                TimeoutAction::ToDuration(FRAME_INTERVAL)
            })
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
