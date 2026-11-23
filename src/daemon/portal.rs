//! The in-process scripted XDG Desktop Portal stub.
//!
//! Every workspace serves `org.freedesktop.portal.Desktop` at
//! `/org/freedesktop/portal/desktop` on its private session bus,
//! implementing `org.freedesktop.portal.FileChooser` (`OpenFile`,
//! `SaveFile`, `SaveFiles`) and `org.freedesktop.portal.OpenURI`
//! (`OpenURI`, `OpenFile`, `OpenDirectory`, `SchemeSupported`) under the
//! standard `handle_token` + `org.freedesktop.portal.Request::Response`
//! contract.
//!
//! Control verbs shape the stub's answers: `portal_script_set` queues
//! FIFO scripted responses per interface, `portal_script_clear` drains
//! them, and `portal_history` returns the bounded invocation log (method,
//! caller, options/URI, response). An unscripted call always resolves
//! immediately with a clean cancel (`response = 1`) so a headless app
//! can never deadlock on a human dialog.

use std::{
    collections::{HashMap, VecDeque},
    os::fd::AsRawFd,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use serde_json::{Map as JsonMap, Value as Json, json};
use tracing::{debug, warn};
use zbus::{
    Connection, fdo, interface,
    message::Header,
    names::{BusName, UniqueName},
    zvariant::{self, ObjectPath, OwnedFd, OwnedObjectPath},
};

use crate::daemon::{bus::BusManager, now_ms, protocol::ErrorResponse};

/// The portal well-known bus name.
const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
/// The portal object path.
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
/// Request handles live under this prefix:
/// `<prefix>/<sender-slug>/<handle-token>`.
const REQUEST_PREFIX: &str = "/org/freedesktop/portal/desktop/request";
/// The `org.freedesktop.portal.Request` interface name.
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
/// The FileChooser interface name.
const FILECHOOSER_IFACE: &str = "org.freedesktop.portal.FileChooser";
/// The OpenURI interface name.
const OPENURI_IFACE: &str = "org.freedesktop.portal.OpenURI";

/// Maximum recorded portal invocations retained per workspace.
const HISTORY_CAP: usize = 256;
/// How many recent invocations `status` embeds.
const REPORT_RECENT: usize = 3;

/// Portal response code: the interaction succeeded.
pub const RESPONSE_ACCEPT: u32 = 0;
/// Portal response code: the user cancelled — also the deterministic
/// answer to every unscripted call.
pub const RESPONSE_CANCEL: u32 = 1;
/// Portal response code: the user ended the interaction another way.
pub const RESPONSE_CLOSE: u32 = 2;

/// A portal interface callers can queue scripts for — the FIFO queue key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PortalInterface {
    /// `org.freedesktop.portal.FileChooser`.
    FileChooser,
    /// `org.freedesktop.portal.OpenURI`.
    OpenUri,
}

impl PortalInterface {
    /// The canonical short name reported back to callers.
    fn label(self) -> &'static str {
        match self {
            Self::FileChooser => "filechooser",
            Self::OpenUri => "openuri",
        }
    }

    /// The D-Bus interface name, for history records.
    fn dbus_name(self) -> &'static str {
        match self {
            Self::FileChooser => FILECHOOSER_IFACE,
            Self::OpenUri => OPENURI_IFACE,
        }
    }
}

/// Parses the spellings of a scriptable portal interface: the short
/// names (`filechooser`, `openuri`), the D-Bus suffixes (`FileChooser`,
/// `OpenURI`) or the full interface names — case-insensitive.
fn parse_interface(raw: &str) -> Option<PortalInterface> {
    let leaf = raw.rsplit('.').next().unwrap_or(raw);
    match leaf.to_ascii_lowercase().as_str() {
        "filechooser" | "file_chooser" => Some(PortalInterface::FileChooser),
        "openuri" | "open_uri" => Some(PortalInterface::OpenUri),
        _ => None,
    }
}

/// One queued scripted response (`portal_script_set`).
#[derive(Debug)]
struct Scripted {
    /// The `Response` code to emit (0/1/2).
    response: u32,
    /// The `uris` (`as`) result for accepts.
    uris: Vec<String>,
}

/// One recorded portal invocation (`portal_history`).
#[derive(Debug, Serialize)]
struct HistoryEntry {
    /// Wall-clock time the call arrived (epoch ms).
    at_ms: u64,
    /// The D-Bus interface the method was invoked on.
    interface: String,
    /// The invoked method name (`OpenFile`, `OpenURI`, …).
    method: String,
    /// The caller's unique bus name (`:1.42`).
    caller: String,
    /// The caller's Unix pid, when the broker reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_pid: Option<u32>,
    /// The request object path returned to the caller.
    handle: String,
    /// The method's non-option arguments (title, URI, fd, …).
    arguments: Json,
    /// The `a{sv}` options dict, verbatim.
    options: Json,
    /// The response code the stub emitted.
    response: u32,
    /// Whether the response came from a queued script.
    scripted: bool,
}

/// The scripting + history state shared between the served D-Bus
/// interfaces and the control verbs.
#[derive(Default)]
struct Shared {
    /// FIFO scripted responses, keyed by scriptable interface.
    scripts: HashMap<PortalInterface, VecDeque<Scripted>>,
    /// Bounded invocation log, oldest first.
    history: VecDeque<HistoryEntry>,
    /// Counter for generated handle tokens (`scry_<n>`).
    next_token: u64,
}

/// Why the stub is not serving (no session bus, or startup failure).
#[derive(Debug)]
enum ServiceState {
    /// Serving `org.freedesktop.portal.Desktop` on the session bus.
    Running,
    /// The session bus never came up; nothing to serve on.
    Unavailable(String),
    /// Connecting/registering/claiming the name failed.
    Failed(String),
}

/// One workspace's portal stub: the served D-Bus connection plus the
/// scripting/history state the control verbs act on.
pub struct PortalManager {
    /// Scripts and history, shared with the served interfaces.
    shared: Arc<Mutex<Shared>>,
    /// The live bus connection; `Some` while serving. Dropped on stop so
    /// the object server and name claim unwind before the broker dies.
    conn: Mutex<Option<Connection>>,
    /// Startup/health state for `status`.
    state: Mutex<ServiceState>,
}

impl PortalManager {
    /// Starts the stub on the workspace's session bus. Never fails the
    /// workspace — a missing bus or a failed name claim lands in
    /// [`Self::report`] instead (the verbs still work on the state).
    pub async fn start(buses: &BusManager) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (conn, state) = match buses.spawn_env().session_address {
            Some(address) => match serve(&address, shared.clone()).await {
                Ok(conn) => {
                    debug!("portal stub serving {PORTAL_NAME}");
                    (Some(conn), ServiceState::Running)
                }
                Err(err) => {
                    warn!("portal stub failed to start: {err}");
                    (None, ServiceState::Failed(err.to_string()))
                }
            },
            None => (
                None,
                ServiceState::Unavailable("session bus is not running".to_string()),
            ),
        };
        Self {
            shared,
            conn: Mutex::new(conn),
            state: Mutex::new(state),
        }
    }

    /// Queues one scripted response for `interface`
    /// (`portal_script_set`); returns the interface's queue depth.
    pub fn script_set(
        &self,
        interface: &str,
        response: u32,
        uris: Vec<String>,
    ) -> Result<usize, ErrorResponse> {
        let Some(iface) = parse_interface(interface) else {
            return Err(ErrorResponse::invalid_params_data(
                format!(
                    "unknown portal interface {interface:?}: \
                     use \"filechooser\" or \"openuri\""
                ),
                json!({"interface": interface}),
            ));
        };
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let queue = shared.scripts.entry(iface).or_default();
        queue.push_back(Scripted { response, uris });
        Ok(queue.len())
    }

    /// Drains queued scripts — all interfaces, or just `interface` when
    /// given (`portal_script_clear`); returns how many were dropped.
    pub fn script_clear(&self, interface: Option<&str>) -> Result<usize, ErrorResponse> {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        match interface {
            Some(raw) => {
                let Some(iface) = parse_interface(raw) else {
                    return Err(ErrorResponse::invalid_params_data(
                        format!(
                            "unknown portal interface {raw:?}: \
                             use \"filechooser\" or \"openuri\""
                        ),
                        json!({"interface": raw}),
                    ));
                };
                Ok(shared
                    .scripts
                    .get_mut(&iface)
                    .map(|queue| {
                        let n = queue.len();
                        queue.clear();
                        n
                    })
                    .unwrap_or(0))
            }
            None => Ok(shared
                .scripts
                .values_mut()
                .map(|queue| {
                    let n = queue.len();
                    queue.clear();
                    n
                })
                .sum()),
        }
    }

    /// The recorded invocations, oldest first (`portal_history`).
    /// `limit` keeps only the newest N.
    pub fn history(&self, limit: Option<usize>) -> Json {
        let shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let all: Vec<&HistoryEntry> = shared.history.iter().collect();
        let entries = match limit {
            Some(n) => &all[all.len().saturating_sub(n)..],
            None => &all[..],
        };
        json!({
            "count": entries.len(),
            "total": all.len(),
            "invocations": entries,
            "queued_scripts": queued_json(&shared),
        })
    }

    /// The `portal` section of `workspace_list`/`status`: stub health
    /// plus recent activity.
    pub fn report(&self) -> Json {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut report = json!({
            "state": match &*state {
                ServiceState::Running => "running",
                ServiceState::Unavailable(_) => "unavailable",
                ServiceState::Failed(_) => "failed",
            },
        });
        match &*state {
            ServiceState::Unavailable(reason) | ServiceState::Failed(reason) => {
                report["error"] = Json::from(reason.clone());
            }
            ServiceState::Running => {}
        }
        drop(state);
        let shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let recent: Vec<&HistoryEntry> = shared
            .history
            .iter()
            .rev()
            .take(REPORT_RECENT)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        report["calls"] = Json::from(shared.history.len());
        report["scripted"] = Json::from(shared.history.iter().filter(|e| e.scripted).count());
        report["queued_scripts"] = queued_json(&shared);
        report["recent"] = json!(recent);
        report
    }

    /// Drops the bus connection so the object server stops and the name
    /// is released while the broker is still alive (workspace teardown).
    pub fn stop(&self) {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(conn);
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) =
            ServiceState::Unavailable("workspace closed".to_string());
    }
}

/// The per-interface queued-script counts of a [`Shared`].
fn queued_json(shared: &Shared) -> Json {
    let mut map = JsonMap::new();
    for iface in [PortalInterface::FileChooser, PortalInterface::OpenUri] {
        map.insert(
            iface.label().to_string(),
            Json::from(shared.scripts.get(&iface).map(VecDeque::len).unwrap_or(0)),
        );
    }
    Json::Object(map)
}

/// Connects to `address`, serves the portal interfaces and claims the
/// well-known name.
async fn serve(address: &str, shared: Arc<Mutex<Shared>>) -> zbus::Result<Connection> {
    let conn = zbus::connection::Builder::address(address)?.build().await?;
    conn.object_server()
        .at(
            PORTAL_PATH,
            FileChooser {
                shared: shared.clone(),
            },
        )
        .await?;
    conn.object_server()
        .at(PORTAL_PATH, OpenUri { shared })
        .await?;
    conn.request_name(PORTAL_NAME).await?;
    Ok(conn)
}

/// The `org.freedesktop.portal.FileChooser` implementation.
struct FileChooser {
    shared: Arc<Mutex<Shared>>,
}

#[interface(name = "org.freedesktop.portal.FileChooser")]
impl FileChooser {
    /// `OpenFile(s parent_window, s title, a{sv} options) → o`.
    async fn open_file(
        &self,
        parent_window: String,
        title: String,
        options: HashMap<String, zvariant::Value<'_>>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.respond(
            conn,
            &header,
            "OpenFile",
            options,
            json!({"parent_window": parent_window, "title": title}),
        )
        .await
    }

    /// `SaveFile(s parent_window, s title, a{sv} options) → o`.
    async fn save_file(
        &self,
        parent_window: String,
        title: String,
        options: HashMap<String, zvariant::Value<'_>>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.respond(
            conn,
            &header,
            "SaveFile",
            options,
            json!({"parent_window": parent_window, "title": title}),
        )
        .await
    }

    /// `SaveFiles(s parent_window, s title, a{sv} options) → o`.
    async fn save_files(
        &self,
        parent_window: String,
        title: String,
        options: HashMap<String, zvariant::Value<'_>>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.respond(
            conn,
            &header,
            "SaveFiles",
            options,
            json!({"parent_window": parent_window, "title": title}),
        )
        .await
    }

    /// Interface version (v4 adds the SaveFiles/choices surface).
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }
}

impl FileChooser {
    /// Shared dispatch for the FileChooser methods.
    async fn respond(
        &self,
        conn: &Connection,
        header: &Header<'_>,
        method: &str,
        options: HashMap<String, zvariant::Value<'_>>,
        arguments: Json,
    ) -> fdo::Result<OwnedObjectPath> {
        handle_request(
            conn,
            &self.shared,
            header,
            PortalInterface::FileChooser,
            method,
            options,
            arguments,
        )
        .await
    }
}

/// The `org.freedesktop.portal.OpenURI` implementation.
struct OpenUri {
    shared: Arc<Mutex<Shared>>,
}

#[interface(name = "org.freedesktop.portal.OpenURI")]
impl OpenUri {
    /// `OpenURI(s parent_window, s uri, a{sv} options) → o`.
    #[zbus(name = "OpenURI")]
    async fn open_uri(
        &self,
        parent_window: String,
        uri: String,
        options: HashMap<String, zvariant::Value<'_>>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.respond(
            conn,
            &header,
            "OpenURI",
            options,
            json!({"parent_window": parent_window, "uri": uri}),
        )
        .await
    }

    /// `OpenFile(s parent_window, h fd, a{sv} options) → o`.
    async fn open_file(
        &self,
        parent_window: String,
        fd: OwnedFd,
        options: HashMap<String, zvariant::Value<'_>>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.respond(
            conn,
            &header,
            "OpenFile",
            options,
            json!({"parent_window": parent_window, "fd": fd.as_raw_fd()}),
        )
        .await
    }

    /// `OpenDirectory(s parent_window, h fd, a{sv} options) → o`.
    async fn open_directory(
        &self,
        parent_window: String,
        fd: OwnedFd,
        options: HashMap<String, zvariant::Value<'_>>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.respond(
            conn,
            &header,
            "OpenDirectory",
            options,
            json!({"parent_window": parent_window, "fd": fd.as_raw_fd()}),
        )
        .await
    }

    /// `SchemeSupported(s scheme, a{sv} options) → b`. The stub answers
    /// every URI — scripted, or deterministically cancelled — so every
    /// scheme counts as supported.
    async fn scheme_supported(
        &self,
        scheme: String,
        options: HashMap<String, zvariant::Value<'_>>,
    ) -> fdo::Result<bool> {
        let _ = (scheme, options);
        Ok(true)
    }

    /// Interface version.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        5
    }
}

impl OpenUri {
    /// Shared dispatch for the OpenURI request methods.
    async fn respond(
        &self,
        conn: &Connection,
        header: &Header<'_>,
        method: &str,
        options: HashMap<String, zvariant::Value<'_>>,
        arguments: Json,
    ) -> fdo::Result<OwnedObjectPath> {
        handle_request(
            conn,
            &self.shared,
            header,
            PortalInterface::OpenUri,
            method,
            options,
            arguments,
        )
        .await
    }
}

/// A served `org.freedesktop.portal.Request` object: `Close()` is a
/// no-op because scripted responses resolve immediately.
struct PortalRequest;

#[interface(name = "org.freedesktop.portal.Request")]
impl PortalRequest {
    /// `Close()`: real portals cancel the pending interaction; our
    /// responses are already on the wire, so there is nothing to close.
    async fn close(&self) {}
}

/// The shared core of every portal request method: compute the request
/// path from the caller's `handle_token`, resolve the response (queued
/// script or immediate cancel), serve `Close` on the path, record the
/// invocation, emit `Request::Response`, and return the handle.
async fn handle_request(
    conn: &Connection,
    shared: &Arc<Mutex<Shared>>,
    header: &Header<'_>,
    queue: PortalInterface,
    method: &str,
    options: HashMap<String, zvariant::Value<'_>>,
    arguments: Json,
) -> fdo::Result<OwnedObjectPath> {
    let sender = header
        .sender()
        .map(UniqueName::as_str)
        .unwrap_or_default()
        .to_string();
    // The request path mirrors xdg-desktop-portal:
    // `…/request/<unique name without ':', '.'→'_'>/<token>`.
    let sender_slug = sender.trim_start_matches(':').replace('.', "_");
    let (token, scripted) = {
        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
        let token = match options.get("handle_token").and_then(value_as_str) {
            Some(raw) if !raw.is_empty() => sanitize_token(raw),
            _ => {
                shared.next_token += 1;
                format!("scry_{}", shared.next_token)
            }
        };
        let scripted = shared.scripts.get_mut(&queue).and_then(VecDeque::pop_front);
        (token, scripted)
    };
    let request_path = format!("{REQUEST_PREFIX}/{sender_slug}/{token}");
    let (response, results) = match &scripted {
        Some(script) => {
            // `uris` is an accept-only result; cancels and closes carry
            // an empty dict like the real portal.
            let mut results = HashMap::new();
            if script.response == RESPONSE_ACCEPT && !script.uris.is_empty() {
                results.insert(
                    "uris".to_string(),
                    zvariant::Value::from(script.uris.clone()),
                );
            }
            (script.response, results)
        }
        None => (RESPONSE_CANCEL, HashMap::new()),
    };
    let caller_pid = caller_pid(conn, &sender).await;
    {
        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
        if shared.history.len() >= HISTORY_CAP {
            shared.history.pop_front();
        }
        shared.history.push_back(HistoryEntry {
            at_ms: now_ms(),
            interface: queue.dbus_name().to_string(),
            method: method.to_string(),
            caller: sender,
            caller_pid,
            handle: request_path.clone(),
            arguments,
            options: options_json(&options),
            response,
            scripted: scripted.is_some(),
        });
    }
    // Serve `Request::Close` on the handle before responding so a caller
    // reaching for it never hits a missing object.
    let _ = conn
        .object_server()
        .at(request_path.clone(), PortalRequest)
        .await;
    conn.emit_signal(
        None::<BusName>,
        request_path.as_str(),
        REQUEST_IFACE,
        "Response",
        &(response, results),
    )
    .await
    .map_err(|err| fdo::Error::Failed(format!("failed to emit Response: {err}")))?;
    ObjectPath::try_from(request_path)
        .map(Into::into)
        .map_err(|err| fdo::Error::Failed(format!("invalid request path: {err}")))
}

/// `GetConnectionUnixProcessID` for `sender`, when the broker answers.
async fn caller_pid(conn: &Connection, sender: &str) -> Option<u32> {
    let proxy = fdo::DBusProxy::new(conn).await.ok()?;
    let name = BusName::try_from(sender).ok()?;
    proxy.get_connection_unix_process_id(name).await.ok()
}

/// A `Value` that is a D-Bus string, as `&str`.
fn value_as_str<'a>(value: &'a zvariant::Value<'a>) -> Option<&'a str> {
    match value {
        zvariant::Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// A `handle_token` may only contain object-path-element characters;
/// anything else is folded to `_`.
fn sanitize_token(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The `a{sv}` options dict as JSON (verbatim, for `portal_history`).
fn options_json(options: &HashMap<String, zvariant::Value<'_>>) -> Json {
    Json::Object(
        options
            .iter()
            .map(|(k, v)| (k.clone(), variant_to_json(v)))
            .collect(),
    )
}

/// A `zvariant` value as JSON, best-effort for diagnostics.
fn variant_to_json(value: &zvariant::Value<'_>) -> Json {
    use zvariant::Value as V;
    match value {
        V::U8(v) => Json::from(*v),
        V::Bool(v) => Json::from(*v),
        V::I16(v) => Json::from(*v),
        V::U16(v) => Json::from(*v),
        V::I32(v) => Json::from(*v),
        V::U32(v) => Json::from(*v),
        V::I64(v) => Json::from(*v),
        V::U64(v) => Json::from(*v),
        V::F64(v) => serde_json::Number::from_f64(*v)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        V::Str(v) => Json::from(v.as_str()),
        V::Signature(v) => Json::from(v.to_string()),
        V::ObjectPath(v) => Json::from(v.as_str()),
        V::Value(inner) => variant_to_json(inner),
        V::Array(array) => Json::Array(array.inner().iter().map(variant_to_json).collect()),
        V::Dict(dict) => Json::Object(
            dict.iter()
                .map(|(k, v)| (variant_key(k), variant_to_json(v)))
                .collect(),
        ),
        V::Structure(structure) => {
            Json::Array(structure.fields().iter().map(variant_to_json).collect())
        }
        V::Fd(fd) => Json::from(fd.as_raw_fd()),
    }
}

/// A dict key as a JSON object key: strings verbatim, other scalar
/// types stringified.
fn variant_key(key: &zvariant::Value<'_>) -> String {
    match variant_to_json(key) {
        Json::String(s) => s,
        other => other.to_string(),
    }
}
