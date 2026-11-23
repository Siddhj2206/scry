//! `zwp_text_input_v3` instance bookkeeping.
//!
//! Smithay's own `Dispatch<ZwpTextInputV3>` discards every client request
//! unless a real `zwp_input_method_v2` client is connected. scry has no IME
//! client — the compositor itself plays that role — so we interpose our own
//! `Dispatch` implementation on [`ScryState`]: it forwards requests to
//! smithay's implementation (keeping its internal instance/serial bookkeeping
//! in sync) while tracking the two facts the IME-emulation path needs itself:
//!
//! - whether the instance is currently *enabled* (enable/disable apply on
//!   `commit`, per protocol);
//! - the instance's commit serial, which `zwp_text_input_v3.done` must echo
//!   back so clients can match the event to their `commit` request.

use std::collections::HashMap;

use smithay::{
    reexports::{
        wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{self, ZwpTextInputV3},
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, Resource,
            backend::{ClientId, ObjectId},
        },
    },
    wayland::text_input::{TextInputManagerState, TextInputUserData},
};

use crate::state::ScryState;

/// Per-instance tracked state.
#[derive(Debug, Default)]
struct InstanceState {
    /// `enable`/`disable` requests that have not been committed yet.
    pending_enable: Option<bool>,
    /// Whether the instance is enabled (applies on commit).
    enabled: bool,
    /// Number of `commit` requests sent on this instance — the serial a
    /// `done` event must echo.
    serial: u32,
}

/// Tracks the enable state and commit serial of each live text-input
/// instance, keyed by `ObjectId`.
#[derive(Debug, Default)]
pub(crate) struct TextInputTracker {
    instances: HashMap<ObjectId, InstanceState>,
}

impl TextInputTracker {
    fn on_request(&mut self, id: ObjectId, request: &zwp_text_input_v3::Request) {
        use zwp_text_input_v3::Request as R;
        if matches!(request, R::Destroy) {
            self.instances.remove(&id);
            return;
        }
        let instance = self.instances.entry(id).or_default();
        match request {
            R::Enable => instance.pending_enable = Some(true),
            R::Disable => instance.pending_enable = Some(false),
            R::Commit => {
                instance.serial += 1;
                if let Some(enable) = instance.pending_enable.take() {
                    instance.enabled = enable;
                }
            }
            _ => {}
        }
    }

    /// Whether `type_text` may inject a commit string through this instance.
    pub(crate) fn is_enabled(&self, id: &ObjectId) -> bool {
        self.instances.get(id).is_some_and(|i| i.enabled)
    }

    /// The commit serial to echo back through `done`.
    pub(crate) fn serial(&self, id: &ObjectId) -> u32 {
        self.instances.get(id).map_or(0, |i| i.serial)
    }
}

impl Dispatch<ZwpTextInputV3, TextInputUserData> for ScryState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ZwpTextInputV3,
        request: zwp_text_input_v3::Request,
        data: &TextInputUserData,
        dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, ScryState>,
    ) {
        state.text_input_tracker.on_request(resource.id(), &request);
        <TextInputManagerState as Dispatch<ZwpTextInputV3, TextInputUserData, ScryState>>::request(
            state, client, resource, request, data, dhandle, data_init,
        );
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwpTextInputV3,
        _data: &TextInputUserData,
    ) {
        state.text_input_tracker.instances.remove(&resource.id());
    }
}
