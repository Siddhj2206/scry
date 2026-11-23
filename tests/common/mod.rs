//! Shared helpers for integration tests: a minimal wayland-client that talks
//! to a live `scry::Compositor` over its real Unix socket.
#![allow(dead_code)] // each test binary uses a different subset of helpers

use std::{
    collections::VecDeque,
    io::Write,
    os::fd::{AsFd, OwnedFd},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_offer, wl_keyboard, wl_output, wl_pointer,
        wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
        primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3},
        viewporter::client::{wp_viewport, wp_viewporter},
    },
    xdg::{
        activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1},
        decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1},
        shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base},
        xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1},
    },
};

pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Waits for a compositor event matching `pred` and returns it.
pub fn wait_for_event(
    rx: &std::sync::mpsc::Receiver<scry::CompositorEvent>,
    pred: impl Fn(&scry::CompositorEvent) -> bool,
) -> scry::CompositorEvent {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for compositor event"));
        let ev = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for compositor event"));
        if pred(&ev) {
            return ev;
        }
    }
}

/// `wl_data_device` selection state collected by the dispatch impls in
/// `tests/clipboard.rs`.
#[derive(Default)]
pub struct ClipboardChannel {
    /// The most recent `wl_data_device.data_offer` object and its MIME types.
    pub offer: Option<(wl_data_offer::WlDataOffer, Vec<String>)>,
    /// Whether the seat's clipboard selection currently holds `offer`.
    pub has_selection: bool,
    /// `wl_data_source.send` requests received: `(mime_type, fd)` — the test
    /// writes the payload into `fd`.
    pub source_sends: VecDeque<(String, OwnedFd)>,
}

/// `zwp_primary_selection_v1` state collected by the dispatch impls in
/// `tests/clipboard.rs`.
#[derive(Default)]
pub struct PrimaryChannel {
    /// The most recent primary `data_offer` object and its MIME types.
    pub offer: Option<(ZwpPrimarySelectionOfferV1, Vec<String>)>,
    /// Whether the seat's primary selection currently holds `offer`.
    pub has_selection: bool,
    /// `zwp_primary_selection_source_v1.send` requests received: `(mime_type,
    /// fd)` — the test writes the payload into `fd`.
    pub source_sends: VecDeque<(String, OwnedFd)>,
}

/// Events collected for one bound `wl_output` (plus its `zxdg_output_v1`).
///
/// Instances live in [`ClientState::output_details`] at the index passed as
/// the dispatch userdata when binding.
#[derive(Default, Debug)]
pub struct OutputDetail {
    /// `wl_output.name`.
    pub name: Option<String>,
    /// `wl_output.description`.
    pub description: Option<String>,
    /// `wl_output.geometry` make/model strings.
    pub make_model: Option<(String, String)>,
    /// The last `wl_output.mode` received: `(width, height, refresh_mhz)`.
    pub mode: Option<(i32, i32, i32)>,
    /// The last `wl_output.scale` factor received.
    pub scale: Option<i32>,
    /// Number of `wl_output.done` events seen.
    pub dones: usize,
    /// `zxdg_output_v1.logical_position`.
    pub xdg_logical_position: Option<(i32, i32)>,
    /// `zxdg_output_v1.logical_size`.
    pub xdg_logical_size: Option<(i32, i32)>,
    /// `zxdg_output_v1.name`.
    pub xdg_name: Option<String>,
}

#[derive(Default)]
pub struct ClientState {
    pub shm_formats: Vec<wl_shm::Format>,
    pub configure_serials: VecDeque<u32>,
    pub toplevel_configures: VecDeque<(i32, i32)>,
    /// `xdg_toplevel.configure` state arrays (raw u32 enum values),
    /// parallel to `toplevel_configures`.
    pub toplevel_states: VecDeque<Vec<u32>>,
    pub close_requested: bool,
    pub outputs: Vec<String>,
    pub output_scales: Vec<i32>,
    /// Per-`wl_output`-resource event details, indexed by binding userdata.
    pub output_details: Vec<OutputDetail>,
    /// `wp_fractional_scale_v1.preferred_scale` factors (fixed-point n/120).
    pub preferred_scales: VecDeque<u32>,
    /// `zxdg_toplevel_decoration_v1.configure` modes, in arrival order.
    pub decoration_modes: Vec<zxdg_toplevel_decoration_v1::Mode>,
    /// `wl_registry.global_remove` names observed.
    pub removed_globals: Vec<u32>,
    pub enters: usize,
    pub frame_callbacks: usize,
    /// `xdg_popup.configure` events: `(x, y, width, height)`.
    pub popup_configures: VecDeque<(i32, i32, i32, i32)>,
    /// `xdg_popup.popup_done` count.
    pub popup_done: usize,
    /// `callback_data` (presentation time, ms) of every `wl_callback.done`.
    pub frame_times: Vec<u32>,
    /// `wl_data_device` clipboard plumbing.
    pub clipboard: ClipboardChannel,
    /// `zwp_primary_selection_v1` plumbing.
    pub primary: PrimaryChannel,

    // -- input ------------------------------------------------------------
    pub seat_caps: u32,
    pub pointer: Option<wl_pointer::WlPointer>,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    /// (surface, surface-local x, y)
    pub pointer_enters: Vec<(wl_surface::WlSurface, f64, f64)>,
    pub pointer_leaves: Vec<wl_surface::WlSurface>,
    /// (surface-local x, y)
    pub pointer_motions: Vec<(f64, f64)>,
    /// (button, state: 1 pressed / 0 released)
    pub pointer_buttons: Vec<(u32, u32)>,
    /// The last serial delivered with a pointer button event.
    pub last_button_serial: u32,
    /// (axis: 0 vertical / 1 horizontal, value)
    pub pointer_axis: Vec<(u32, f64)>,
    /// Raw keymap payload from `wl_keyboard.keymap` (XKB v1 text format).
    pub keymap: Option<Vec<u8>>,
    /// (surface, serial)
    pub kb_enters: Vec<(wl_surface::WlSurface, u32)>,
    pub kb_leaves: Vec<wl_surface::WlSurface>,
    /// (evdev keycode, state: 1 pressed / 0 released)
    pub keys: Vec<(u32, u32)>,
    /// Latest (depressed, latched, locked, group) from `wl_keyboard.modifiers`.
    pub mods: (u32, u32, u32, u32),
    /// Every modifiers event seen so far, in order.
    pub mods_log: Vec<(u32, u32, u32, u32)>,
    /// zwp_text_input_v3 events
    pub ti_enters: usize,
    pub ti_leaves: usize,
    pub ti_strings: Vec<String>,
    pub ti_dones: Vec<u32>,
    /// xdg_activation token produced by `xdg_activation_token_v1.done`.
    pub activation_token: Option<String>,
}

impl ClientState {
    /// Decodes every delivered key press through the advertised keymap and
    /// returns the UTF-8 text a real client would have typed.
    pub fn typed_text(&self) -> String {
        use smithay::input::keyboard::xkb;
        let data = self.keymap.as_ref().expect("client got no keymap");
        // The keymap payload is NUL-terminated; strip it for String::new.
        let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            String::from_utf8_lossy(&data[..end]).into_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("keymap failed to compile");
        let mut state = xkb::State::new(&keymap);
        let mut out = String::new();
        for &(key, pressed) in &self.keys {
            let code = xkb::Keycode::new(key + 8);
            if pressed == 1 {
                state.update_key(code, xkb::KeyDirection::Down);
                out.push_str(&state.key_get_utf8(code));
            } else {
                state.update_key(code, xkb::KeyDirection::Up);
            }
        }
        out
    }
}

/// A connected test client with the core globals bound.
pub struct TestClient {
    pub state: ClientState,
    pub globals: GlobalList,
    pub eq: EventQueue<ClientState>,
    pub qh: QueueHandle<ClientState>,
    pub compositor: wl_compositor::WlCompositor,
    pub shm: wl_shm::WlShm,
    pub subcompositor: Option<wayland_client::protocol::wl_subcompositor::WlSubcompositor>,
    pub wm_base: xdg_wm_base::XdgWmBase,
    pub output: Option<wl_output::WlOutput>,
    pub seat: Option<wl_seat::WlSeat>,
    pub text_input_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    pub activation: Option<xdg_activation_v1::XdgActivationV1>,
}

impl TestClient {
    pub fn connect(socket_path: &Path) -> Self {
        let stream =
            UnixStream::connect(socket_path).expect("failed to connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("failed to create wayland connection");
        let (globals, eq) =
            registry_queue_init::<ClientState>(&conn).expect("registry init failed");
        let qh = eq.handle();

        let compositor = globals
            .bind::<wl_compositor::WlCompositor, _, _>(&qh, 4..=6, ())
            .expect("wl_compositor not advertised");
        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
            .expect("wl_shm not advertised");
        let wm_base = globals
            .bind::<xdg_wm_base::XdgWmBase, _, _>(&qh, 1..=6, ())
            .expect("xdg_wm_base not advertised");
        let subcompositor = globals
            .bind::<wayland_client::protocol::wl_subcompositor::WlSubcompositor, _, _>(
                &qh,
                1..=1,
                (),
            )
            .ok();
        let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ()).ok();
        let text_input_manager = globals
            .bind::<zwp_text_input_manager_v3::ZwpTextInputManagerV3, _, _>(&qh, 1..=1, ())
            .ok();
        let activation = globals
            .bind::<xdg_activation_v1::XdgActivationV1, _, _>(&qh, 1..=1, ())
            .ok();

        let mut client = Self {
            state: ClientState::default(),
            globals,
            eq,
            qh,
            compositor,
            shm,
            subcompositor,
            wm_base,
            output: None,
            seat,
            text_input_manager,
            activation,
        };
        if client
            .globals
            .contents()
            .clone_list()
            .iter()
            .any(|g| g.interface == "wl_output")
        {
            client.output = Some(client.bind_output(0));
        }
        client.roundtrip();
        client
    }

    /// Names of all advertised globals.
    pub fn global_names(&self) -> Vec<String> {
        self.globals
            .contents()
            .clone_list()
            .iter()
            .map(|g| g.interface.clone())
            .collect()
    }

    /// One server roundtrip, processing all pending events.
    pub fn roundtrip(&mut self) {
        self.eq
            .roundtrip(&mut self.state)
            .expect("roundtrip failed");
    }

    /// Binds the `nth` advertised `wl_output` global (0-based), tracking its
    /// events in `state.output_details`, and returns the resource.
    pub fn bind_output(&mut self, nth: usize) -> wl_output::WlOutput {
        let global = self
            .globals
            .contents()
            .clone_list()
            .iter()
            .filter(|g| g.interface == "wl_output")
            .nth(nth)
            .expect("not enough wl_output globals advertised")
            .clone();
        let index = self.state.output_details.len();
        self.state.output_details.push(OutputDetail::default());
        self.globals.registry().bind::<wl_output::WlOutput, _, _>(
            global.name,
            global.version,
            &self.qh,
            index,
        )
    }

    /// Dispatches events until `pred` holds or the timeout elapses.
    ///
    /// Each iteration is a server roundtrip (`wl_display.sync`), so the loop
    /// can never block indefinitely on a quiet socket — only a genuinely
    /// deadlocked compositor could stall it.
    pub fn pump_until(&mut self, pred: impl Fn(&ClientState) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !pred(&self.state) {
            self.roundtrip();
            assert!(Instant::now() < deadline, "timed out pumping client events");
        }
    }

    /// Allocates a wl_shm buffer filled with a solid 0xAARRGGBB pixel.
    pub fn shm_buffer(&self, width: i32, height: i32, argb: u32) -> wl_buffer::WlBuffer {
        self.shm_buffer_pixels(
            width,
            height,
            &vec![argb; (width * height) as usize],
            wl_shm::Format::Argb8888,
        )
    }

    /// Allocates a wl_shm buffer from explicit 0xAARRGGBB pixels and a format.
    ///
    /// `pixels` is row-major; for `Argb8888`/`Xrgb8888` each u32 is written
    /// little-endian, matching the shm wire layout.
    pub fn shm_buffer_pixels(
        &self,
        width: i32,
        height: i32,
        pixels: &[u32],
        format: wl_shm::Format,
    ) -> wl_buffer::WlBuffer {
        assert_eq!(pixels.len(), (width * height) as usize);
        let mut file = tempfile::tempfile().expect("tempfile failed");
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for pixel in pixels {
            data.extend_from_slice(&pixel.to_le_bytes());
        }
        file.write_all(&data).expect("write shm data failed");
        let pool = self
            .shm
            .create_pool(file.as_fd(), width * height * 4, &self.qh, ());
        pool.create_buffer(0, width, height, width * 4, format, &self.qh, ())
    }

    /// Creates an xdg toplevel, waits for the initial configure (auto-acked by
    /// the dispatch impl), attaches a buffer and commits. Returns once the
    /// commit request has been flushed to the server; callers should wait for
    /// `WindowMapped` on the event stream to observe the map.
    ///
    /// Note: the initial `surface.commit()` produces a `SurfaceCommitted`
    /// event with `window_id: None` — subscribers interested in the buffer
    /// commit should wait for the `SurfaceCommitted` that follows
    /// `WindowMapped`.
    pub fn map_toplevel(&mut self, width: i32, height: i32, app_id: &str, title: &str) -> Toplevel {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xsurface = self.wm_base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xsurface.get_toplevel(&self.qh, ());
        toplevel.set_app_id(app_id.to_string());
        toplevel.set_title(title.to_string());
        surface.commit();
        self.pump_until(|s| !s.configure_serials.is_empty());
        let buffer = self.shm_buffer(width, height, 0xff33_6699);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        self.roundtrip();
        Toplevel {
            surface,
            xsurface,
            toplevel,
            buffer,
        }
    }

    /// Creates an enabled `zwp_text_input_v3` instance on the compositor's
    /// seat. Requires the manager global; panics if it is missing.
    pub fn new_text_input(&self) -> zwp_text_input_v3::ZwpTextInputV3 {
        let seat = self.seat.as_ref().expect("wl_seat not advertised");
        let manager = self
            .text_input_manager
            .as_ref()
            .expect("zwp_text_input_manager_v3 not advertised");
        let text_input = manager.get_text_input(seat, &self.qh, ());
        text_input.enable();
        text_input.commit();
        text_input
    }

    /// Runs an `xdg_activation_v1` activation request for `surface`:
    /// creates a token, commits it, then activates the surface with it.
    pub fn activate(&mut self, surface: &wl_surface::WlSurface) {
        let activation = self
            .activation
            .clone()
            .expect("xdg_activation_v1 not advertised");
        let token = activation.get_activation_token(&self.qh, ());
        token.commit();
        self.pump_until(|s| s.activation_token.is_some());
        let token_string = self.state.activation_token.take().unwrap();
        activation.activate(token_string, surface);
        self.roundtrip();
    }
}

pub struct Toplevel {
    pub surface: wl_surface::WlSurface,
    pub xsurface: xdg_surface::XdgSurface,
    pub toplevel: xdg_toplevel::XdgToplevel,
    pub buffer: wl_buffer::WlBuffer,
}

// -- dispatch boilerplate -----------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::GlobalRemove { name } = event {
            state.removed_globals.push(name);
        }
    }
}

wayland_client::delegate_noop!(ClientState: wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ClientState: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(ClientState: wayland_client::protocol::wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(ClientState: wayland_client::protocol::wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(ClientState: xdg_positioner::XdgPositioner);

impl Dispatch<xdg_popup::XdgPopup, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => state.popup_configures.push_back((x, y, width, height)),
            xdg_popup::Event::PopupDone => state.popup_done += 1,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_buffer.release is expected once the compositor drops a buffer.
    }
}

impl Dispatch<wl_shm::WlShm, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_shm::WlShm,
        event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format {
            format: WEnum::Value(format),
        } = event
        {
            state.shm_formats.push(format);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::Enter { .. } = event {
            state.enters += 1;
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let detail = &mut state.output_details[*index];
        match event {
            wl_output::Event::Name { name } => {
                state.outputs.push(name.clone());
                detail.name = Some(name);
            }
            wl_output::Event::Description { description } => {
                detail.description = Some(description);
            }
            wl_output::Event::Geometry { make, model, .. } => {
                detail.make_model = Some((make, model));
            }
            wl_output::Event::Mode {
                width,
                height,
                refresh,
                ..
            } => {
                detail.mode = Some((width, height, refresh));
            }
            wl_output::Event::Scale { factor } => {
                state.output_scales.push(factor);
                detail.scale = Some(factor);
            }
            wl_output::Event::Done => detail.dones += 1,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(ClientState: wp_viewporter::WpViewporter);
wayland_client::delegate_noop!(ClientState: wp_viewport::WpViewport);
wayland_client::delegate_noop!(ClientState: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(ClientState: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
wayland_client::delegate_noop!(ClientState: zxdg_output_manager_v1::ZxdgOutputManagerV1);

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale, .. } = event {
            state.preferred_scales.push_back(scale);
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure {
            mode: WEnum::Value(mode),
        } = event
        {
            state.decoration_modes.push(mode);
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let detail = &mut state.output_details[*index];
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                detail.xdg_logical_position = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                detail.xdg_logical_size = Some((width, height));
            }
            zxdg_output_v1::Event::Name { name } => detail.xdg_name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ClientState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for ClientState {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.configure_serials.push_back(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                state.toplevel_configures.push_back((width, height));
                // The states array is a flat u32 blob on the wire.
                state.toplevel_states.push_back(
                    states
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u32::from_ne_bytes(*c))
                        .collect(),
                );
            }
            xdg_toplevel::Event::Close => state.close_requested = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClientState {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(capability),
            } => {
                state.seat_caps = capability.bits();
                if capability.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                    state.pointer = Some(proxy.get_pointer(qh, ()));
                }
                if capability.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                    state.keyboard = Some(proxy.get_keyboard(qh, ()));
                }
            }
            wl_seat::Event::Name { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => state.pointer_enters.push((surface, surface_x, surface_y)),
            wl_pointer::Event::Leave { surface, .. } => state.pointer_leaves.push(surface),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.pointer_motions.push((surface_x, surface_y)),
            wl_pointer::Event::Button {
                serial,
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                state.last_button_serial = serial;
                state.pointer_buttons.push((
                    button,
                    u32::from(button_state == wl_pointer::ButtonState::Pressed),
                ));
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => {
                let axis = match axis {
                    wl_pointer::Axis::VerticalScroll => 0,
                    wl_pointer::Axis::HorizontalScroll => 1,
                    _ => 2,
                };
                state.pointer_axis.push((axis, value));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                assert_eq!(
                    format,
                    WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                    "unexpected keymap format"
                );
                let file = std::fs::File::from(fd);
                let mut data = vec![0u8; size as usize];
                // The fd is a dup of the keymap file shared across seats and
                // clients; read at offset 0 without touching its shared
                // position (a second client would otherwise hit EOF).
                std::os::unix::fs::FileExt::read_exact_at(&file, &mut data, 0)
                    .expect("keymap read failed");
                state.keymap = Some(data);
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => state.kb_enters.push((surface, serial)),
            wl_keyboard::Event::Leave { surface, .. } => state.kb_leaves.push(surface),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => state
                .keys
                .push((key, u32::from(key_state == wl_keyboard::KeyState::Pressed))),
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                state.mods = (mods_depressed, mods_latched, mods_locked, group);
                state
                    .mods_log
                    .push((mods_depressed, mods_latched, mods_locked, group));
            }
            wl_keyboard::Event::RepeatInfo { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<zwp_text_input_manager_v3::ZwpTextInputManagerV3, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        _event: zwp_text_input_manager_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { .. } => state.ti_enters += 1,
            zwp_text_input_v3::Event::Leave { .. } => state.ti_leaves += 1,
            zwp_text_input_v3::Event::CommitString { text: Some(text) } => {
                state.ti_strings.push(text)
            }
            zwp_text_input_v3::Event::Done { serial } => state.ti_dones.push(serial),
            _ => {}
        }
    }
}

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &xdg_activation_v1::XdgActivationV1,
        _event: xdg_activation_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.activation_token = Some(token);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { callback_data } = event {
            state.frame_callbacks += 1;
            state.frame_times.push(callback_data);
        }
    }
}
