//! Integration tests for the targeted input and clipboard control verbs
//! (M2.4): `click`, `drag`, `type_text`, `key_combo`, `clipboard_get` and
//! `clipboard_set` over the daemon's Unix-socket JSON-RPC 2.0 API.
//!
//! The daemon under test is real: each workspace owns a live `scry::Compositor`
//! whose `WAYLAND_DISPLAY` socket accepts real `wayland-client` connections,
//! so pointer/keyboard/text/selection delivery is observed exactly as a real
//! application would see it.

mod common;

use std::{
    io::{Read, Write},
    os::fd::AsFd,
    path::{Path, PathBuf},
    time::Duration,
};

use common::daemon::{RPC_TIMEOUT, RpcClient};
use common::{ClientState, TestClient};
use scry::daemon::{DaemonConfig, DaemonHandle};
use serde_json::{Value, json};
use tempfile::TempDir;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_seat},
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
    zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
    zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};

/// evdev button codes reported through `wl_pointer.button`.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// evdev key codes for the key_combo assertions.
const KEY_C: u32 = 46;
const KEY_LEFTCTRL: u32 = 29;

const TEXT_MIME: &str = "text/plain";

// Until `list_windows` (M2.2) lands on the control API, window ids come
// from the compositor's sequential assignment: the first mapped toplevel
// in a fresh workspace is `1`, the second `2`, and so on.

// -- daemon fixture -----------------------------------------------------------

/// A live daemon bound to a fresh temporary runtime/state directory.
struct TestDaemon {
    /// Held for its `Drop`, which shuts the daemon down cleanly.
    _handle: DaemonHandle,
    _dir: TempDir,
    socket_path: PathBuf,
}

fn spawn_daemon() -> TestDaemon {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let socket_path = dir.path().join("run/scry.sock");
    let state_dir = dir.path().join("state");
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        state_dir,
        default_idle_ttl: Duration::from_secs(300),
        reaper_interval: Duration::from_millis(25),
        change_log_capacity: scry::daemon::DEFAULT_CHANGE_LOG_CAPACITY,
    };
    let handle = DaemonHandle::spawn(config).expect("daemon failed to start");
    TestDaemon {
        _handle: handle,
        _dir: dir,
        socket_path,
    }
}

/// Creates a workspace and returns its `WAYLAND_DISPLAY` socket path.
fn create_workspace(rpc: &mut RpcClient, id: &str, options: Value) -> PathBuf {
    let mut params = json!({"id": id});
    params
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    let result = rpc.call("workspace_create", params);
    PathBuf::from(result["workspace"]["socket_path"].as_str().unwrap())
}

// -- wayland client with selection plumbing -----------------------------------

/// A test client with the seat and both selection protocols bound, so tests
/// can own or read the workspace's clipboard and primary selection.
struct SelectionClient {
    client: TestClient,
    data_device: wl_data_device::WlDataDevice,
    primary_device: ZwpPrimarySelectionDeviceV1,
    data_device_manager: wl_data_device_manager::WlDataDeviceManager,
    primary_manager: ZwpPrimarySelectionDeviceManagerV1,
}

impl SelectionClient {
    /// Connects to a workspace socket, binds the selection globals and
    /// creates both seat devices.
    fn connect(socket_path: &Path) -> Self {
        let mut client = TestClient::connect(socket_path);
        let qh = client.qh.clone();
        let seat = client
            .globals
            .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ())
            .expect("wl_seat not advertised");
        let data_device_manager = client
            .globals
            .bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(&qh, 1..=3, ())
            .expect("wl_data_device_manager not advertised");
        let data_device = data_device_manager.get_data_device(&seat, &qh, ());
        let primary_manager = client
            .globals
            .bind::<ZwpPrimarySelectionDeviceManagerV1, _, _>(&qh, 1..=1, ())
            .expect("zwp_primary_selection_device_manager_v1 not advertised");
        let primary_device = primary_manager.get_device(&seat, &qh, ());
        client.roundtrip();
        Self {
            client,
            data_device,
            primary_device,
            data_device_manager,
            primary_manager,
        }
    }

    fn pump_until(&mut self, pred: impl Fn(&ClientState) -> bool) {
        self.client.pump_until(pred)
    }

    /// Sets this client's `wl_data_device` clipboard, offering `mimes`.
    fn set_clipboard(&mut self, mimes: &[&str]) -> wl_data_source::WlDataSource {
        let source = self
            .data_device_manager
            .create_data_source(&self.client.qh, ());
        for mime in mimes {
            source.offer(mime.to_string());
        }
        self.data_device.set_selection(Some(&source), 0);
        source
    }

    /// Sets this client's primary selection, offering `mimes`.
    fn set_primary(&mut self, mimes: &[&str]) -> ZwpPrimarySelectionSourceV1 {
        let source = self.primary_manager.create_source(&self.client.qh, ());
        for mime in mimes {
            source.offer(mime.to_string());
        }
        self.primary_device.set_selection(Some(&source), 0);
        source
    }

    /// Answers the first pending `wl_data_source.send` request by writing
    /// `data` and closing the fd. Returns the negotiated mime type.
    fn answer_clipboard_send(&mut self, data: &[u8]) -> String {
        self.pump_until(|s| !s.clipboard.source_sends.is_empty());
        let (mime, fd) = self
            .client
            .state
            .clipboard
            .source_sends
            .pop_front()
            .unwrap();
        let _ = std::fs::File::from(fd).write_all(data);
        mime
    }

    /// Answers the first pending primary `send` request.
    fn answer_primary_send(&mut self, data: &[u8]) -> String {
        self.pump_until(|s| !s.primary.source_sends.is_empty());
        let (mime, fd) = self.client.state.primary.source_sends.pop_front().unwrap();
        let _ = std::fs::File::from(fd).write_all(data);
        mime
    }

    /// Reads `mime` from the client's current `wl_data_device` offer,
    /// blocking until the sender closes the transfer.
    fn receive_clipboard(&mut self, mime: &str) -> Vec<u8> {
        let (offer, _) = self
            .client
            .state
            .clipboard
            .offer
            .as_ref()
            .expect("client has no clipboard offer to read");
        let (read_end, write_end) = std::os::unix::net::UnixStream::pair().unwrap();
        read_end.set_read_timeout(Some(RPC_TIMEOUT)).unwrap();
        offer.receive(mime.to_string(), write_end.as_fd());
        drop(write_end);
        self.client.roundtrip();
        let mut data = Vec::new();
        let mut read_end = read_end;
        read_end
            .read_to_end(&mut data)
            .expect("clipboard read failed");
        data
    }

    /// Reads `mime` from the client's current primary-selection offer.
    fn receive_primary(&mut self, mime: &str) -> Vec<u8> {
        let (offer, _) = self
            .client
            .state
            .primary
            .offer
            .as_ref()
            .expect("client has no primary offer to read");
        let (read_end, write_end) = std::os::unix::net::UnixStream::pair().unwrap();
        read_end.set_read_timeout(Some(RPC_TIMEOUT)).unwrap();
        offer.receive(mime.to_string(), write_end.as_fd());
        drop(write_end);
        self.client.roundtrip();
        let mut data = Vec::new();
        let mut read_end = read_end;
        read_end
            .read_to_end(&mut data)
            .expect("primary read failed");
        data
    }
}

// -- protocol dispatch for selection plumbing ---------------------------------

wayland_client::delegate_noop!(ClientState: ignore wl_data_device_manager::WlDataDeviceManager);

impl Dispatch<wl_data_device::WlDataDevice, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.clipboard.offer = Some((id, Vec::new()));
            }
            wl_data_device::Event::Selection { id } => {
                let selected = id.as_ref().is_some_and(|id| {
                    state
                        .clipboard
                        .offer
                        .as_ref()
                        .is_some_and(|(offer, _)| offer == id)
                });
                state.clipboard.has_selection = selected;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(ClientState, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for ClientState {
    fn event(
        state: &mut Self,
        proxy: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && let Some((offer, mimes)) = state.clipboard.offer.as_mut()
            && offer == proxy
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Send { mime_type, fd } = event {
            state.clipboard.source_sends.push_back((mime_type, fd));
        }
    }
}

wayland_client::delegate_noop!(ClientState: ignore ZwpPrimarySelectionDeviceManagerV1);

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                state.primary.offer = Some((offer, Vec::new()));
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => {
                let selected = id.as_ref().is_some_and(|id| {
                    state
                        .primary
                        .offer
                        .as_ref()
                        .is_some_and(|(offer, _)| offer == id)
                });
                state.primary.has_selection = selected;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(ClientState, ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<ZwpPrimarySelectionOfferV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        proxy: &ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event
            && let Some((offer, mimes)) = state.primary.offer.as_mut()
            && offer == proxy
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_source_v1::Event::Send { mime_type, fd } = event {
            state.primary.source_sends.push_back((mime_type, fd));
        }
    }
}

// -- pointer verbs --------------------------------------------------------------

#[test]
fn click_delivers_pointer_events_over_rpc() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "clicks", json!({}));

    let mut client = TestClient::connect(&socket);
    let toplevel = client.map_toplevel(100, 100, "org.scry.Click", "Click");
    client.roundtrip();

    // Output-coordinate click: the client sees pointer enter at the
    // surface-local point plus a press/release pair.
    let result = rpc.call(
        "click",
        json!({"workspace": "clicks", "x": 30.0, "y": 40.0}),
    );
    assert!(result["seq"].as_u64().unwrap() > 0);
    client.pump_until(|s| s.pointer_enters.len() == 1 && s.pointer_buttons.len() == 2);
    let (surface, x, y) = &client.state.pointer_enters[0];
    assert_eq!(*surface, toplevel.surface);
    assert_eq!((*x, *y), (30.0, 40.0));
    assert_eq!(client.state.pointer_buttons[0], (BTN_LEFT, 1));
    assert_eq!(client.state.pointer_buttons[1], (BTN_LEFT, 0));

    // Button selection and click count: two right-button cycles.
    rpc.call(
        "click",
        json!({"workspace": "clicks", "x": 50.0, "y": 60.0, "button": "right", "count": 2}),
    );
    client.pump_until(|s| s.pointer_buttons.len() == 6);
    assert_eq!(client.state.pointer_buttons[2], (BTN_RIGHT, 1));
    assert_eq!(client.state.pointer_buttons[3], (BTN_RIGHT, 0));
    assert_eq!(client.state.pointer_buttons[4], (BTN_RIGHT, 1));
    assert_eq!(client.state.pointer_buttons[5], (BTN_RIGHT, 0));
}

#[test]
fn window_local_click_hits_covered_windows() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "local", json!({}));

    let mut client = TestClient::connect(&socket);
    let bottom = client.map_toplevel(100, 100, "org.scry.Bottom", "Bottom");
    client.roundtrip();
    let top = client.map_toplevel(100, 100, "org.scry.Top", "Top");
    client.roundtrip();

    // New windows cascade to distinct slots (M2.2), so stack `top` over
    // `bottom` explicitly: `place_window` moves without raising, leaving
    // `top` (mapped later) on top.
    rpc.call(
        "place_window",
        json!({"workspace": "local", "window": 2, "x": 0, "y": 0}),
    );
    client.roundtrip();

    // Output coordinates hit the topmost window (`top` covers `bottom`).
    rpc.call("click", json!({"workspace": "local", "x": 10.0, "y": 10.0}));
    client.pump_until(|s| !s.pointer_enters.is_empty());
    assert_eq!(client.state.pointer_enters[0].0, top.surface);

    // Window-local coordinates target `bottom` (the first mapped window,
    // id 1) even though it is fully covered.
    rpc.call(
        "click",
        json!({"workspace": "local", "window": 1, "x": 20.0, "y": 25.0, "button": "middle"}),
    );
    client.pump_until(|s| {
        s.pointer_buttons
            .iter()
            .any(|&(b, st)| b == BTN_MIDDLE && st == 1)
    });
    // The button landed on `bottom`'s surface at local (20, 25): the pointer
    // left `top` and entered `bottom`.
    assert!(
        client
            .state
            .pointer_enters
            .iter()
            .any(|(surf, x, y)| *surf == bottom.surface && *x == 20.0 && *y == 25.0),
        "bottom surface never entered at local coordinates: {:?}",
        client.state.pointer_enters
    );
}

#[test]
fn drag_delivers_motion_between_press_and_release() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "drags", json!({}));

    let mut client = TestClient::connect(&socket);
    let _toplevel = client.map_toplevel(200, 200, "org.scry.Drag", "Drag");
    client.roundtrip();

    let result = rpc.call(
        "drag",
        json!({
            "workspace": "drags",
            "from": {"x": 20.0, "y": 20.0},
            "to": {"x": 100.0, "y": 120.0},
            "steps": 4,
        }),
    );
    assert!(result["seq"].as_u64().unwrap() > 0);

    client.pump_until(|s| s.pointer_buttons.len() >= 2 && s.pointer_motions.len() >= 4);
    assert_eq!(client.state.pointer_buttons[0], (BTN_LEFT, 1));
    assert_eq!(client.state.pointer_buttons[1], (BTN_LEFT, 0));

    // Motions advance monotonically from the start toward the end point.
    let motions = &client.state.pointer_motions;
    let last = motions.last().unwrap();
    assert_eq!(*last, (100.0, 120.0));
    for pair in motions.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        assert!(x1 >= x0 && y1 >= y0, "non-monotonic drag path: {motions:?}");
    }
}

/// Regression: an out-of-bounds `to` must be rejected *before* the
/// pointer button is pressed — a failure mid-drag used to leave a
/// dangling pressed button on the client.
#[test]
fn drag_rejects_bad_endpoints_without_pressing() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "dragerrs", json!({}));

    let mut client = TestClient::connect(&socket);
    let _toplevel = client.map_toplevel(100, 100, "org.scry.DragErrs", "DragErrs");
    client.roundtrip();

    // `to` is window-local but outside the 100x100 window: the drag must
    // fail with a structured error before any button event is delivered.
    let err = rpc.call_err(
        "drag",
        json!({
            "workspace": "dragerrs",
            "from": {"x": 10.0, "y": 10.0},
            "to": {"x": 500.0, "y": 500.0, "window": 1},
        }),
    );
    assert_eq!(err.code, -32602);

    client.roundtrip();
    client.roundtrip();
    assert!(
        client.state.pointer_buttons.is_empty(),
        "drag left a dangling press on the client: {:?}",
        client.state.pointer_buttons
    );

    // A bad `from` (window-local, out of bounds) fails the same way.
    let err = rpc.call_err(
        "drag",
        json!({
            "workspace": "dragerrs",
            "from": {"x": -5.0, "y": 500.0, "window": 1},
            "to": {"x": 10.0, "y": 10.0},
        }),
    );
    assert_eq!(err.code, -32602);
    client.roundtrip();
    assert!(client.state.pointer_buttons.is_empty());

    // And a valid drag still works afterwards: the pointer is not stuck
    // holding the button down from the failed attempts.
    rpc.call(
        "drag",
        json!({
            "workspace": "dragerrs",
            "from": {"x": 10.0, "y": 10.0},
            "to": {"x": 50.0, "y": 50.0},
        }),
    );
    client.pump_until(|s| s.pointer_buttons.len() >= 2);
    assert_eq!(client.state.pointer_buttons[0], (BTN_LEFT, 1));
    assert_eq!(client.state.pointer_buttons[1], (BTN_LEFT, 0));
}

// -- keyboard verbs -----------------------------------------------------------

#[test]
fn type_text_and_key_combo_reach_the_focused_window() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "keys", json!({}));

    let mut client = TestClient::connect(&socket);
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Keys", "Keys");
    client.pump_until(|s| !s.kb_enters.is_empty());

    // XKB fallback path: no text_input bound, each char becomes key events.
    rpc.call("type_text", json!({"workspace": "keys", "text": "hi"}));
    client.pump_until(|s| s.keys.len() >= 4);
    assert_eq!(client.state.typed_text(), "hi");

    // zwp_text_input_v3 path: arbitrary Unicode is committed as a string.
    let _text_input = client.new_text_input();
    client.roundtrip();
    client.state.keys.clear();
    rpc.call("type_text", json!({"workspace": "keys", "text": "héllo ✓"}));
    client.pump_until(|s| !s.ti_strings.is_empty());
    assert_eq!(client.state.ti_strings[0], "héllo ✓");
    assert!(
        client.state.keys.is_empty(),
        "text_input path must not emit XKB key events"
    );

    // key_combo holds the modifier, taps the key, then releases.
    rpc.call(
        "key_combo",
        json!({"workspace": "keys", "key": "c", "modifiers": ["ctrl"]}),
    );
    client.pump_until(|s| s.keys.len() >= 4);
    let keys = &client.state.keys;
    assert_eq!(
        &keys[..4],
        &[(KEY_LEFTCTRL, 1), (KEY_C, 1), (KEY_C, 0), (KEY_LEFTCTRL, 0)],
        "unexpected combo sequence: {keys:?}"
    );
}

#[test]
fn type_text_with_window_focuses_the_target_first() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "focus", json!({}));

    let mut client = TestClient::connect(&socket);
    let first = client.map_toplevel(100, 100, "org.scry.First", "First");
    client.roundtrip();
    let second = client.map_toplevel(100, 100, "org.scry.Second", "Second");
    client.pump_until(|s| s.kb_enters.iter().any(|(surf, _)| *surf == second.surface));

    // `window: 1` focuses the first toplevel before typing; the client sees
    // keyboard leave/enter moving focus back to `first`.
    rpc.call(
        "type_text",
        json!({"workspace": "focus", "window": 1, "text": "x"}),
    );
    client.pump_until(|s| {
        s.kb_leaves.contains(&second.surface)
            && s.kb_enters
                .last()
                .is_some_and(|(surf, _)| *surf == first.surface)
    });
    assert_eq!(client.state.typed_text(), "x");
}

// -- structured errors ----------------------------------------------------------

#[test]
fn invalid_input_params_return_structured_errors() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "errs", json!({}));

    let mut client = TestClient::connect(&socket);
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Errs", "Errs");
    client.roundtrip();

    // Unknown key / modifier names → invalid params (-32602).
    let err = rpc.call_err(
        "key_combo",
        json!({"workspace": "errs", "key": "NoSuchKey123"}),
    );
    assert_eq!(err.code, -32602);
    let err = rpc.call_err(
        "key_combo",
        json!({"workspace": "errs", "key": "c", "modifiers": ["bogus-mod"]}),
    );
    assert_eq!(err.code, -32602);

    // Window-local coordinates outside the window → invalid params.
    let err = rpc.call_err(
        "click",
        json!({"workspace": "errs", "window": 1, "x": 500.0, "y": 500.0}),
    );
    assert_eq!(err.code, -32602);

    // Unknown window id → structured window error (-32004).
    let err = rpc.call_err(
        "click",
        json!({"workspace": "errs", "window": 99, "x": 1.0, "y": 1.0}),
    );
    assert_eq!(err.code, -32004);

    // Malformed params (missing coordinates, bad button, bad types).
    for params in [
        json!({"workspace": "errs", "x": 10.0}),
        json!({"workspace": "errs", "x": 10.0, "y": 10.0, "button": "nuclear"}),
        json!({"workspace": "errs", "x": 10.0, "y": 10.0, "count": 0}),
        json!({"workspace": "errs", "text": 42}),
        json!({"workspace": "errs", "from": {"x": 0.0, "y": 0.0}}), // drag w/o to
    ] {
        let method = if params.get("from").is_some() {
            "drag"
        } else if params.get("text").is_some() {
            "type_text"
        } else {
            "click"
        };
        let err = rpc.call_err(method, params.clone());
        assert_eq!(err.code, -32602, "{method} accepted {params}");
    }
}

#[test]
fn keyboard_verbs_fail_when_nothing_is_focused() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let _socket = create_workspace(&mut rpc, "empty", json!({}));

    // No windows are mapped: there is no focused surface.
    let err = rpc.call_err("type_text", json!({"workspace": "empty", "text": "hi"}));
    assert_eq!(err.code, -32602);
    let err = rpc.call_err("key_combo", json!({"workspace": "empty", "key": "a"}));
    assert_eq!(err.code, -32602);
}

// -- clipboard verbs ------------------------------------------------------------

#[test]
fn clipboard_set_and_get_roundtrip_over_rpc() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "clip", json!({}));

    let mut client = SelectionClient::connect(&socket);
    let _toplevel = client
        .client
        .map_toplevel(100, 100, "org.scry.Clip", "Clip");
    client.client.roundtrip();

    // clipboard_set installs a compositor-side selection; the focused
    // client is offered the mime type and can pull the payload.
    let set = rpc.call(
        "clipboard_set",
        json!({"workspace": "clip", "mime": TEXT_MIME, "data": "hello clipboard"}),
    );
    assert_eq!(set["size"], json!(15));
    client.pump_until(|s| s.clipboard.has_selection);
    assert_eq!(
        client.client.state.clipboard.offer.as_ref().unwrap().1,
        vec![TEXT_MIME.to_string()]
    );
    let data = client.receive_clipboard(TEXT_MIME);
    assert_eq!(data, b"hello clipboard");

    // clipboard_get reads a client-owned selection back. The compositor
    // asks the owning client to write into a pipe, so the RPC runs on a
    // helper thread while this client pumps its queue and answers the send.
    let _source = client.set_clipboard(&[TEXT_MIME, "text/html"]);
    client.client.roundtrip();

    let socket_path = daemon.socket_path.clone();
    let reader = std::thread::spawn(move || {
        let mut rpc = RpcClient::connect(&socket_path);
        rpc.call(
            "clipboard_get",
            json!({"workspace": "clip", "mime": "text/html"}),
        )
    });
    // The daemon negotiated "text/html" out of the offered list.
    let mime = client.answer_clipboard_send(b"<b>hi</b>");
    assert_eq!(mime, "text/html");
    let got = reader.join().expect("clipboard_get thread panicked");
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(got["data_base64"].as_str().unwrap())
        .expect("data_base64 is not valid base64");
    assert_eq!(decoded, b"<b>hi</b>");
    assert_eq!(got["mime"], "text/html");
    assert!(got["mime_types"].as_array().unwrap().len() == 2);

    // Reading an unoffered mime type is a structured error.
    let err = rpc.call_err(
        "clipboard_get",
        json!({"workspace": "clip", "mime": "image/png"}),
    );
    assert_eq!(err.code, -32602);
}

#[test]
fn primary_selection_roundtrips_over_rpc() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "primary", json!({}));

    let mut client = SelectionClient::connect(&socket);
    let _toplevel = client
        .client
        .map_toplevel(100, 100, "org.scry.Prim", "Prim");
    client.client.roundtrip();

    // Compositor-side primary set → client reads it.
    rpc.call(
        "clipboard_set",
        json!({"workspace": "primary", "selection": "primary", "data": "primary text"}),
    );
    client.pump_until(|s| s.primary.has_selection);
    let data = client.receive_primary(TEXT_MIME);
    assert_eq!(data, b"primary text");

    // Client-owned primary → clipboard_get without an explicit mime picks
    // the first offered type.
    let _source = client.set_primary(&[TEXT_MIME]);
    client.client.roundtrip();
    let socket_path = daemon.socket_path.clone();
    let reader = std::thread::spawn(move || {
        let mut rpc = RpcClient::connect(&socket_path);
        rpc.call(
            "clipboard_get",
            json!({"workspace": "primary", "selection": "primary"}),
        )
    });
    let mime = client.answer_primary_send(b"client primary");
    assert_eq!(mime, TEXT_MIME);
    let got = reader.join().expect("clipboard_get thread panicked");
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(got["data_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, b"client primary");
    assert_eq!(got["selection"], "primary");
}

// -- isolation & readOnly -------------------------------------------------------

#[test]
fn input_and_clipboard_stay_isolated_between_workspaces() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket_a = create_workspace(&mut rpc, "iso-a", json!({}));
    let socket_b = create_workspace(&mut rpc, "iso-b", json!({}));

    let mut client_a = SelectionClient::connect(&socket_a);
    let mut client_b = SelectionClient::connect(&socket_b);
    let _ta = client_a.client.map_toplevel(100, 100, "org.scry.A", "A");
    let _tb = client_b.client.map_toplevel(100, 100, "org.scry.B", "B");
    client_a.client.roundtrip();
    client_b.client.roundtrip();

    // Pointer input in A never reaches B's surfaces.
    rpc.call("click", json!({"workspace": "iso-a", "x": 10.0, "y": 10.0}));
    client_a.pump_until(|s| !s.pointer_enters.is_empty());
    client_b.client.roundtrip();
    client_b.client.roundtrip();
    assert!(
        client_b.client.state.pointer_enters.is_empty(),
        "pointer input leaked into workspace B"
    );

    // The clipboard set in A is invisible in B.
    rpc.call(
        "clipboard_set",
        json!({"workspace": "iso-a", "data": "secret a"}),
    );
    client_a.pump_until(|s| s.clipboard.has_selection);
    client_b.client.roundtrip();
    assert!(
        !client_b.client.state.clipboard.has_selection,
        "clipboard leaked into workspace B"
    );
    let err = rpc.call_err("clipboard_get", json!({"workspace": "iso-b"}));
    assert_eq!(err.code, -32602, "iso-b clipboard should be empty");
}

#[test]
fn read_only_workspaces_gate_input_and_clipboard_verbs() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let socket = create_workspace(&mut rpc, "frozen", json!({"read_only": true}));

    let mut client = SelectionClient::connect(&socket);
    let _toplevel = client.client.map_toplevel(100, 100, "org.scry.Ro", "Ro");
    client.client.roundtrip();

    // Every mutating input/clipboard verb is rejected with -32003 before
    // reaching the compositor.
    for (method, params) in [
        ("click", json!({"workspace": "frozen", "x": 5.0, "y": 5.0})),
        (
            "drag",
            json!({"workspace": "frozen", "from": {"x":0.0,"y":0.0}, "to": {"x":1.0,"y":1.0}}),
        ),
        ("type_text", json!({"workspace": "frozen", "text": "hi"})),
        ("key_combo", json!({"workspace": "frozen", "key": "a"})),
        (
            "clipboard_set",
            json!({"workspace": "frozen", "data": "nope"}),
        ),
    ] {
        let err = rpc.call_err(method, params);
        assert_eq!(err.code, -32003, "{method} was not read-only rejected");
    }

    // `clipboard_get` is an observation verb: a wayland client inside the
    // readOnly workspace may still own a selection, and it reads back fine.
    let _source = client.set_clipboard(&[TEXT_MIME]);
    client.client.roundtrip();
    let socket_path = daemon.socket_path.clone();
    let reader = std::thread::spawn(move || {
        let mut rpc = RpcClient::connect(&socket_path);
        rpc.call("clipboard_get", json!({"workspace": "frozen"}))
    });
    let _ = client.answer_clipboard_send(b"still readable");
    let got = reader.join().expect("clipboard_get thread panicked");
    assert_eq!(got["mime"], TEXT_MIME);
}
