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
    sync::{Arc, Mutex},
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
    /// Action names the node offers on `org.a11y.atspi.Action`
    /// (`"click"`, `"press"`, ...); empty = no `Action` interface. The
    /// first entry is the node's conventional default action.
    #[serde(default)]
    pub actions: Vec<String>,
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
            actions: Vec::new(),
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

    /// Sets the `org.a11y.atspi.Action` names the node offers.
    pub fn actions(mut self, actions: &[&str]) -> Self {
        self.actions = actions.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// The live, mutable state behind one served node — the seam staleness
/// and `node_action` tests drive after `serve` returns.
struct NodeState {
    name: String,
    role: u32,
    role_name: String,
    states: Vec<u32>,
    bounds: Option<[i32; 4]>,
    /// `Text` interface contents (`None` = no `Text` interface served).
    text: Option<String>,
    /// `Value` interface reading (`None` = no `Value` interface served).
    value: Option<f64>,
    /// `Value::Text` answer.
    value_text: Option<String>,
    /// How long `GetChildren` sleeps before answering (slow-peer tests).
    delay: Duration,
    children: Vec<(String, OwnedObjectPath)>,
    /// This node's path inside its parent, so `remove_node` can prune it
    /// from the parent's `GetChildren`.
    parent_path: Option<String>,
    interfaces: Vec<String>,
    actions: Vec<atspi::Action>,
    /// `DoAction` invocations recorded, as action names.
    invoked: Vec<String>,
    /// Once set, every interface method fails like a vanished object.
    removed: bool,
    /// GTK4-on-Wayland behavior: `CoordType::Screen` extents report
    /// `(0, 0, w, h)` — clients cannot know their compositor position.
    degenerate_screen: bool,
}

impl NodeState {
    /// `fdo::Error::Failed` once the node is removed — what an AT-SPI
    /// client sees when the object's widget was destroyed.
    fn check_live(&self) -> zbus::fdo::Result<()> {
        if self.removed {
            return Err(zbus::fdo::Error::Failed(
                "the accessible object no longer exists".to_string(),
            ));
        }
        Ok(())
    }
}

/// All nodes of one served tree, keyed by object path.
type Nodes = Arc<Mutex<HashMap<String, Arc<Mutex<NodeState>>>>>;

/// A registered fake peer: keeps the a11y-bus connection (and therefore
/// the registration) alive until dropped.
pub struct FakePeer {
    /// The peer's unique name on the workspace a11y bus.
    pub bus_name: String,
    /// The served application root's object path.
    pub root_path: String,
    conn: zbus::blocking::Connection,
    nodes: Nodes,
    /// Monotonic suffix for paths of nodes added after `serve`.
    added: std::sync::atomic::AtomicUsize,
}

impl FakePeer {
    /// One node's mutable state by object path.
    fn node(&self, path: &str) -> Arc<Mutex<NodeState>> {
        self.nodes
            .lock()
            .unwrap()
            .get(path)
            .unwrap_or_else(|| panic!("no served node at {path}"))
            .clone()
    }

    /// Changes a served node's accessible name — drives the `STALE_REF`
    /// name-mismatch path in `click_node`/`node_action` tests.
    pub fn set_name(&self, path: &str, name: &str) {
        let node = self.node(path);
        node.lock().unwrap().name = name.to_string();
    }

    /// Changes a served node's role by role name (`"push button"`, …) —
    /// drives the `STALE_REF` role-mismatch path.
    pub fn set_role(&self, path: &str, role: &str) {
        let node = self.node(path);
        let mut node = node.lock().unwrap();
        node.role = role_value(role);
        node.role_name = role.to_string();
    }

    /// Removes a served node: its own methods start failing and its
    /// parent's `GetChildren` drops it — the `STALE_REF` disappearance
    /// path.
    pub fn remove_node(&self, path: &str) {
        let parent_path = {
            let node = self.node(path);
            let mut node = node.lock().unwrap();
            node.removed = true;
            node.parent_path.clone()
        };
        if let Some(parent_path) = parent_path {
            let parent = self.node(&parent_path);
            let mut parent = parent.lock().unwrap();
            parent.children.retain(|(_, child)| child.as_str() != path);
        }
    }

    /// The action names `DoAction` recorded on a node, in order.
    pub fn invoked_actions(&self, path: &str) -> Vec<String> {
        let node = self.node(path);
        node.lock().unwrap().invoked.clone()
    }

    /// Changes a served node's `Text` contents — drives `tree_diff`
    /// changed-line hunks.
    pub fn set_text(&self, path: &str, text: &str) {
        let node = self.node(path);
        node.lock().unwrap().text = Some(text.to_string());
    }

    /// Replaces a served node's AT-SPI state names — drives `tree_diff`
    /// hunks for pure state flips (checked, focused) that leave role,
    /// name and bounds alone.
    pub fn set_states(&self, path: &str, states: &[&str]) {
        let node = self.node(path);
        node.lock().unwrap().states =
            state_words(&states.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    }

    /// Serves `node` (and its subtree) as a new child of `parent_path`,
    /// inserted at `index` (clamped to the end). Returns the new node's
    /// object path — the `tree_diff` added-lines path.
    pub fn insert_child(&self, parent_path: &str, index: usize, node: FakeNode) -> String {
        let suffix = self
            .added
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = format!("{parent_path}/added{suffix}");
        serve_node_tree(
            &self.conn,
            &self.nodes,
            &self.bus_name,
            &self.root_path,
            &path,
            &node,
            Some(parent_path),
        );
        let parent = self.node(parent_path);
        let mut parent = parent.lock().unwrap();
        let index = index.min(parent.children.len());
        parent.children.insert(
            index,
            (
                self.bus_name.clone(),
                OwnedObjectPath::try_from(path.clone()).unwrap(),
            ),
        );
        path
    }

    /// `insert_child` at the end of the parent's child list.
    pub fn add_child(&self, parent_path: &str, node: FakeNode) -> String {
        self.insert_child(parent_path, usize::MAX, node)
    }

    /// Puts the peer in GTK4-on-Wayland mode: `CoordType::Screen`
    /// extents degenerate to `(0, 0, w, h)` — the real positions are
    /// only available under `CoordType::Window`/`Parent`.
    pub fn degenerate_screen(&self, path: &str) {
        let node = self.node(path);
        node.lock().unwrap().degenerate_screen = true;
    }
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

    let nodes: Nodes = Arc::new(Mutex::new(HashMap::new()));
    for (path, node, parent) in &flat {
        serve_one(
            &conn,
            &nodes,
            &unique,
            &root_path,
            path,
            node,
            parent.as_deref(),
        );
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
        nodes,
        added: std::sync::atomic::AtomicUsize::new(0),
    }
}

/// Serves `node` and its whole subtree on `conn`, registering every node
/// into `nodes` — shared by `serve`'s initial tree and `insert_child`'s
/// runtime additions.
fn serve_node_tree(
    conn: &zbus::blocking::Connection,
    nodes: &Nodes,
    unique: &str,
    root_path: &str,
    path: &str,
    node: &FakeNode,
    parent: Option<&str>,
) {
    serve_one(conn, nodes, unique, root_path, path, node, parent);
    for (index, child) in node.children.iter().enumerate() {
        serve_node_tree(
            conn,
            nodes,
            unique,
            root_path,
            &format!("{path}/{index}"),
            child,
            Some(path),
        );
    }
}

/// Registers one node's state and interfaces on `conn` and inserts it
/// into `nodes`. `parent` is the node's parent object path (`None` on
/// the application root).
fn serve_one(
    conn: &zbus::blocking::Connection,
    nodes: &Nodes,
    unique: &str,
    root_path: &str,
    path: &str,
    node: &FakeNode,
    parent: Option<&str>,
) {
    {
        let children: Vec<(String, OwnedObjectPath)> = node
            .children
            .iter()
            .enumerate()
            .map(|(i, _)| {
                (
                    unique.to_string(),
                    OwnedObjectPath::try_from(format!("{path}/{i}")).unwrap(),
                )
            })
            .collect();
        let parent_ref = match parent {
            Some(parent) => (
                unique.to_string(),
                OwnedObjectPath::try_from(parent).unwrap(),
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
        if !node.actions.is_empty() {
            interfaces.push("org.a11y.atspi.Action".to_string());
        }
        let state = Arc::new(Mutex::new(NodeState {
            name: node.name.clone(),
            role: role_value(&node.role),
            role_name: node.role.clone(),
            states: state_words(&node.states),
            bounds: node.bounds,
            text: node.text.clone(),
            value: node.value,
            value_text: node.value_text.clone(),
            delay: Duration::from_millis(node.delay_ms),
            children,
            parent_path: parent.map(str::to_string),
            interfaces,
            actions: node
                .actions
                .iter()
                .map(|name| atspi::Action {
                    name: name.clone(),
                    description: format!("fake {name}"),
                    keybinding: String::new(),
                })
                .collect(),
            invoked: Vec::new(),
            removed: false,
            degenerate_screen: false,
        }));
        nodes
            .lock()
            .unwrap()
            .insert(path.to_string(), state.clone());
        conn.object_server()
            .at(
                path,
                FakeAccessible {
                    unique: unique.to_string(),
                    parent: parent_ref,
                    app: (
                        unique.to_string(),
                        OwnedObjectPath::try_from(root_path).unwrap(),
                    ),
                    state: state.clone(),
                },
            )
            .unwrap_or_else(|e| panic!("failed to serve {path}: {e}"));
        if node.bounds.is_some() {
            conn.object_server()
                .at(
                    path,
                    FakeComponent {
                        state: state.clone(),
                    },
                )
                .unwrap_or_else(|e| panic!("failed to serve {path} component: {e}"));
        }
        if node.text.is_some() {
            conn.object_server()
                .at(
                    path,
                    FakeText {
                        state: state.clone(),
                    },
                )
                .unwrap_or_else(|e| panic!("failed to serve {path} text: {e}"));
        }
        if node.value.is_some() {
            conn.object_server()
                .at(
                    path,
                    FakeValue {
                        state: state.clone(),
                    },
                )
                .unwrap_or_else(|e| panic!("failed to serve {path} value: {e}"));
        }
        if !node.actions.is_empty() {
            conn.object_server()
                .at(path, FakeAction { state })
                .unwrap_or_else(|e| panic!("failed to serve {path} action: {e}"));
        }
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

/// `org.a11y.atspi.Accessible` implementation for one fake node. All
/// answers read `state` live, so tests can mutate or remove the node
/// after `serve` returns.
struct FakeAccessible {
    unique: String,
    parent: (String, OwnedObjectPath),
    app: (String, OwnedObjectPath),
    state: Arc<Mutex<NodeState>>,
}

#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl FakeAccessible {
    async fn get_children(&self) -> zbus::fdo::Result<Vec<(String, OwnedObjectPath)>> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        // A deliberately slow peer: the deadline/cancellation tests need
        // GetChildren to take real wall-clock time.
        if !state.delay.is_zero() {
            std::thread::sleep(state.delay);
        }
        Ok(state.children.clone())
    }

    async fn get_child_at_index(&self, index: i32) -> (String, OwnedObjectPath) {
        self.state
            .lock()
            .unwrap()
            .children
            .get(index as usize)
            .cloned()
            .unwrap_or_else(|| (String::new(), OwnedObjectPath::try_from(NULL_PATH).unwrap()))
    }

    async fn get_index_in_parent(&self) -> i32 {
        0
    }

    async fn get_role(&self) -> zbus::fdo::Result<u32> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        Ok(state.role)
    }

    async fn get_role_name(&self) -> String {
        self.state.lock().unwrap().role_name.clone()
    }

    async fn get_localized_role_name(&self) -> String {
        self.state.lock().unwrap().role_name.clone()
    }

    async fn get_state(&self) -> Vec<u32> {
        self.state.lock().unwrap().states.clone()
    }

    async fn get_interfaces(&self) -> Vec<String> {
        self.state.lock().unwrap().interfaces.clone()
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
    async fn name(&self) -> zbus::fdo::Result<String> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        Ok(state.name.clone())
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
        self.state.lock().unwrap().children.len() as i32
    }

    #[zbus(property)]
    async fn accessible_id(&self) -> String {
        self.state.lock().unwrap().name.clone()
    }
}

/// `org.a11y.atspi.Component` implementation for bounded fake nodes.
struct FakeComponent {
    state: Arc<Mutex<NodeState>>,
}

#[zbus::interface(name = "org.a11y.atspi.Component")]
impl FakeComponent {
    async fn contains(&self, x: i32, y: i32, _coord_type: u32) -> bool {
        let Some([bx, by, w, h]) = self.state.lock().unwrap().bounds else {
            return false;
        };
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

    async fn get_extents(&self, coord_type: u32) -> (i32, i32, i32, i32) {
        let state = self.state.lock().unwrap();
        let bounds = state.bounds.unwrap_or_default();
        // CoordType::Screen (0): a degenerate peer reports no position.
        if coord_type == 0 && state.degenerate_screen {
            return (0, 0, bounds[2], bounds[3]);
        }
        bounds.into()
    }

    async fn get_position(&self, coord_type: u32) -> (i32, i32) {
        let state = self.state.lock().unwrap();
        let bounds = state.bounds.unwrap_or_default();
        if coord_type == 0 && state.degenerate_screen {
            return (0, 0);
        }
        (bounds[0], bounds[1])
    }

    async fn get_size(&self) -> (i32, i32) {
        let bounds = self.state.lock().unwrap().bounds.unwrap_or_default();
        (bounds[2], bounds[3])
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
    state: Arc<Mutex<NodeState>>,
}

#[zbus::interface(name = "org.a11y.atspi.Text")]
impl FakeText {
    async fn get_text(&self, start_offset: i32, end_offset: i32) -> zbus::fdo::Result<String> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        let text = state.text.clone().unwrap_or_default();
        let start = start_offset.max(0) as usize;
        let end = (end_offset.max(0) as usize).min(text.chars().count());
        Ok(text
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect())
    }

    #[zbus(property)]
    async fn character_count(&self) -> zbus::fdo::Result<i32> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        Ok(state
            .text
            .as_ref()
            .map(|text| text.chars().count() as i32)
            .unwrap_or_default())
    }

    #[zbus(property)]
    async fn caret_offset(&self) -> i32 {
        0
    }
}

/// `org.a11y.atspi.Value` implementation for nodes carrying `value`.
struct FakeValue {
    state: Arc<Mutex<NodeState>>,
}

#[zbus::interface(name = "org.a11y.atspi.Value")]
impl FakeValue {
    #[zbus(property)]
    async fn current_value(&self) -> zbus::fdo::Result<f64> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        Ok(state.value.unwrap_or_default())
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
    async fn text(&self) -> zbus::fdo::Result<String> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        Ok(state.value_text.clone().unwrap_or_default())
    }
}

/// `org.a11y.atspi.Action` implementation for actionable fake nodes:
/// `DoAction` records the invoked action's name into `state.invoked`.
struct FakeAction {
    state: Arc<Mutex<NodeState>>,
}

#[zbus::interface(name = "org.a11y.atspi.Action")]
impl FakeAction {
    async fn do_action(&self, index: i32) -> zbus::fdo::Result<bool> {
        let mut state = self.state.lock().unwrap();
        state.check_live()?;
        let Some(action) = state.actions.get(index as usize) else {
            return Err(zbus::fdo::Error::Failed(format!(
                "no action at index {index}"
            )));
        };
        let name = action.name.clone();
        state.invoked.push(name);
        Ok(true)
    }

    async fn get_actions(&self) -> zbus::fdo::Result<Vec<atspi::Action>> {
        let state = self.state.lock().unwrap();
        state.check_live()?;
        Ok(state.actions.clone())
    }

    async fn get_description(&self, index: i32) -> String {
        self.state
            .lock()
            .unwrap()
            .actions
            .get(index as usize)
            .map(|a| a.description.clone())
            .unwrap_or_default()
    }

    async fn get_key_binding(&self, index: i32) -> String {
        self.state
            .lock()
            .unwrap()
            .actions
            .get(index as usize)
            .map(|a| a.keybinding.clone())
            .unwrap_or_default()
    }

    async fn get_localized_name(&self, index: i32) -> String {
        self.get_name(index).await
    }

    async fn get_name(&self, index: i32) -> String {
        self.state
            .lock()
            .unwrap()
            .actions
            .get(index as usize)
            .map(|a| a.name.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn n_actions(&self) -> i32 {
        self.state.lock().unwrap().actions.len() as i32
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
