//! AT-SPI2 ↔ compositor fusion: the engine behind `get_tree`.
//!
//! Each capture enumerates the workspace's private a11y bus for registered
//! application roots (the `org.a11y.atspi.Registry` desktop's children),
//! correlates every app to compositor windows by exact pid
//! (`GetConnectionUnixProcessID` ↔ `WindowInfo`'s `SO_PEERCRED` pid, with
//! frame-name/title matching when one process owns multiple toplevels),
//! then walks each matched subtree translating `Component` screen extents
//! into absolute workspace coordinates anchored on the window's
//! authoritative logical geometry.
//!
//! Snapshots get a monotonic `snapshot_id` and per-node `eN` refs whose
//! `(bus_name, object_path, role, exact_name)` identity is retained —
//! M3.4 re-verifies that triple on `click_node`/`node_action`, M3.5 diffs
//! snapshots through [`SnapshotStore`].

use std::{collections::VecDeque, time::Duration};

use atspi::{
    CoordType, Interface, ObjectRefOwned, Role,
    proxy::{
        accessible::{AccessibleProxy, ObjectRefExt},
        component::ComponentProxy,
    },
};
use serde::Serialize;
use zbus::{fdo::DBusProxy, names::BusName, proxy::CacheProperties};

use crate::{Rect, WindowId, WindowInfo, daemon::now_ms};

/// Recent `get_tree` snapshots retained per workspace — the ring
/// `tree_diff` (M3.5) resolves `from_snapshot` through.
pub const SNAPSHOT_HISTORY: usize = 8;

/// Defensive traversal ceilings until M3.3 wires caller-facing bounds: a
/// degenerate or cyclic peer can never make `get_tree` walk forever.
const MAX_NODES: usize = 10_000;
/// Same guard on descent depth.
const MAX_DEPTH: usize = 64;
/// Whole-capture deadline when the caller passes none: a hung peer must
/// not pin the request forever.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

/// The registry's well-known name and its desktop root object — the
/// objects `Socket::Embed` and `Accessible::GetChildren` speak to.
const REGISTRY_NAME: &str = "org.a11y.atspi.Registry";
const REGISTRY_ROOT: &str = "/org/a11y/atspi/accessible/root";

/// The fallback hint attached to windows that carry no a11y tree.
const CAPTURE_HINT: &str =
    "no accessibility tree; use capture_window for a pixel-level view of this window";

/// Per-window fusion outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum A11yStatus {
    /// Fused tree captured for this window.
    Ok,
    /// An app matched but produced an empty tree (no children).
    Empty,
    /// No AT-SPI2 application registered for this window's pid, or the
    /// a11y bus itself is unreachable.
    Unavailable,
    /// The app matched but querying its tree failed.
    Error,
}

/// The a11y peer a window fused with — the correlation audit trail.
#[derive(Debug, Clone, Serialize)]
pub struct A11yPeer {
    /// The peer's unique name on the workspace a11y bus.
    pub bus_name: String,
    /// The peer's Unix pid per `GetConnectionUnixProcessID`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The application root's accessible name, when readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
}

/// One accessibility node in a fused snapshot, with its deterministic
/// `eN` ref and the `(bus_name, path, role, exact name)` staleness
/// identity action verbs re-check before acting (M3.4).
#[derive(Debug, Clone, Serialize)]
pub struct TreeNode {
    /// The compact per-snapshot ref (`e1`, `e2`, ...).
    #[serde(rename = "ref")]
    pub node_ref: String,
    /// The node's accessible role name (`"frame"`, `"push button"`, ...).
    pub role: String,
    /// The node's exact accessible name.
    pub name: String,
    /// AT-SPI state names present on the node (`"enabled"`, `"focused"`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    /// Fused absolute workspace coordinates, when the node exposes a
    /// `Component` interface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    /// The owning peer's unique name on the workspace a11y bus.
    pub bus_name: String,
    /// The object's path within the owning peer.
    pub path: String,
    /// Why this node's children are absent despite a failed `GetChildren`
    /// (peer error mid-walk).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Descendant nodes, in accessible order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

/// One window's fused entry in a `get_tree` snapshot: the compositor's
/// authoritative metadata plus the matched a11y subtree (or the reason
/// there is none).
#[derive(Debug, Clone, Serialize)]
pub struct FusedWindow {
    /// The compositor `WindowId`.
    pub window_id: WindowId,
    /// `xdg_toplevel.app_id`, if the client set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// `xdg_toplevel.title`, if the client set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `SO_PEERCRED` pid of the window's client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Whether the toplevel is currently activated.
    pub activated: bool,
    /// The window's authoritative logical bounds.
    pub geometry: Rect,
    /// Fusion outcome for this window.
    pub a11y_status: A11yStatus,
    /// The a11y peer this window correlated with, when it matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a11y: Option<A11yPeer>,
    /// Why the window carries no tree (`unavailable`/`error` detail).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a11y_error: Option<String>,
    /// Observation fallback for windows without a usable tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The fused accessible subtree rooted at this window's frame (or the
    /// application root when no frame could be distinguished).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<TreeNode>,
}

/// A retained `get_tree` result.
#[derive(Debug, Clone, Serialize)]
pub struct TreeSnapshot {
    /// Monotonic per-workspace snapshot identifier.
    pub id: u64,
    /// Capture wall-clock time (ms since epoch).
    pub created_at_ms: u64,
    /// Fused windows, in compositor order.
    pub windows: Vec<FusedWindow>,
}

/// Everything an action verb needs to re-verify a ref's identity on the
/// live peer before acting: `(bus_name, object_path, role, exact_name)`.
#[derive(Debug, Clone)]
pub struct RefTarget {
    /// The window the node fused into.
    pub window_id: WindowId,
    /// The owning peer's unique bus name at snapshot time.
    pub bus_name: String,
    /// The object's path within the owning peer.
    pub object_path: String,
    /// The node's role name at snapshot time.
    pub role: String,
    /// The node's exact name at snapshot time.
    pub name: String,
    /// The node's fused bounds at snapshot time.
    pub bounds: Option<Rect>,
}

impl TreeSnapshot {
    /// Resolves an `eN` ref back to its staleness identity (`click_node`/
    /// `node_action` re-query role + name at this `(bus_name, path)`).
    pub fn lookup_ref(&self, node_ref: &str) -> Option<RefTarget> {
        fn walk(nodes: &[TreeNode], node_ref: &str, window_id: WindowId) -> Option<RefTarget> {
            for node in nodes {
                if node.node_ref == node_ref {
                    return Some(RefTarget {
                        window_id,
                        bus_name: node.bus_name.clone(),
                        object_path: node.path.clone(),
                        role: node.role.clone(),
                        name: node.name.clone(),
                        bounds: node.bounds,
                    });
                }
                if let Some(found) = walk(&node.children, node_ref, window_id) {
                    return Some(found);
                }
            }
            None
        }
        self.windows.iter().find_map(|w| {
            w.root
                .as_ref()
                .and_then(|root| walk(std::slice::from_ref(root), node_ref, w.window_id))
        })
    }
}

/// Per-workspace bounded ring of recent fused snapshots — M3.4's ref
/// lookups and M3.5's `tree_diff`/`tree_find` read back through it.
#[derive(Debug, Default)]
pub struct SnapshotStore {
    next_id: u64,
    snapshots: VecDeque<TreeSnapshot>,
}

impl SnapshotStore {
    /// Records `windows` as the next snapshot, evicting the oldest when
    /// the ring is full, and returns the stored snapshot.
    pub fn record(&mut self, windows: Vec<FusedWindow>) -> TreeSnapshot {
        self.next_id += 1;
        let snapshot = TreeSnapshot {
            id: self.next_id,
            created_at_ms: now_ms(),
            windows,
        };
        self.snapshots.push_back(snapshot.clone());
        while self.snapshots.len() > SNAPSHOT_HISTORY {
            self.snapshots.pop_front();
        }
        snapshot
    }

    /// A retained snapshot by id (`None` once evicted or never taken).
    pub fn get(&self, id: u64) -> Option<&TreeSnapshot> {
        self.snapshots.iter().find(|s| s.id == id)
    }

    /// The most recent snapshot, if any.
    pub fn latest(&self) -> Option<&TreeSnapshot> {
        self.snapshots.back()
    }
}

/// The a11y-bus side of a capture, reported at the response top level so
/// a dead bus is diagnosable without scanning every window.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BusReport {
    /// Connected to the workspace a11y bus; `registered_apps` counts the
    /// application roots the registry reported.
    Connected {
        /// Number of application roots the registry reported.
        registered_apps: usize,
    },
    /// No live a11y bus address (broker down or never started).
    Unavailable {
        /// Why no live address was available.
        reason: String,
    },
    /// The bus accepted a connection but the registry query failed.
    Error {
        /// What the registry query reported.
        message: String,
    },
}

/// The full `get_tree` capture: per-window fused entries plus the bus
/// report.
#[derive(Debug)]
pub struct Capture {
    /// The a11y bus report for this capture.
    pub bus: BusReport,
    /// Fused windows, in compositor order.
    pub windows: Vec<FusedWindow>,
}

/// A toplevel-role child of an app root, paired with its exact accessible
/// name for title matching.
struct Frame {
    obj: ObjectRefOwned,
    name: String,
    /// Whether a window has already claimed this frame in the capture.
    claimed: bool,
}

/// An application root the registry reported, with its resolved peer pid
/// and lazily fetched toplevel-role children.
struct AppPeer {
    bus_name: String,
    pid: Option<u32>,
    root: ObjectRefOwned,
    name: Option<String>,
    frames: Option<Vec<Frame>>,
}

impl AppPeer {
    /// This app's toplevel-role children as `(object ref, exact name)`,
    /// fetched (and cached) by querying each child's `Name`/`GetRole`.
    async fn frames(&mut self, conn: &zbus::Connection) -> &mut Vec<Frame> {
        if self.frames.is_none() {
            let mut frames = Vec::new();
            if let Ok(proxy) = self.root.as_accessible_proxy(conn).await
                && let Ok(children) = proxy.get_children().await
            {
                for child in children {
                    if child.is_null() {
                        continue;
                    }
                    let Ok(child_proxy) = child.as_accessible_proxy(conn).await else {
                        continue;
                    };
                    let Ok(role) = child_proxy.get_role().await else {
                        continue;
                    };
                    if !is_toplevel_role(role) {
                        continue;
                    }
                    let name = child_proxy.name().await.unwrap_or_default();
                    frames.push(Frame {
                        obj: child,
                        name,
                        claimed: false,
                    });
                }
            }
            self.frames = Some(frames);
        }
        self.frames.as_mut().expect("frames populated above")
    }
}

/// Window-level roles that pair with Wayland toplevels when one process
/// owns several: the app root's `Frame`/`Dialog`/`Window`/`Alert` children.
fn is_toplevel_role(role: Role) -> bool {
    matches!(
        role,
        Role::Frame | Role::Dialog | Role::Window | Role::Alert | Role::InternalFrame
    )
}

/// Fills the per-window fallback fields for `status`.
fn fallback(window: &WindowInfo, status: A11yStatus, error: String) -> FusedWindow {
    FusedWindow {
        window_id: window.id,
        app_id: window.app_id.clone(),
        title: window.title.clone(),
        pid: window.pid,
        activated: window.activated,
        geometry: window.geometry,
        a11y_status: status,
        a11y: None,
        a11y_error: Some(error),
        hint: Some(CAPTURE_HINT.to_string()),
        root: None,
    }
}

/// Every window marked `unavailable` with the same reason — the shape a
/// down/empty a11y bus takes in `get_tree`.
fn all_unavailable(windows: &[WindowInfo], bus: BusReport, reason: &str) -> Capture {
    Capture {
        bus,
        windows: windows
            .iter()
            .map(|w| fallback(w, A11yStatus::Unavailable, reason.to_string()))
            .collect(),
    }
}

/// Captures a fused tree for `windows` (in compositor order) against the
/// workspace's private a11y bus.
///
/// Never panics and never fails on a dead or empty bus: every degradation
/// lands in per-window `a11y_status`/`a11y_error` and the `bus` report.
/// The only hard failure is the whole-capture deadline.
pub async fn capture(
    a11y_address: Option<String>,
    windows: &[WindowInfo],
    deadline: Option<Duration>,
) -> Result<Capture, String> {
    let work = capture_inner(a11y_address, windows);
    match tokio::time::timeout(deadline.unwrap_or(DEFAULT_DEADLINE), work).await {
        Ok(capture) => Ok(capture),
        Err(_elapsed) => Err("a11y capture exceeded its deadline".to_string()),
    }
}

async fn capture_inner(a11y_address: Option<String>, windows: &[WindowInfo]) -> Capture {
    let Some(address) = a11y_address else {
        return all_unavailable(
            windows,
            BusReport::Unavailable {
                reason: "the workspace AT-SPI2 bus is not running".to_string(),
            },
            "the workspace AT-SPI2 bus is not running",
        );
    };
    let connect =
        zbus::connection::Builder::address(address.as_str()).map(|builder| builder.build());
    let conn = match connect {
        Ok(fut) => match fut.await {
            Ok(conn) => conn,
            Err(err) => {
                return all_unavailable(
                    windows,
                    BusReport::Error {
                        message: format!("failed to connect to a11y bus: {err}"),
                    },
                    "failed to connect to the workspace AT-SPI2 bus",
                );
            }
        },
        Err(err) => {
            return all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("invalid a11y bus address: {err}"),
                },
                "the workspace AT-SPI2 bus address is invalid",
            );
        }
    };

    let dbus = match DBusProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            return all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("failed to reach the bus daemon interface: {err}"),
                },
                "the workspace AT-SPI2 bus is not answering",
            );
        }
    };

    // The registry's desktop root: its children are the registered
    // applications' root accessibles.
    let registry = match accessible_at(&conn, REGISTRY_NAME, REGISTRY_ROOT).await {
        Ok(proxy) => proxy,
        Err(err) => {
            return all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("failed to reach the AT-SPI2 registry: {err}"),
                },
                "the AT-SPI2 registry is not answering",
            );
        }
    };
    let app_roots = match registry.get_children().await {
        Ok(children) => children,
        Err(err) => {
            return all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("failed to enumerate registered applications: {err}"),
                },
                "the AT-SPI2 registry is not answering",
            );
        }
    };

    let mut apps: Vec<AppPeer> = Vec::new();
    for root in app_roots {
        if root.is_null() {
            continue;
        }
        let Some(unique) = root.name().cloned() else {
            continue;
        };
        let bus_name = unique.as_str().to_string();
        let pid = dbus
            .get_connection_unix_process_id(BusName::Unique(unique))
            .await
            .ok();
        let name = match root.as_accessible_proxy(&conn).await {
            Ok(proxy) => proxy.name().await.ok(),
            Err(_) => None,
        };
        apps.push(AppPeer {
            bus_name,
            pid,
            root,
            name,
            frames: None,
        });
    }

    let bus = BusReport::Connected {
        registered_apps: apps.len(),
    };
    // Same-pid windows are correlated as a group (frame-name ↔ title
    // matching must see the whole group at once, and windows sharing a
    // pid need not be contiguous in compositor order).
    let mut pids_in_order: Vec<Option<u32>> = Vec::new();
    for window in windows {
        if !pids_in_order.contains(&window.pid) {
            pids_in_order.push(window.pid);
        }
    }
    let mut fused: Vec<Option<FusedWindow>> = windows.iter().map(|_| None).collect();
    let mut ref_counter = 0u64;
    for pid in pids_in_order {
        let group: Vec<usize> = windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.pid == pid)
            .map(|(i, _)| i)
            .collect();
        for &window_index in &group {
            fused[window_index] = Some(
                fuse_group_window(
                    &conn,
                    &mut apps,
                    &windows[window_index],
                    group.len(),
                    &mut ref_counter,
                )
                .await,
            );
        }
    }
    Capture {
        bus,
        windows: fused
            .into_iter()
            .map(|w| w.expect("every window fused"))
            .collect(),
    }
}

/// Fuses one window inside its same-pid group.
async fn fuse_group_window(
    conn: &zbus::Connection,
    apps: &mut [AppPeer],
    window: &WindowInfo,
    group_len: usize,
    ref_counter: &mut u64,
) -> FusedWindow {
    let Some(pid) = window.pid else {
        return fallback(
            window,
            A11yStatus::Unavailable,
            "window has no client pid (cannot correlate an accessibility peer)".to_string(),
        );
    };
    let candidates: Vec<usize> = apps
        .iter()
        .enumerate()
        .filter(|(_, app)| app.pid == Some(pid))
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        return fallback(
            window,
            A11yStatus::Unavailable,
            format!("no AT-SPI2 application registered for pid {pid}"),
        );
    }

    // Resolve which object is this window's fused root.
    let (app_index, root) = if group_len == 1 {
        let app_index = candidates[0];
        match pick_single_window_root(conn, &mut apps[app_index], window).await {
            Ok(root) => (app_index, root),
            Err(err) => return fallback(window, A11yStatus::Error, err),
        }
    } else {
        match match_multi_window_root(conn, apps, &candidates, window).await {
            Some((app_index, root)) => (app_index, root),
            None => {
                return fallback(
                    window,
                    A11yStatus::Unavailable,
                    format!(
                        "pid {pid} owns {group_len} windows but none of its accessibility frames is named {:?}",
                        window.title.as_deref().unwrap_or("")
                    ),
                );
            }
        }
    };

    // Anchor: the fused root's own screen extents are translated so the
    // tree lands exactly on the compositor's authoritative geometry.
    // Toolkits that already report compositor-absolute extents then get a
    // zero delta; window-relative toolkits get the window origin.
    let delta = component_extents(conn, &root)
        .await
        .map(|e| (window.geometry.x - e.0, window.geometry.y - e.1))
        .unwrap_or((0, 0));

    let mut budget = Budget::default();
    let root_node = build_node(conn, &root, delta, ref_counter, &mut budget).await;

    let peer = A11yPeer {
        bus_name: apps[app_index].bus_name.clone(),
        pid: apps[app_index].pid,
        app_name: apps[app_index].name.clone(),
    };
    let mut entry = fused_base(window, peer);
    match root_node {
        Some(node) => {
            entry.a11y_status = if node.children.is_empty() {
                entry.hint = Some(CAPTURE_HINT.to_string());
                A11yStatus::Empty
            } else {
                A11yStatus::Ok
            };
            entry.root = Some(node);
        }
        None => {
            entry.a11y_status = A11yStatus::Error;
            entry.a11y_error = Some("the matched accessibility peer stopped answering".to_string());
            entry.hint = Some(CAPTURE_HINT.to_string());
        }
    }
    entry
}

/// The `FusedWindow` scaffold every fused outcome shares.
fn fused_base(window: &WindowInfo, peer: A11yPeer) -> FusedWindow {
    FusedWindow {
        window_id: window.id,
        app_id: window.app_id.clone(),
        title: window.title.clone(),
        pid: window.pid,
        activated: window.activated,
        geometry: window.geometry,
        a11y_status: A11yStatus::Unavailable,
        a11y: Some(peer),
        a11y_error: None,
        hint: None,
        root: None,
    }
}

/// Single-window correlation: prefer a toplevel-role child whose name
/// matches the window title, then a lone toplevel child, then the app
/// root itself (toolkits that hang their whole tree off the application
/// object).
async fn pick_single_window_root(
    conn: &zbus::Connection,
    app: &mut AppPeer,
    window: &WindowInfo,
) -> Result<ObjectRefOwned, String> {
    let frames = app.frames(conn).await;
    if let Some(title) = &window.title
        && let Some(frame) = frames.iter_mut().find(|frame| frame.name == *title)
    {
        frame.claimed = true;
        return Ok(frame.obj.clone());
    }
    if frames.len() == 1 {
        frames[0].claimed = true;
        return Ok(frames[0].obj.clone());
    }
    Ok(app.root.clone())
}

/// Multi-window, same-pid correlation: the window's fused root is the
/// unclaimed toplevel-role child whose accessible name equals the window
/// title (the spec's secondary match). `None` when no frame matches.
async fn match_multi_window_root(
    conn: &zbus::Connection,
    apps: &mut [AppPeer],
    candidates: &[usize],
    window: &WindowInfo,
) -> Option<(usize, ObjectRefOwned)> {
    let title = window.title.as_deref()?;
    for &app_index in candidates {
        let frames = apps[app_index].frames(conn).await;
        for frame in frames.iter_mut() {
            if frame.name == title && !frame.claimed {
                frame.claimed = true;
                return Some((app_index, frame.obj.clone()));
            }
        }
    }
    None
}

/// Builds an `AccessibleProxy` for `destination` + `path` with property
/// caching off (a11y objects don't emit `PropertiesChanged` reliably).
async fn accessible_at<'a>(
    conn: &'a zbus::Connection,
    destination: &'a str,
    path: &'a str,
) -> Result<AccessibleProxy<'a>, zbus::Error> {
    let destination: BusName = destination
        .try_into()
        .map_err(|e| zbus::Error::Failure(format!("bad bus name {destination:?}: {e}")))?;
    let path: zbus::zvariant::ObjectPath = path
        .try_into()
        .map_err(|e| zbus::Error::Failure(format!("bad object path {path:?}: {e}")))?;
    AccessibleProxy::builder(conn)
        .destination(destination)?
        .path(path)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// Traversal budget guards (M3.3 turns these into caller-facing bounds
/// with inline truncation markers).
#[derive(Default)]
struct Budget {
    nodes: usize,
}

/// A stack frame in the iterative depth-first walk.
struct WalkFrame {
    node: TreeNode,
    /// Remaining child refs, reversed so `pop` yields accessible order.
    pending: Vec<ObjectRefOwned>,
}

/// Reads one object's fields and returns it as a child-less `TreeNode`
/// plus its child refs. Per-field failures degrade (empty name, unknown
/// role, no bounds) rather than killing the walk; only a failed proxy
/// build / `GetChildren` propagates an error note.
async fn read_node(
    conn: &zbus::Connection,
    obj: &ObjectRefOwned,
    delta: (i32, i32),
    ref_counter: &mut u64,
    budget: &mut Budget,
) -> Option<WalkFrame> {
    if budget.nodes >= MAX_NODES {
        return None;
    }
    budget.nodes += 1;
    *ref_counter += 1;
    let node_ref = format!("e{ref_counter}");
    let bus_name = obj.name_as_str().unwrap_or_default().to_string();
    let path = obj.path_as_str().to_string();
    let proxy = obj.as_accessible_proxy(conn).await.ok()?;

    let name = proxy.name().await.unwrap_or_default();
    let role = proxy
        .get_role()
        .await
        .map(|role| role.name().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let states = proxy
        .get_state()
        .await
        .map(|set| {
            set.iter()
                .map(|state| state.to_static_str().to_string())
                .collect()
        })
        .unwrap_or_default();
    let interfaces = proxy.get_interfaces().await.unwrap_or_default();
    let bounds = if interfaces.contains(Interface::Component) {
        component_extents(conn, obj)
            .await
            .map(|(x, y, width, height)| Rect {
                x: x + delta.0,
                y: y + delta.1,
                width,
                height,
            })
    } else {
        None
    };
    let (mut pending, error) = match proxy.get_children().await {
        Ok(children) => (children, None),
        Err(err) => (Vec::new(), Some(format!("GetChildren failed: {err}"))),
    };
    pending.reverse();
    Some(WalkFrame {
        node: TreeNode {
            node_ref,
            role,
            name,
            states,
            bounds,
            bus_name,
            path,
            error,
            children: Vec::new(),
        },
        pending,
    })
}

/// Iterative depth-first walk from `root`, fusing every node's bounds by
/// `delta`. `None` only when the root object cannot be proxied at all.
async fn build_node(
    conn: &zbus::Connection,
    root: &ObjectRefOwned,
    delta: (i32, i32),
    ref_counter: &mut u64,
    budget: &mut Budget,
) -> Option<TreeNode> {
    let mut stack: Vec<WalkFrame> = vec![read_node(conn, root, delta, ref_counter, budget).await?];

    let mut completed_root: Option<TreeNode> = None;
    while let Some(frame) = stack.last_mut() {
        if let Some(child_ref) = frame.pending.pop() {
            if child_ref.is_null() || stack.len() >= MAX_DEPTH || budget.nodes >= MAX_NODES {
                continue;
            }
            match read_node(conn, &child_ref, delta, ref_counter, budget).await {
                Some(child_frame) => stack.push(child_frame),
                None => continue, // child vanished mid-walk; siblings continue
            }
        } else {
            let done = stack.pop().expect("non-empty stack").node;
            match stack.last_mut() {
                Some(parent) => parent.node.children.push(done),
                None => {
                    completed_root = Some(done);
                    break;
                }
            }
        }
    }
    completed_root
}

/// `Component::GetExtents` in screen coordinates for `obj`, if the peer
/// serves the interface and answers.
async fn component_extents(
    conn: &zbus::Connection,
    obj: &ObjectRefOwned,
) -> Option<(i32, i32, i32, i32)> {
    let destination = BusName::try_from(obj.name_as_str()?).ok()?;
    let proxy = ComponentProxy::builder(conn)
        .destination(destination)
        .ok()?
        .path(obj.path().clone())
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()?;
    proxy.get_extents(CoordType::Screen).await.ok()
}
