//! Session clipboard (`wl_data_device`) and primary selection
//! (`zwp_primary_selection_v1`) support.
//!
//! Wayland selections are per-seat: the compositor advertises
//! `wl_data_device_manager` and `zwp_primary_selection_device_manager_v1`,
//! and clients negotiate MIME types through offer objects before any bytes
//! move. This module owns:
//!
//! - the smithay [`SelectionHandler`] plumbing bridging both protocols into
//!   [`CompositorEvent::ClipboardUpdated`] events,
//! - the compositor-provided selection installed by
//!   [`CompositorHandle::set_selection`](crate::CompositorHandle::set_selection)
//!   (clients read it through [`SelectionHandler::send_selection`]), and
//! - the handle-facing commands for reading, setting, clearing and querying
//!   selections.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::mpsc::Sender,
};

use smithay::{
    input::Seat,
    reexports::{
        calloop::{Interest, Mode, PostAction, generic::Generic},
        wayland_server::protocol::wl_data_device_manager::DndAction,
    },
    wayland::selection::{
        SelectionHandler, SelectionSource, SelectionTarget,
        data_device::{
            ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
            SelectionRequestError as DataDeviceRequestError, ServerDndGrabHandler,
            clear_data_device_selection, current_data_device_selection_userdata,
            default_action_chooser, request_data_device_client_selection,
            set_data_device_selection,
        },
        primary_selection::{
            PrimarySelectionHandler, PrimarySelectionState,
            SelectionRequestError as PrimaryRequestError, clear_primary_selection,
            current_primary_selection_userdata, request_primary_client_selection,
            set_primary_selection,
        },
    },
};
use tracing::warn;

use crate::{
    error::Error,
    event::CompositorEvent,
    handle::{Outcome, Reply},
    state::ScryState,
};

/// Which per-seat selection a handle operation or event targets.
///
/// Wayland has two orthogonal session selections: the explicit copy/paste
/// clipboard and the X11-style primary selection (select-to-copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionKind {
    /// The explicit copy/paste clipboard (`wl_data_device`).
    Clipboard,
    /// The primary selection (`zwp_primary_selection_v1`).
    Primary,
}

impl SelectionKind {
    fn from_target(target: SelectionTarget) -> Self {
        match target {
            SelectionTarget::Clipboard => Self::Clipboard,
            SelectionTarget::Primary => Self::Primary,
        }
    }
}

impl std::fmt::Display for SelectionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clipboard => f.write_str("clipboard"),
            Self::Primary => f.write_str("primary"),
        }
    }
}

/// The contents of a compositor-provided selection: the `(mime_type, bytes)`
/// pairs offered to clients during MIME negotiation.
#[derive(Debug, Clone)]
pub(crate) struct ServerSelection {
    contents: Vec<(String, Vec<u8>)>,
}

impl ServerSelection {
    fn mime_types(&self) -> Vec<String> {
        self.contents.iter().map(|(mime, _)| mime.clone()).collect()
    }

    fn get(&self, mime: &str) -> Option<&[u8]> {
        self.contents
            .iter()
            .find(|(m, _)| m == mime)
            .map(|(_, data)| data.as_slice())
    }
}

/// The compositor's cached view of what each selection currently offers.
///
/// Client selections are opaque after `new_selection` fires, so the offered
/// MIME types are recorded here for `selection_mime_types` queries and for
/// describing [`Error::MimeTypeNotOffered`] failures.
#[derive(Debug, Default)]
pub(crate) struct SelectionOffers {
    clipboard: Vec<String>,
    primary: Vec<String>,
}

impl SelectionOffers {
    /// MIME types `kind` currently offers; empty when unset.
    pub fn get(&self, kind: SelectionKind) -> &[String] {
        match kind {
            SelectionKind::Clipboard => &self.clipboard,
            SelectionKind::Primary => &self.primary,
        }
    }

    fn set(&mut self, kind: SelectionKind, mime_types: Vec<String>) {
        match kind {
            SelectionKind::Clipboard => self.clipboard = mime_types,
            SelectionKind::Primary => self.primary = mime_types,
        }
    }
}

/// Why forwarding a read request to the owning client failed, unified over
/// the two protocol-specific `SelectionRequestError` enums.
enum ClientSelectionError {
    NoSelection,
    MimeTypeNotOffered,
    /// The current selection is compositor-provided: serve it from
    /// [`current_data_device_selection_userdata`] / [`current_primary_selection_userdata`].
    ServerSide,
}

impl From<DataDeviceRequestError> for ClientSelectionError {
    fn from(err: DataDeviceRequestError) -> Self {
        match err {
            DataDeviceRequestError::NoSelection => Self::NoSelection,
            DataDeviceRequestError::InvalidMimetype => Self::MimeTypeNotOffered,
            DataDeviceRequestError::ServerSideSelection => Self::ServerSide,
        }
    }
}

impl From<PrimaryRequestError> for ClientSelectionError {
    fn from(err: PrimaryRequestError) -> Self {
        match err {
            PrimaryRequestError::NoSelection => Self::NoSelection,
            PrimaryRequestError::InvalidMimetype => Self::MimeTypeNotOffered,
            PrimaryRequestError::ServerSideSelection => Self::ServerSide,
        }
    }
}

impl ScryState {
    /// Installs a compositor-provided selection of `kind`: focused clients
    /// are offered `mime` and receive `data` through
    /// [`SelectionHandler::send_selection`] when they request it.
    ///
    /// Replaces the current selection; client sources lose ownership per the
    /// protocol (their `cancelled` event is emitted by smithay).
    pub(crate) fn set_selection(&mut self, kind: SelectionKind, mime: String, data: Vec<u8>) {
        let selection = ServerSelection {
            contents: vec![(mime, data)],
        };
        let mime_types = selection.mime_types();
        match kind {
            SelectionKind::Clipboard => {
                set_data_device_selection(
                    &self.display_handle,
                    &self.seat,
                    mime_types.clone(),
                    selection,
                );
            }
            SelectionKind::Primary => {
                set_primary_selection(
                    &self.display_handle,
                    &self.seat,
                    mime_types.clone(),
                    selection,
                );
            }
        }
        self.selection_offers.set(kind, mime_types.clone());
        self.emit(|seq| CompositorEvent::ClipboardUpdated {
            seq,
            selection: kind,
            mime_types,
        });
    }

    /// Clears the current selection of `kind`.
    pub(crate) fn clear_selection(&mut self, kind: SelectionKind) {
        match kind {
            SelectionKind::Clipboard => {
                clear_data_device_selection(&self.display_handle, &self.seat);
            }
            SelectionKind::Primary => {
                clear_primary_selection(&self.display_handle, &self.seat);
            }
        }
        self.selection_offers.set(kind, Vec::new());
        self.emit(|seq| CompositorEvent::ClipboardUpdated {
            seq,
            selection: kind,
            mime_types: Vec::new(),
        });
    }

    /// The MIME types `kind` currently offers; empty when unset.
    pub(crate) fn selection_mime_types(&self, kind: SelectionKind) -> Vec<String> {
        self.selection_offers.get(kind).to_vec()
    }

    /// Reads the current selection negotiated to `mime`.
    ///
    /// For a client-owned selection the owning client is asked to write into
    /// a pipe; its reply is read back asynchronously on the event loop so the
    /// compositor never blocks. For a compositor-provided selection the
    /// stored bytes are returned directly.
    pub(crate) fn read_selection(
        &mut self,
        kind: SelectionKind,
        mime: String,
        reply: Sender<Outcome<Vec<u8>>>,
    ) {
        let (read_end, write_end) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(err) => {
                let _ = reply.send(Err(Error::Io(err)));
                return;
            }
        };
        if let Err(err) = read_end.set_nonblocking(true) {
            let _ = reply.send(Err(Error::Io(err)));
            return;
        }

        match self.request_client_selection(kind, mime.clone(), write_end.into()) {
            Ok(()) => self.watch_selection_read(read_end, reply),
            Err(ClientSelectionError::NoSelection) => {
                let _ = reply.send(Err(Error::EmptySelection(kind)));
            }
            Err(ClientSelectionError::MimeTypeNotOffered) => {
                let _ = reply.send(Err(Error::MimeTypeNotOffered {
                    kind,
                    requested: mime,
                    offered: self.selection_offers.get(kind).to_vec(),
                }));
            }
            Err(ClientSelectionError::ServerSide) => {
                let outcome = match self
                    .server_selection(kind)
                    .and_then(|s| s.get(&mime).map(|data| data.to_vec()))
                {
                    Some(value) => Ok(Reply {
                        seq: self.seq,
                        value,
                    }),
                    None => Err(Error::MimeTypeNotOffered {
                        kind,
                        requested: mime,
                        offered: self.selection_offers.get(kind).to_vec(),
                    }),
                };
                let _ = reply.send(outcome);
            }
        }
    }

    /// Asks the owning client to write the current `kind` selection as `mime`
    /// into `fd`.
    fn request_client_selection(
        &self,
        kind: SelectionKind,
        mime: String,
        fd: std::os::fd::OwnedFd,
    ) -> Result<(), ClientSelectionError> {
        match kind {
            SelectionKind::Clipboard => {
                request_data_device_client_selection(&self.seat, mime, fd).map_err(Into::into)
            }
            SelectionKind::Primary => {
                request_primary_client_selection(&self.seat, mime, fd).map_err(Into::into)
            }
        }
    }

    /// The compositor-provided contents of `kind`'s selection, if it is
    /// currently server-side.
    fn server_selection(&self, kind: SelectionKind) -> Option<ServerSelection> {
        match kind {
            SelectionKind::Clipboard => {
                current_data_device_selection_userdata(&self.seat).map(|s| s.clone())
            }
            SelectionKind::Primary => {
                current_primary_selection_userdata(&self.seat).map(|s| s.clone())
            }
        }
    }

    /// Streams `read_end` on the event loop until the owning client closes
    /// it, then replies with the accumulated selection bytes.
    fn watch_selection_read(&mut self, read_end: UnixStream, reply: Sender<Outcome<Vec<u8>>>) {
        let mut data = Vec::new();
        let result = self.loop_handle.insert_source(
            Generic::new(read_end, Interest::READ, Mode::Level),
            move |_, stream, state| {
                let mut buf = [0u8; 16 * 1024];
                // `Generic` hands back `&mut NoIoDrop<UnixStream>`; `Read` is
                // implemented for `&UnixStream`, so bind the reference.
                let mut stream = &**stream;
                match stream.read(&mut buf) {
                    Ok(0) => {
                        // EOF: the owning client closed its end of the pipe.
                        let _ = reply.send(Ok(Reply {
                            seq: state.seq,
                            value: std::mem::take(&mut data),
                        }));
                        Ok(PostAction::Remove)
                    }
                    Ok(n) => {
                        data.extend_from_slice(&buf[..n]);
                        Ok(PostAction::Continue)
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        Ok(PostAction::Continue)
                    }
                    Err(err) => {
                        let _ = reply.send(Err(Error::Io(err)));
                        Ok(PostAction::Remove)
                    }
                }
            },
        );
        if result.is_err() {
            warn!("failed to register selection read source on the event loop");
        }
    }
}

// -- smithay handler implementations ------------------------------------------

impl SelectionHandler for ScryState {
    type SelectionUserData = ServerSelection;

    /// A client set (or cleared, `source: None`) a selection. The compositor
    /// records the offered MIME types and emits `ClipboardUpdated`.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        let kind = SelectionKind::from_target(ty);
        let mime_types = source
            .as_ref()
            .map(|source| source.mime_types())
            .unwrap_or_default();
        self.selection_offers.set(kind, mime_types.clone());
        self.emit(|seq| CompositorEvent::ClipboardUpdated {
            seq,
            selection: kind,
            mime_types,
        });
    }

    /// A client asked to read a compositor-provided selection: write the
    /// stored bytes for the negotiated `mime_type` into `fd`.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        let Some(data) = user_data.get(&mime_type) else {
            warn!(?ty, %mime_type, "client requested unoffered selection mime type");
            return;
        };
        let mut file = std::fs::File::from(fd);
        if let Err(err) = file.write_all(data) {
            warn!(?ty, %mime_type, "failed to write selection contents: {err}");
        }
    }
}

impl DataDeviceHandler for ScryState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }

    fn action_choice(&mut self, available: DndAction, preferred: DndAction) -> DndAction {
        default_action_chooser(available, preferred)
    }
}

// Drag'n'drop is not part of M1: the grab hooks carry default behavior.
impl ClientDndGrabHandler for ScryState {}
impl ServerDndGrabHandler for ScryState {}

impl PrimarySelectionHandler for ScryState {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

smithay::delegate_data_device!(ScryState);
smithay::delegate_primary_selection!(ScryState);
