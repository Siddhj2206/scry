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

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use atspi::{
    CoordType, Interface, ObjectRef, ObjectRefOwned, Role,
    proxy::{
        accessible::{AccessibleProxy, ObjectRefExt},
        component::ComponentProxy,
        text::TextProxy,
        value::ValueProxy,
    },
};
use serde::Serialize;
use zbus::{fdo::DBusProxy, names::BusName, proxy::CacheProperties};

use crate::{Rect, WindowId, WindowInfo, daemon::now_ms};

/// Recent `get_tree` snapshots retained per workspace — the ring
/// `tree_diff` (M3.5) resolves `from_snapshot` through.
pub const SNAPSHOT_HISTORY: usize = 8;

/// Default node-count cap for `get_tree` (`max_nodes` override): even a
/// degenerate or cyclic peer cannot make a request walk forever.
pub const DEFAULT_MAX_NODES: usize = 10_000;
/// Default descent-depth cap (`max_depth` override), counting the fused
/// root as depth 1.
pub const DEFAULT_MAX_DEPTH: usize = 64;
/// Whole-capture deadline when the caller passes no `deadline_ms`: a hung
/// peer must not pin the request forever.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);
/// Grace added to `deadline` for the hard capture timeout. The in-loop
/// deadline normally wins (yielding a partial tree with markers); this
/// slack only bounds a peer hung inside a single D-Bus call, where the
/// loop never gets a chance to check.
const DEADLINE_SLACK: Duration = Duration::from_secs(3);

/// Sentinel substituted for `ROLE_PASSWORD_TEXT` text and value fields —
/// secret characters never enter a snapshot or log.
pub const REDACTED: &str = "[REDACTED]";

/// Defensive cap on one node's `Text` contents, keeping a document-sized
/// text field from swamping the snapshot.
const MAX_TEXT_CHARS: i32 = 4096;

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

/// Why a branch was cut short during a bounded walk — the
/// machine-readable half of every inline `- ... <reason>` marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// `max_depth` stopped descent below this node.
    Depth,
    /// `max_nodes` ran out of budget before this branch finished.
    Nodes,
    /// `deadline_ms` elapsed while this branch was still open.
    Deadline,
}

impl TruncationReason {
    /// The inline note text (`- ... <reason>`) rendered in JSON and by
    /// `scry tree`.
    pub fn note(self) -> &'static str {
        match self {
            Self::Depth => "- ... depth limit reached",
            Self::Nodes => "- ... node limit reached",
            Self::Deadline => "- ... deadline exceeded",
        }
    }
}

/// The inline truncation marker attached to a node (or window) whose
/// subtree was cut short by a capture bound.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Truncation {
    /// Which bound tripped: `"depth"`, `"nodes"` or `"deadline"`.
    pub reason: TruncationReason,
    /// The inline note (`- ... depth limit reached`, ...).
    pub note: &'static str,
}

impl Truncation {
    fn of(reason: TruncationReason) -> Self {
        Self {
            reason,
            note: reason.note(),
        }
    }
}

/// A `Value` interface reading: the current value as a number, or the
/// redaction sentinel for `ROLE_PASSWORD_TEXT` nodes.
#[derive(Debug, Clone, Copy)]
pub enum NodeValue {
    /// `Value::CurrentValue`.
    Number(f64),
    /// Withheld — the node's value could reveal a secret (or its length).
    Redacted,
}

impl Serialize for NodeValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Number(v) => serializer.serialize_f64(*v),
            Self::Redacted => serializer.serialize_str(REDACTED),
        }
    }
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
    /// Text contents from the `Text` interface (capped at
    /// `MAX_TEXT_CHARS`); `"[REDACTED]"` on `ROLE_PASSWORD_TEXT` nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The `Value` interface's current reading; `"[REDACTED]"` on
    /// `ROLE_PASSWORD_TEXT` nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<NodeValue>,
    /// The `Value` interface's textual form, when non-empty; likewise
    /// redacted on password nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_text: Option<String>,
    /// `true` on `ROLE_PASSWORD_TEXT` nodes: text/value are sentinels,
    /// never live peer data.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
    /// The owning peer's unique name on the workspace a11y bus.
    pub bus_name: String,
    /// The object's path within the owning peer.
    pub path: String,
    /// Why this node's children are absent despite a failed `GetChildren`
    /// (peer error mid-walk).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why descent below this node stopped: the capture bound that
    /// tripped, plus the inline `- ... <reason>` note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncation>,
    /// Descendant nodes, in accessible order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

fn is_false(v: &bool) -> bool {
    !*v
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
    /// Set when a matched peer's tree could not even start because a
    /// capture bound was already exhausted — the window's own inline
    /// `- ... <reason>` marker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncation>,
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

/// The caller-facing bounds of one `get_tree` capture.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Maximum descent depth; the fused root counts as depth 1.
    pub max_depth: usize,
    /// Maximum nodes emitted across the whole capture.
    pub max_nodes: usize,
    /// Wall-clock deadline: past it, open branches degrade into
    /// `- ... deadline exceeded` markers and the walk unwinds.
    pub deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_nodes: DEFAULT_MAX_NODES,
            deadline: DEFAULT_DEADLINE,
        }
    }
}

/// A resolved `root_ref` subtree scope: the staleness identity read from
/// a retained snapshot, re-verified against the live peer at capture
/// time before the subtree is walked.
#[derive(Debug, Clone)]
pub struct SubtreeRoot {
    /// The window the ref fused into.
    pub window_id: WindowId,
    /// The owning peer's unique name on the workspace a11y bus.
    pub bus_name: String,
    /// The object's path within the owning peer.
    pub object_path: String,
    /// The node's role name at snapshot time.
    pub role: String,
    /// The node's exact name at snapshot time.
    pub name: String,
    /// The node's fused bounds at snapshot time — the anchor the
    /// re-walked subtree is translated onto.
    pub anchor: Option<Rect>,
}

/// How a `get_tree` capture can fail outright (partial trees degrade into
/// truncation markers instead of failing).
#[derive(Debug)]
pub enum CaptureError {
    /// A peer call hung past `deadline + slack` — no partial tree could
    /// be produced. Maps to `TIMEOUT`.
    Deadline,
    /// The `root_ref` node is gone or its `(role, exact name)` identity
    /// changed since the snapshot it came from. Maps to `STALE_REF`.
    StaleRoot {
        /// The role name the snapshot recorded.
        expected_role: String,
        /// The exact name the snapshot recorded.
        expected_name: String,
        /// The live `(role, name)` — `None` when the object stopped
        /// answering entirely.
        actual: Option<(String, String)>,
    },
}

/// Response-level truncation bookkeeping: which bounds tripped, how many
/// inline markers were emitted, and how many nodes made the snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct TruncationSummary {
    /// Bounds that fired during the capture (`"depth"`, `"nodes"`,
    /// `"deadline"`), in tripping order.
    pub tripped: Vec<TruncationReason>,
    /// How many inline `- ... <reason>` markers the capture emitted.
    pub markers: usize,
    /// Nodes emitted across all windows.
    pub nodes_emitted: usize,
}

/// The full `get_tree` capture: per-window fused entries, the bus report
/// and the truncation summary.
#[derive(Debug)]
pub struct Capture {
    /// The a11y bus report for this capture.
    pub bus: BusReport,
    /// Fused windows, in compositor order.
    pub windows: Vec<FusedWindow>,
    /// Capture-bounds bookkeeping (`limits` echoes at the handler level).
    pub truncation: TruncationSummary,
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
        truncated: None,
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
        truncation: TruncationSummary {
            tripped: Vec::new(),
            markers: 0,
            nodes_emitted: 0,
        },
    }
}

/// Captures a fused tree for `windows` (in compositor order) against the
/// workspace's private a11y bus.
///
/// Never panics and never fails on a dead or empty bus: every degradation
/// lands in per-window `a11y_status`/`a11y_error`, the `bus` report, or
/// inline truncation markers. `subtree` scopes the capture to one
/// previously snapshotted node (`root_ref`); its identity is re-verified
/// against the live peer first. The only hard failures are a peer call
/// outliving `deadline + slack` and a stale subtree root.
pub async fn capture(
    a11y_address: Option<String>,
    windows: &[WindowInfo],
    limits: &Limits,
    subtree: Option<&SubtreeRoot>,
) -> Result<Capture, CaptureError> {
    let work = capture_inner(a11y_address, windows, limits, subtree);
    match tokio::time::timeout(limits.deadline + DEADLINE_SLACK, work).await {
        Ok(capture) => capture,
        Err(_elapsed) => Err(CaptureError::Deadline),
    }
}

async fn capture_inner(
    a11y_address: Option<String>,
    windows: &[WindowInfo],
    limits: &Limits,
    subtree: Option<&SubtreeRoot>,
) -> Result<Capture, CaptureError> {
    let mut budget = Budget::new(limits);
    let Some(address) = a11y_address else {
        return Ok(all_unavailable(
            windows,
            BusReport::Unavailable {
                reason: "the workspace AT-SPI2 bus is not running".to_string(),
            },
            "the workspace AT-SPI2 bus is not running",
        ));
    };
    let connect =
        zbus::connection::Builder::address(address.as_str()).map(|builder| builder.build());
    let conn = match connect {
        Ok(fut) => match fut.await {
            Ok(conn) => conn,
            Err(err) => {
                return Ok(all_unavailable(
                    windows,
                    BusReport::Error {
                        message: format!("failed to connect to a11y bus: {err}"),
                    },
                    "failed to connect to the workspace AT-SPI2 bus",
                ));
            }
        },
        Err(err) => {
            return Ok(all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("invalid a11y bus address: {err}"),
                },
                "the workspace AT-SPI2 bus address is invalid",
            ));
        }
    };

    let dbus = match DBusProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            return Ok(all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("failed to reach the bus daemon interface: {err}"),
                },
                "the workspace AT-SPI2 bus is not answering",
            ));
        }
    };

    // `root_ref` scoping: the subtree root's staleness identity is
    // re-checked against the live peer, then walked directly — no
    // registry enumeration or window correlation needed.
    if let Some(subtree) = subtree {
        return subtree_capture(&conn, &dbus, windows, subtree, &mut budget).await;
    }

    // The registry's desktop root: its children are the registered
    // applications' root accessibles.
    let registry = match accessible_at(&conn, REGISTRY_NAME, REGISTRY_ROOT).await {
        Ok(proxy) => proxy,
        Err(err) => {
            return Ok(all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("failed to reach the AT-SPI2 registry: {err}"),
                },
                "the AT-SPI2 registry is not answering",
            ));
        }
    };
    let app_roots = match registry.get_children().await {
        Ok(children) => children,
        Err(err) => {
            return Ok(all_unavailable(
                windows,
                BusReport::Error {
                    message: format!("failed to enumerate registered applications: {err}"),
                },
                "the AT-SPI2 registry is not answering",
            ));
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
                    &mut budget,
                )
                .await,
            );
        }
    }
    Ok(Capture {
        bus,
        windows: fused
            .into_iter()
            .map(|w| w.expect("every window fused"))
            .collect(),
        truncation: budget.summary(),
    })
}

/// Fuses one window inside its same-pid group.
async fn fuse_group_window(
    conn: &zbus::Connection,
    apps: &mut [AppPeer],
    window: &WindowInfo,
    group_len: usize,
    ref_counter: &mut u64,
    budget: &mut Budget,
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

    let peer = A11yPeer {
        bus_name: apps[app_index].bus_name.clone(),
        pid: apps[app_index].pid,
        app_name: apps[app_index].name.clone(),
    };
    let mut entry = fused_base(window, peer);

    // A global bound already exhausted (node budget spent by earlier
    // windows, or the deadline passed mid-correlation): this window's
    // peer matched but nothing can be emitted — mark it at the window
    // level instead of starting a doomed walk.
    if let Some(reason) = budget.cut() {
        entry.a11y_status = A11yStatus::Ok;
        entry.truncated = Some(Truncation::of(reason));
        budget.note(reason);
        return entry;
    }

    // Anchor: the fused root's own screen extents are translated so the
    // tree lands exactly on the compositor's authoritative geometry.
    // Toolkits that already report compositor-absolute extents then get a
    // zero delta; window-relative toolkits get the window origin.
    let delta = component_extents(conn, &root)
        .await
        .map(|e| (window.geometry.x - e.0, window.geometry.y - e.1))
        .unwrap_or((0, 0));

    let root_node = build_node(conn, &root, delta, ref_counter, budget).await;
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
        truncated: None,
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

/// One `root_ref`-scoped capture: re-verify the subtree root's staleness
/// identity live, then walk just that subtree under the same bounds.
///
/// Registry enumeration is skipped (the ref already pins
/// `bus_name`/`path`); the bus report still reflects connectivity.
async fn subtree_capture(
    conn: &zbus::Connection,
    dbus: &DBusProxy<'_>,
    windows: &[WindowInfo],
    subtree: &SubtreeRoot,
    budget: &mut Budget,
) -> Result<Capture, CaptureError> {
    let stale = |actual: Option<(String, String)>| CaptureError::StaleRoot {
        expected_role: subtree.role.clone(),
        expected_name: subtree.name.clone(),
        actual,
    };

    let Some(window) = windows.iter().find(|w| w.id == subtree.window_id) else {
        return Err(stale(None));
    };

    // Resolve the ref's `(bus_name, object_path)` to an ObjectRef and
    // re-check `(role, exact name)` on the live peer.
    let unique =
        zbus::names::UniqueName::try_from(subtree.bus_name.clone()).map_err(|_| stale(None))?;
    let path = zbus::zvariant::ObjectPath::try_from(subtree.object_path.clone())
        .map_err(|_| stale(None))?;
    let obj = ObjectRef::new_owned(unique.clone(), path);
    let proxy = obj
        .as_accessible_proxy(conn)
        .await
        .map_err(|_| stale(None))?;
    let live_role = proxy.get_role().await.map_err(|_| stale(None))?;
    let live_name = proxy.name().await.map_err(|_| stale(None))?;
    if live_role.name() != subtree.role || live_name != subtree.name {
        return Err(stale(Some((live_role.name().to_string(), live_name))));
    }

    // Anchor on the bounds the snapshot recorded so re-walked coordinates
    // stay in absolute workspace space (zero delta when nothing moved).
    let delta = match subtree.anchor {
        Some(anchor) => component_extents(conn, &obj)
            .await
            .map(|e| (anchor.x - e.0, anchor.y - e.1))
            .unwrap_or((0, 0)),
        None => (0, 0),
    };

    // Peer provenance for the entry's correlation audit trail.
    let pid = dbus
        .get_connection_unix_process_id(BusName::Unique(unique))
        .await
        .ok();
    let mut app_name = None;
    if let Ok(app_ref) = proxy.get_application().await
        && !app_ref.is_null()
        && let Ok(app_proxy) = app_ref.as_accessible_proxy(conn).await
    {
        app_name = app_proxy.name().await.ok();
    }
    let peer = A11yPeer {
        bus_name: subtree.bus_name.clone(),
        pid,
        app_name,
    };

    let mut entry = fused_base(window, peer);
    entry.a11y_status = A11yStatus::Ok;
    if let Some(reason) = budget.cut() {
        entry.truncated = Some(Truncation::of(reason));
        budget.note(reason);
    } else {
        let mut ref_counter = 0u64;
        match build_node(conn, &obj, delta, &mut ref_counter, budget).await {
            Some(node) => entry.root = Some(node),
            None => {
                entry.a11y_status = A11yStatus::Error;
                entry.a11y_error =
                    Some("the matched accessibility peer stopped answering".to_string());
                entry.hint = Some(CAPTURE_HINT.to_string());
            }
        }
    }
    Ok(Capture {
        bus: BusReport::Connected { registered_apps: 0 },
        windows: vec![entry],
        truncation: budget.summary(),
    })
}

/// Shared traversal budget across all windows of one capture: counts
/// emitted nodes, tracks the wall-clock deadline, and records which
/// bounds tripped plus how many inline markers were placed.
struct Budget {
    limits: Limits,
    nodes: usize,
    deadline: Instant,
    tripped: Vec<TruncationReason>,
    markers: usize,
}

impl Budget {
    fn new(limits: &Limits) -> Self {
        Self {
            limits: limits.clone(),
            nodes: 0,
            deadline: Instant::now() + limits.deadline,
            tripped: Vec::new(),
            markers: 0,
        }
    }

    /// The global bound that currently forbids further descent, if any:
    /// the deadline first (elapsed wall-clock), then the node budget.
    fn cut(&self) -> Option<TruncationReason> {
        if Instant::now() >= self.deadline {
            return Some(TruncationReason::Deadline);
        }
        if self.nodes >= self.limits.max_nodes {
            return Some(TruncationReason::Nodes);
        }
        None
    }

    /// Records a bound as tripped (once, in first-tripped order).
    fn trip(&mut self, reason: TruncationReason) {
        if !self.tripped.contains(&reason) {
            self.tripped.push(reason);
        }
    }

    /// Attaches a `- ... <reason>` marker to `node` if it isn't already
    /// marked (first bound wins the note; both still count as tripped).
    fn mark(&mut self, node: &mut TreeNode, reason: TruncationReason) {
        self.trip(reason);
        if node.truncated.is_none() {
            node.truncated = Some(Truncation::of(reason));
            self.markers += 1;
        }
    }

    /// Records a window-level `- ... <reason>` marker.
    fn note(&mut self, reason: TruncationReason) {
        self.trip(reason);
        self.markers += 1;
    }

    fn summary(&self) -> TruncationSummary {
        TruncationSummary {
            tripped: self.tripped.clone(),
            markers: self.markers,
            nodes_emitted: self.nodes,
        }
    }
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
///
/// `ROLE_PASSWORD_TEXT` nodes never have their `Text`/`Value` read at
/// all: the redaction sentinel is emitted without touching the secret.
async fn read_node(
    conn: &zbus::Connection,
    obj: &ObjectRefOwned,
    delta: (i32, i32),
    ref_counter: &mut u64,
    budget: &mut Budget,
) -> Option<WalkFrame> {
    budget.nodes += 1;
    *ref_counter += 1;
    let node_ref = format!("e{ref_counter}");
    let bus_name = obj.name_as_str().unwrap_or_default().to_string();
    let path = obj.path_as_str().to_string();
    let proxy = obj.as_accessible_proxy(conn).await.ok()?;

    let name = proxy.name().await.unwrap_or_default();
    let role = proxy.get_role().await.ok();
    let redacted = role == Some(Role::PasswordText);
    let role = role
        .map(|role| role.name().to_string())
        .unwrap_or_else(|| "unknown".to_string());
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
    let (text, value, value_text) = if redacted {
        (
            interfaces
                .contains(Interface::Text)
                .then(|| REDACTED.to_string()),
            interfaces
                .contains(Interface::Value)
                .then_some(NodeValue::Redacted),
            interfaces
                .contains(Interface::Value)
                .then(|| REDACTED.to_string()),
        )
    } else {
        let text = if interfaces.contains(Interface::Text) {
            node_text(conn, obj).await
        } else {
            None
        };
        let (value, value_text) = if interfaces.contains(Interface::Value) {
            node_value(conn, obj).await
        } else {
            (None, None)
        };
        (text, value, value_text)
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
            text,
            value,
            value_text,
            redacted,
            bus_name,
            path,
            error,
            truncated: None,
            children: Vec::new(),
        },
        pending,
    })
}

/// Iterative depth-first walk from `root` under `budget`, fusing every
/// node's bounds by `delta`.
///
/// Bound semantics: `max_depth` cuts the branch below a node (marker on
/// that node, walk continues at its siblings); `max_nodes`/`deadline` are
/// capture-wide — when either trips, every open frame still holding
/// unread children gets its own `- ... <reason>` marker and the walk
/// unwinds. The root is always read, even on an exhausted budget.
/// `None` only when the root object cannot be proxied at all.
async fn build_node(
    conn: &zbus::Connection,
    root: &ObjectRefOwned,
    delta: (i32, i32),
    ref_counter: &mut u64,
    budget: &mut Budget,
) -> Option<TreeNode> {
    let mut stack: Vec<WalkFrame> = vec![read_node(conn, root, delta, ref_counter, budget).await?];

    let mut completed_root: Option<TreeNode> = None;
    while !stack.is_empty() {
        let depth = stack.len();
        let frame = stack.last_mut().expect("non-empty stack");
        // A capture-wide bound trips: mark the branch cut and unwind —
        // every frame still holding children gets its own marker as it
        // pops, then attaches to its parent like a completed node.
        if let Some(reason) = budget.cut() {
            if !frame.pending.is_empty() {
                budget.mark(&mut frame.node, reason);
            }
            frame.pending.clear();
        }
        if let Some(child_ref) = frame.pending.pop() {
            if child_ref.is_null() {
                continue;
            }
            if depth >= budget.limits.max_depth {
                // The child would exceed max_depth: cut this branch at
                // the node (skipping its remaining children too) and keep
                // walking its siblings.
                budget.mark(&mut frame.node, TruncationReason::Depth);
                frame.pending.clear();
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

/// `Text` contents for `obj`, capped at `MAX_TEXT_CHARS`. `None` when the
/// peer declines either call — text is advisory, never walk-fatal.
async fn node_text(conn: &zbus::Connection, obj: &ObjectRefOwned) -> Option<String> {
    let proxy = text_proxy(conn, obj).await?;
    let count = proxy.character_count().await.ok()?;
    if count <= 0 {
        return Some(String::new());
    }
    proxy.get_text(0, count.min(MAX_TEXT_CHARS)).await.ok()
}

/// `(current_value, text)` for `obj`'s `Value` interface, best-effort per
/// field.
async fn node_value(
    conn: &zbus::Connection,
    obj: &ObjectRefOwned,
) -> (Option<NodeValue>, Option<String>) {
    let Some(proxy) = value_proxy(conn, obj).await else {
        return (None, None);
    };
    let value = proxy.current_value().await.ok().map(NodeValue::Number);
    let value_text = proxy.text().await.ok().filter(|text| !text.is_empty());
    (value, value_text)
}

async fn text_proxy<'a>(
    conn: &'a zbus::Connection,
    obj: &'a ObjectRefOwned,
) -> Option<TextProxy<'a>> {
    let destination = BusName::try_from(obj.name_as_str()?).ok()?;
    TextProxy::builder(conn)
        .destination(destination)
        .ok()?
        .path(obj.path().clone())
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()
}

async fn value_proxy<'a>(
    conn: &'a zbus::Connection,
    obj: &'a ObjectRefOwned,
) -> Option<ValueProxy<'a>> {
    let destination = BusName::try_from(obj.name_as_str()?).ok()?;
    ValueProxy::builder(conn)
        .destination(destination)
        .ok()?
        .path(obj.path().clone())
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()
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
