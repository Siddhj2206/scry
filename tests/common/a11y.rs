//! A controllable AT-SPI2 test peer for `get_tree` tests: serves a
//! synthetic accessible tree on a workspace's real private a11y bus over
//! `zbus` and registers it with the real `at-spi2-registryd` through the
//! standard `Socket::Embed` handshake — the same registration real GTK/Qt
//! apps perform.
//!
//! Two ways to use it:
//!
//! - `serve()` inside the test process: the test binary's own pid is both
//!   the Wayland client (`TestClient`) and the a11y peer, so
//!   `SO_PEERCRED` ↔ `GetConnectionUnixProcessID` correlation works
//!   in-process.
//! - `run_helper()` re-execs the test binary as a spawned child (distinct
//!   pid) for multi-PID correlation tests: it maps a Wayland toplevel and
//!   serves a tree, driven entirely by the `SCRY_A11Y_HELPER` env spec.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::TestClient;

/// The registry's well-known name and its desktop root, mirroring
/// `src/daemon/a11y.rs` — the objects `Embed` and `GetChildren` speak to.
const REGISTRY_NAME: &str = "org.a11y.atspi.Registry";
const REGISTRY_ROOT: &str = "/org/a11y/atspi/accessible/root";
/// The AT-SPI null-object sentinel.
const NULL_PATH: &str = "/org/a11y/atspi/null";

/// The accessible role of an app's root object.
const ROLE_APPLICATION: u32 = 75;
/// How long [`serve`] waits for `org.a11y.atspi.Registry` to appear.
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(10);

/// A role name ("frame", "push button") → its AT-SPI wire value.
pub fn role_value(name: &str) -> u32 {
    for value in 0..=255u32 {
        if let Ok(role) = atspi::Role::try_from(value)
            && role.name() == name
        {
            return value;
        }
    }
    panic!("unknown AT-SPI role name {name:?}");
}

/// An AT-SPI state name list → the two `u32` bitfield words `GetState`
/// returns on the wire.
pub fn state_words(names: &[String]) -> Vec<u32> {
    let set: atspi::StateSet = names
        .iter()
        .map(|n| atspi::State::from(n.as_str()))
        .collect();
    let bits = set.bits();
    vec![bits as u32, (bits >> 32) as u32]
}

/// One node in a synthetic accessibility tree.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FakeNode {
    /// The node's accessible name.
    pub name: String,
    /// Its role name (`"frame"`, `"push button"`, `"application"`, ...).
    pub role: String,
    /// `Component::GetExtents` answer in the fake toolkit's screen space
    /// (typically window-relative: the daemon fuses it onto the window's
    /// compositor geometry). `None` = the node exposes no `Component`.
    #[serde(default)]
    pub bounds: Option<[i32; 4]>,
    /// AT-SPI state names to report (`"enabled"`, `"showing"`, ...).
    #[serde(default)]
    pub states: Vec<String>,
    /// `Text` interface contents — serves `org.a11y.atspi.Text` when set.
    #[serde(default)]
    pub text: Option<String>,
    /// `Value` interface contents `(current, text)` — serves
    /// `org.a11y.atspi.Value` when set.
    #[serde(default)]
    pub value: Option<f64>,
    /// `Value::Text` answer, when `value` is set.
    #[serde(default)]
    pub value_text: Option<String>,
    /// Milliseconds each `GetChildren` sleeps before answering — a
    /// deliberately slow peer for deadline/cancellation tests.
    #[serde(default)]
    pub delay_ms: u64,
    /// Child nodes.
    #[serde(default)]
    pub children: Vec<FakeNode>,
}

impl FakeNode {
    /// A leaf-less node builder.
    pub fn new(name: &str, role: &str) -> Self {
        Self {
            name: name.to_string(),
            role: role.to_string(),
            bounds: None,
            states: vec!["enabled".into(), "showing".into(), "visible".into()],
            text: None,
            value: None,
            value_text: None,
            delay_ms: 0,
            children: Vec::new(),
        }
    }

    /// Sets `Component` bounds.
    pub fn bounds(mut self, bounds: [i32; 4]) -> Self {
        self.bounds = Some(bounds);
        self
    }

    /// Sets states.
    pub fn states(mut self, states: &[&str]) -> Self {
        self.states = states.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Gives the node a `Text` interface holding `text`.
    pub fn text(mut self, text: &str) -> Self {
        self.text = Some(text.to_string());
        self
    }

    /// Gives the node a `Value` interface reading `value`.
    pub fn value(mut self, value: f64) -> Self {
        self.value = Some(value);
        self
    }

    /// Gives the node's `Value` interface a textual form.
    pub fn value_text(mut self, text: &str) -> Self {
        self.value_text = Some(text.to_string());
        self
    }

    /// Makes every `GetChildren` call on this node sleep `ms` first.
    pub fn delay_ms(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }

    /// Appends one child.
    pub fn child(mut self, child: FakeNode) -> Self {
        self.children.push(child);
        self
    }
}

/// A registered fake peer: keeps the a11y-bus connection (and therefore
/// the registration) alive until dropped.
pub struct FakePeer {
    /// The peer's unique name on the workspace a11y bus.
    pub bus_name: String,
    /// The served application root's object path.
    pub root_path: String,
    conn: zbus::blocking::Connection,
}

/// Waits for `org.a11y.atspi.Registry` to own a name on `conn`'s bus.
fn wait_for_registry(conn: &zbus::blocking::Connection) {
    let deadline = Instant::now() + REGISTRY_TIMEOUT;
    loop {
        let reply = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "ListNames",
                &(),
            )
            .expect("ListNames failed");
        let names: Vec<String> = reply.body().deserialize().unwrap();
        if names.iter().any(|n| n == REGISTRY_NAME) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {REGISTRY_NAME} on the a11y bus"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Serves `root`'s tree on the a11y bus at `addr` and registers it with
/// the workspace registry daemon via `Socket::Embed`.
pub fn serve(addr: &str, root: &FakeNode) -> FakePeer {
    let conn = zbus::blocking::connection::Builder::address(addr)
        .unwrap_or_else(|e| panic!("bad a11y address {addr:?}: {e}"))
        .build()
        .unwrap_or_else(|e| panic!("failed to connect to a11y bus {addr:?}: {e}"));
    let unique = conn.unique_name().unwrap().to_string();
    wait_for_registry(&conn);

    // Flatten the tree: every node gets a stable object path and knows its
    // parent's. Children always carry the peer's unique bus name in their
    // ObjectRefs, like real toolkits.
    let root_path = "/fake/a11y/app".to_string();
    let mut flat: Vec<(String, &FakeNode, Option<String>)> = vec![(root_path.clone(), root, None)];
    let mut index = 0;
    while index < flat.len() {
        let (parent_path, node, _) = flat[index].clone();
        for (child_index, child) in node.children.iter().enumerate() {
            flat.push((
                format!("{parent_path}/{child_index}"),
                child,
                Some(parent_path.clone()),
            ));
        }
        index += 1;
    }

    for (path, node, parent) in &flat {
        let children: Vec<(String, OwnedObjectPath)> = node
            .children
            .iter()
            .enumerate()
            .map(|(i, _)| {
                (
                    unique.clone(),
                    OwnedObjectPath::try_from(format!("{path}/{i}")).unwrap(),
                )
            })
            .collect();
        let parent_ref = match parent {
            Some(parent) => (
                unique.clone(),
                OwnedObjectPath::try_from(parent.as_str()).unwrap(),
            ),
            None => (String::new(), OwnedObjectPath::try_from(NULL_PATH).unwrap()),
        };
        let mut interfaces = vec!["org.a11y.atspi.Accessible".to_string()];
        if node.bounds.is_some() {
            interfaces.push("org.a11y.atspi.Component".to_string());
        }
        if node.text.is_some() {
            interfaces.push("org.a11y.atspi.Text".to_string());
        }
        if node.value.is_some() {
            interfaces.push("org.a11y.atspi.Value".to_string());
        }
        let role = role_value(&node.role);
        conn.object_server()
            .at(
                path.as_str(),
                FakeAccessible {
                    unique: unique.clone(),
                    name: node.name.clone(),
                    role,
                    role_name: node.role.clone(),
                    states: state_words(&node.states),
                    parent: parent_ref,
                    app: (
                        unique.clone(),
                        OwnedObjectPath::try_from(root_path.as_str()).unwrap(),
                    ),
                    children,
                    child_count: node.children.len() as i32,
                    interfaces,
                    delay: Duration::from_millis(node.delay_ms),
                },
            )
            .unwrap_or_else(|e| panic!("failed to serve {path}: {e}"));
        if let Some(bounds) = node.bounds {
            conn.object_server()
                .at(path.as_str(), FakeComponent { bounds })
                .unwrap_or_else(|e| panic!("failed to serve {path} component: {e}"));
        }
        if let Some(text) = &node.text {
            conn.object_server()
                .at(path.as_str(), FakeText { text: text.clone() })
                .unwrap_or_else(|e| panic!("failed to serve {path} text: {e}"));
        }
        if let Some(value) = node.value {
            conn.object_server()
                .at(
                    path.as_str(),
                    FakeValue {
                        current: value,
                        text: node.value_text.clone().unwrap_or_default(),
                    },
                )
                .unwrap_or_else(|e| panic!("failed to serve {path} value: {e}"));
        }
    }

    // The app root also implements `org.a11y.atspi.Application` — the
    // registry sets `Id` on it during the Embed handshake.
    conn.object_server()
        .at(root_path.as_str(), FakeApplication::default())
        .expect("failed to serve application interface");

    let plug_path = ObjectPath::try_from(root_path.as_str()).unwrap();
    let reply = conn
        .call_method(
            Some(REGISTRY_NAME),
            REGISTRY_ROOT,
            Some("org.a11y.atspi.Socket"),
            "Embed",
            &((unique.as_str(), &plug_path),),
        )
        .expect("Socket::Embed failed");
    let _socket: (String, OwnedObjectPath) = reply.body().deserialize().unwrap();

    FakePeer {
        bus_name: unique,
        root_path,
        conn,
    }
}

/// The env-var spec a spawned helper process reads.
#[derive(Debug, Deserialize, Serialize)]
pub struct HelperSpec {
    /// `xdg_toplevel.app_id` for the helper's window.
    pub app_id: String,
    /// `xdg_toplevel.title` for the helper's window.
    pub title: String,
    /// Toplevel size in logical pixels.
    pub width: i32,
    /// Toplevel size in logical pixels.
    pub height: i32,
    /// File the helper writes once its window + a11y tree are live.
    pub ready: PathBuf,
    /// The accessibility tree to serve.
    pub tree: FakeNode,
}

/// Body of the spawned fake-app helper: reads `SCRY_A11Y_HELPER`, maps a
/// Wayland toplevel on `WAYLAND_DISPLAY`, serves the a11y tree on
/// `AT_SPI_BUS_ADDRESS`, then writes the ready file and parks forever
/// (workspace teardown's SIGTERM ends it).
pub fn run_helper() -> ! {
    let spec: HelperSpec = serde_json::from_str(
        &std::env::var("SCRY_A11Y_HELPER").expect("SCRY_A11Y_HELPER env is not set"),
    )
    .expect("SCRY_A11Y_HELPER is not valid JSON");
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set");
    let display = std::env::var("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY not set");
    let addr = std::env::var("AT_SPI_BUS_ADDRESS").expect("AT_SPI_BUS_ADDRESS not set");

    let mut client = TestClient::connect(&Path::new(&runtime_dir).join(&display));
    let _toplevel = client.map_toplevel(spec.width, spec.height, &spec.app_id, &spec.title);
    let _peer = serve(&addr, &spec.tree);
    std::fs::write(&spec.ready, "ready").expect("failed to write ready file");
    loop {
        std::thread::park();
    }
}

/// `org.a11y.atspi.Accessible` implementation for one fake node.
struct FakeAccessible {
    unique: String,
    name: String,
    role: u32,
    role_name: String,
    states: Vec<u32>,
    parent: (String, OwnedObjectPath),
    app: (String, OwnedObjectPath),
    children: Vec<(String, OwnedObjectPath)>,
    child_count: i32,
    interfaces: Vec<String>,
    /// How long `GetChildren` sleeps before answering (slow-peer tests).
    delay: Duration,
}

#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl FakeAccessible {
    async fn get_children(&self) -> Vec<(String, OwnedObjectPath)> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.children.clone()
    }

    async fn get_child_at_index(&self, index: i32) -> (String, OwnedObjectPath) {
        self.children
            .get(index as usize)
            .cloned()
            .unwrap_or_else(|| (String::new(), OwnedObjectPath::try_from(NULL_PATH).unwrap()))
    }

    async fn get_index_in_parent(&self) -> i32 {
        0
    }

    async fn get_role(&self) -> u32 {
        self.role
    }

    async fn get_role_name(&self) -> String {
        self.role_name.clone()
    }

    async fn get_localized_role_name(&self) -> String {
        self.role_name.clone()
    }

    async fn get_state(&self) -> Vec<u32> {
        self.states.clone()
    }

    async fn get_interfaces(&self) -> Vec<String> {
        self.interfaces.clone()
    }

    async fn get_attributes(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    async fn get_relation_set(&self) -> Vec<(u32, Vec<(String, OwnedObjectPath)>)> {
        Vec::new()
    }

    async fn get_application(&self) -> (String, OwnedObjectPath) {
        self.app.clone()
    }

    #[zbus(property)]
    async fn name(&self) -> String {
        self.name.clone()
    }

    #[zbus(property)]
    async fn description(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    async fn parent(&self) -> (String, OwnedObjectPath) {
        self.parent.clone()
    }

    #[zbus(property)]
    async fn child_count(&self) -> i32 {
        self.child_count
    }

    #[zbus(property)]
    async fn accessible_id(&self) -> String {
        self.name.clone()
    }
}

/// `org.a11y.atspi.Component` implementation for bounded fake nodes.
struct FakeComponent {
    bounds: [i32; 4],
}

#[zbus::interface(name = "org.a11y.atspi.Component")]
impl FakeComponent {
    async fn contains(&self, x: i32, y: i32, _coord_type: u32) -> bool {
        let [bx, by, w, h] = self.bounds;
        x >= bx && x < bx + w && y >= by && y < by + h
    }

    async fn get_accessible_at_point(
        &self,
        _x: i32,
        _y: i32,
        _coord_type: u32,
    ) -> (String, OwnedObjectPath) {
        (String::new(), OwnedObjectPath::try_from(NULL_PATH).unwrap())
    }

    async fn get_extents(&self, _coord_type: u32) -> (i32, i32, i32, i32) {
        self.bounds.into()
    }

    async fn get_position(&self, _coord_type: u32) -> (i32, i32) {
        (self.bounds[0], self.bounds[1])
    }

    async fn get_size(&self) -> (i32, i32) {
        (self.bounds[2], self.bounds[3])
    }

    async fn get_layer(&self) -> u32 {
        3 // Layer::Widget
    }

    async fn get_alpha(&self) -> f64 {
        1.0
    }

    #[zbus(name = "GetMDIZOrder")]
    async fn get_mdi_z_order(&self) -> i16 {
        0
    }

    async fn grab_focus(&self) -> bool {
        true
    }

    async fn scroll_to(&self, _type_: u32) -> bool {
        false
    }

    async fn scroll_to_point(&self, _coord_type: u32, _x: i32, _y: i32) -> bool {
        false
    }

    async fn set_extents(
        &self,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
        _coord_type: u32,
    ) -> bool {
        false
    }

    async fn set_position(&self, _x: i32, _y: i32, _coord_type: u32) -> bool {
        false
    }

    async fn set_size(&self, _width: i32, _height: i32) -> bool {
        false
    }
}

/// `org.a11y.atspi.Text` implementation for nodes carrying `text`.
struct FakeText {
    text: String,
}

#[zbus::interface(name = "org.a11y.atspi.Text")]
impl FakeText {
    async fn get_text(&self, start_offset: i32, end_offset: i32) -> String {
        let start = start_offset.max(0) as usize;
        let end = (end_offset.max(0) as usize).min(self.text.chars().count());
        self.text
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect()
    }

    #[zbus(property)]
    async fn character_count(&self) -> i32 {
        self.text.chars().count() as i32
    }

    #[zbus(property)]
    async fn caret_offset(&self) -> i32 {
        0
    }
}

/// `org.a11y.atspi.Value` implementation for nodes carrying `value`.
struct FakeValue {
    current: f64,
    text: String,
}

#[zbus::interface(name = "org.a11y.atspi.Value")]
impl FakeValue {
    #[zbus(property)]
    async fn current_value(&self) -> f64 {
        self.current
    }

    #[zbus(property)]
    async fn minimum_value(&self) -> f64 {
        0.0
    }

    #[zbus(property)]
    async fn maximum_value(&self) -> f64 {
        100.0
    }

    #[zbus(property)]
    async fn minimum_increment(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn text(&self) -> String {
        self.text.clone()
    }
}

/// `org.a11y.atspi.Application` on the fake app root — the registry sets
/// `Id` on it during `Embed`.
struct FakeApplication {
    id: std::sync::atomic::AtomicI32,
}

impl Default for FakeApplication {
    fn default() -> Self {
        Self {
            id: std::sync::atomic::AtomicI32::new(0),
        }
    }
}

#[zbus::interface(name = "org.a11y.atspi.Application")]
impl FakeApplication {
    #[zbus(property)]
    async fn id(&self) -> i32 {
        self.id.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[zbus(property)]
    async fn set_id(&self, id: i32) {
        self.id.store(id, std::sync::atomic::Ordering::Relaxed);
    }

    #[zbus(property)]
    async fn atspi_version(&self) -> String {
        "2.0".to_string()
    }

    #[zbus(property)]
    async fn toolkit_name(&self) -> String {
        "scry-fake".to_string()
    }

    #[zbus(property)]
    async fn version(&self) -> String {
        "1".to_string()
    }

    async fn get_locale(&self, _lctype: u32) -> String {
        "en_US.UTF-8".to_string()
    }
}
