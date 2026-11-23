//! Session clipboard (wl_data_device) and primary selection
//! (zwp_primary_selection_v1): compositor -> client, client -> compositor,
//! client -> client round trips, `ClipboardUpdated` events and structured
//! errors for empty selections and unoffered MIME types.

mod common;

use std::{
    io::{Read, Write},
    os::fd::AsFd,
    time::Duration,
};

use common::{ClientState, TestClient, wait_for_event};
use scry::{Compositor, CompositorEvent, SelectionKind};
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

const TEXT_MIME: &str = "text/plain";

/// A test client with the seat and both selection protocols bound.
pub struct SelectionClient {
    pub client: TestClient,
    pub seat: wl_seat::WlSeat,
    pub data_device_manager: wl_data_device_manager::WlDataDeviceManager,
    pub data_device: wl_data_device::WlDataDevice,
    pub primary_manager: ZwpPrimarySelectionDeviceManagerV1,
    pub primary_device: ZwpPrimarySelectionDeviceV1,
}

impl SelectionClient {
    /// Connects, binds the selection globals and creates both seat devices.
    /// The client gains keyboard focus by mapping a toplevel.
    pub fn connect(compositor: &Compositor) -> Self {
        let mut client = TestClient::connect(compositor.handle().socket_path());
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
            seat,
            data_device_manager,
            data_device,
            primary_manager,
            primary_device,
        }
    }

    /// Dispatches events until `pred` holds or the timeout elapses.
    pub fn pump_until(&mut self, pred: impl Fn(&ClientState) -> bool) {
        self.client.pump_until(pred)
    }

    /// Gives this client keyboard focus by mapping a toplevel.
    pub fn map_toplevel(
        &mut self,
        width: i32,
        height: i32,
        app_id: &str,
        title: &str,
    ) -> common::Toplevel {
        self.client.map_toplevel(width, height, app_id, title)
    }

    /// Sets this client's `wl_data_device` clipboard, offering `mimes`.
    ///
    /// The returned source's `send` events land in
    /// `state.clipboard.source_sends`; write the payload into the fd there.
    pub fn set_clipboard(&mut self, mimes: &[&str]) -> wl_data_source::WlDataSource {
        let source = self
            .data_device_manager
            .create_data_source(&self.client.qh, ());
        for mime in mimes {
            source.offer(mime.to_string());
        }
        self.data_device.set_selection(Some(&source), 0);
        source
    }

    /// Sets this client's primary selection, offering `mimes`. Send requests
    /// land in `state.primary.source_sends`.
    pub fn set_primary(&mut self, mimes: &[&str]) -> ZwpPrimarySelectionSourceV1 {
        let source = self.primary_manager.create_source(&self.client.qh, ());
        for mime in mimes {
            source.offer(mime.to_string());
        }
        self.primary_device.set_selection(Some(&source), 0);
        source
    }

    /// Answers the first pending `wl_data_source.send` request by writing
    /// `data` and closing the fd. Returns the negotiated mime type.
    pub fn answer_clipboard_send(&mut self, data: &[u8]) -> String {
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
    pub fn answer_primary_send(&mut self, data: &[u8]) -> String {
        self.pump_until(|s| !s.primary.source_sends.is_empty());
        let (mime, fd) = self.client.state.primary.source_sends.pop_front().unwrap();
        let _ = std::fs::File::from(fd).write_all(data);
        mime
    }

    /// Requests `mime` from the client's current `wl_data_device` offer and
    /// returns the stream to read the payload from.
    ///
    /// When the selection is owned by another client, the answer only arrives
    /// once that client dispatches its `send` event — drive its queue before
    /// reading.
    pub fn receive_clipboard_start(&mut self, mime: &str) -> std::os::unix::net::UnixStream {
        let (offer, _) = self
            .client
            .state
            .clipboard
            .offer
            .as_ref()
            .expect("client has no clipboard offer to read");
        let (read_end, write_end) = std::os::unix::net::UnixStream::pair().unwrap();
        read_end
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        offer.receive(mime.to_string(), write_end.as_fd());
        drop(write_end);
        self.client.roundtrip();
        read_end
    }

    /// Requests `mime` from the client's current primary-selection offer and
    /// returns the stream to read the payload from.
    pub fn receive_primary_start(&mut self, mime: &str) -> std::os::unix::net::UnixStream {
        let (offer, _) = self
            .client
            .state
            .primary
            .offer
            .as_ref()
            .expect("client has no primary offer to read");
        let (read_end, write_end) = std::os::unix::net::UnixStream::pair().unwrap();
        read_end
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        offer.receive(mime.to_string(), write_end.as_fd());
        drop(write_end);
        self.client.roundtrip();
        read_end
    }

    /// Reads `mime` from the client's current `wl_data_device` offer,
    /// blocking until the sender closes the transfer.
    pub fn receive_clipboard(&mut self, mime: &str) -> Vec<u8> {
        let mut read_end = self.receive_clipboard_start(mime);
        let mut data = Vec::new();
        read_end
            .read_to_end(&mut data)
            .expect("clipboard read failed");
        data
    }

    /// Reads `mime` from the client's current primary-selection offer.
    pub fn receive_primary(&mut self, mime: &str) -> Vec<u8> {
        let mut read_end = self.receive_primary_start(mime);
        let mut data = Vec::new();
        read_end
            .read_to_end(&mut data)
            .expect("primary read failed");
        data
    }
}

// -- protocol dispatch for selection plumbing --------------------------------

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

// -- tests ---------------------------------------------------------------------

#[test]
fn advertises_seat_and_selection_globals() {
    let compositor = Compositor::spawn().expect("compositor failed to start");
    let client = TestClient::connect(compositor.handle().socket_path());
    let names = client.global_names();
    for interface in [
        "wl_seat",
        "wl_data_device_manager",
        "zwp_primary_selection_device_manager_v1",
    ] {
        assert!(
            names.contains(&interface.to_string()),
            "missing global {interface}"
        );
    }
}

#[test]
fn compositor_clipboard_is_delivered_to_clients() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = SelectionClient::connect(&compositor);
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Clip", "Clip");
    let _ = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    handle
        .set_selection(
            SelectionKind::Clipboard,
            TEXT_MIME.to_string(),
            b"hello clipboard".to_vec(),
        )
        .expect("set_selection failed");

    client.pump_until(|s| s.clipboard.has_selection);
    let (offer, mimes) = client.client.state.clipboard.offer.as_ref().unwrap();
    assert_eq!(mimes, &vec![TEXT_MIME.to_string()]);
    let _ = offer;

    let data = client.receive_clipboard(TEXT_MIME);
    assert_eq!(data, b"hello clipboard");
}

#[test]
fn client_clipboard_emits_event_and_reads_back() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = SelectionClient::connect(&compositor);
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Set", "Set");
    let _ = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    // The focused client owns the selection and negotiates two MIME types.
    let _source = client.set_clipboard(&[TEXT_MIME, "text/html"]);
    client.client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(
            e,
            CompositorEvent::ClipboardUpdated {
                selection: SelectionKind::Clipboard,
                ..
            }
        )
    });
    let CompositorEvent::ClipboardUpdated { mime_types, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        mime_types,
        vec![TEXT_MIME.to_string(), "text/html".to_string()]
    );

    // The same offers are queryable through the handle.
    let reply = handle
        .selection_mime_types(SelectionKind::Clipboard)
        .unwrap();
    assert_eq!(reply.value, mime_types);

    // `read_selection` blocks until the client answers the `send` request,
    // so read on a thread while the test pumps the client's event queue.
    let reader = {
        let handle = handle.clone();
        std::thread::spawn(move || {
            handle.read_selection(SelectionKind::Clipboard, TEXT_MIME.to_string())
        })
    };
    let mime = client.answer_clipboard_send(b"from client");
    assert_eq!(mime, TEXT_MIME);

    let reply = reader
        .join()
        .expect("reader thread panicked")
        .expect("read_selection failed");
    assert_eq!(reply.value, b"from client");
}

#[test]
fn client_clipboard_is_offered_to_other_clients() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    // Client A owns a toplevel (and focus) and sets the clipboard.
    let mut a = SelectionClient::connect(&compositor);
    let _ta = a.map_toplevel(50, 50, "org.scry.A", "A");
    let _ = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    let _source = a.set_clipboard(&[TEXT_MIME]);
    a.client.roundtrip();
    let _ = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::ClipboardUpdated { .. })
    });

    // Client B focuses its own toplevel; the existing selection is offered to it.
    let mut b = SelectionClient::connect(&compositor);
    let _tb = b.map_toplevel(50, 50, "org.scry.B", "B");
    let _ = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });
    b.pump_until(|s| s.clipboard.has_selection);
    assert_eq!(
        b.client.state.clipboard.offer.as_ref().unwrap().1,
        vec![TEXT_MIME.to_string()]
    );

    // B requests the payload; A answers its `send` event.
    let mut read_end = b.receive_clipboard_start(TEXT_MIME);
    let mime = a.answer_clipboard_send(b"A's payload");
    assert_eq!(mime, TEXT_MIME);

    let mut data = Vec::new();
    read_end
        .read_to_end(&mut data)
        .expect("client-to-client clipboard read failed");
    assert_eq!(data, b"A's payload");
}

#[test]
fn compositor_primary_selection_is_delivered_to_clients() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = SelectionClient::connect(&compositor);
    let _toplevel = client.map_toplevel(100, 100, "org.scry.Pri", "Pri");
    let _ = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    handle
        .set_selection(
            SelectionKind::Primary,
            TEXT_MIME.to_string(),
            b"primary payload".to_vec(),
        )
        .unwrap();

    client.pump_until(|s| s.primary.has_selection);
    let data = client.receive_primary(TEXT_MIME);
    assert_eq!(data, b"primary payload");
}

#[test]
fn client_primary_selection_emits_event_and_reads_back() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    let mut client = SelectionClient::connect(&compositor);
    let _toplevel = client.map_toplevel(100, 100, "org.scry.PriSet", "PriSet");
    let _ = wait_for_event(&events, |e| {
        matches!(e, CompositorEvent::WindowMapped { .. })
    });

    let _source = client.set_primary(&[TEXT_MIME]);
    client.client.roundtrip();

    let ev = wait_for_event(&events, |e| {
        matches!(
            e,
            CompositorEvent::ClipboardUpdated {
                selection: SelectionKind::Primary,
                ..
            }
        )
    });
    let CompositorEvent::ClipboardUpdated {
        selection,
        mime_types,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(selection, SelectionKind::Primary);
    assert_eq!(mime_types, vec![TEXT_MIME.to_string()]);

    let reader = {
        let handle = handle.clone();
        std::thread::spawn(move || {
            handle.read_selection(SelectionKind::Primary, TEXT_MIME.to_string())
        })
    };
    let mime = client.answer_primary_send(b"selected text");
    assert_eq!(mime, TEXT_MIME);

    let reply = reader
        .join()
        .expect("reader thread panicked")
        .expect("read_selection failed");
    assert_eq!(reply.value, b"selected text");
}

#[test]
fn selection_errors_and_clear_are_structured() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle().clone();
    let events = handle.subscribe().unwrap();

    // Reading an empty selection is an error, not a hang.
    let err = handle
        .read_selection(SelectionKind::Clipboard, TEXT_MIME.to_string())
        .expect_err("empty clipboard must error");
    assert!(matches!(
        err,
        scry::Error::EmptySelection(SelectionKind::Clipboard)
    ));
    let err = handle
        .read_selection(SelectionKind::Primary, TEXT_MIME.to_string())
        .expect_err("empty primary selection must error");
    assert!(matches!(
        err,
        scry::Error::EmptySelection(SelectionKind::Primary)
    ));

    // An unoffered MIME type reports what is actually offered.
    handle
        .set_selection(
            SelectionKind::Clipboard,
            TEXT_MIME.to_string(),
            b"data".to_vec(),
        )
        .unwrap();
    let err = handle
        .read_selection(
            SelectionKind::Clipboard,
            "application/octet-stream".to_string(),
        )
        .expect_err("unsupported mime must error");
    match err {
        scry::Error::MimeTypeNotOffered {
            kind,
            requested,
            offered,
        } => {
            assert_eq!(kind, SelectionKind::Clipboard);
            assert_eq!(requested, "application/octet-stream");
            assert_eq!(offered, vec![TEXT_MIME.to_string()]);
        }
        other => panic!("expected MimeTypeNotOffered, got {other:?}"),
    }

    // Reading the compositor-provided selection back works too.
    let reply = handle
        .read_selection(SelectionKind::Clipboard, TEXT_MIME.to_string())
        .unwrap();
    assert_eq!(reply.value, b"data");

    // Clearing emits an update with no MIME types and empties the offers.
    handle.clear_selection(SelectionKind::Clipboard).unwrap();
    let ev = wait_for_event(
        &events,
        |e| matches!(e, CompositorEvent::ClipboardUpdated { selection: SelectionKind::Clipboard, mime_types, .. } if mime_types.is_empty()),
    );
    let CompositorEvent::ClipboardUpdated { mime_types, .. } = ev else {
        unreachable!()
    };
    assert!(mime_types.is_empty());
    let reply = handle
        .selection_mime_types(SelectionKind::Clipboard)
        .unwrap();
    assert!(reply.value.is_empty());

    let err = handle
        .read_selection(SelectionKind::Clipboard, TEXT_MIME.to_string())
        .expect_err("cleared clipboard must error");
    assert!(matches!(
        err,
        scry::Error::EmptySelection(SelectionKind::Clipboard)
    ));
}
