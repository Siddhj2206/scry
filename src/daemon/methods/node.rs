//! `click_node` and `node_action`: staleness-checked interaction with
//! fused accessibility nodes (M3.4).
//!
//! Both verbs resolve an `eN` ref through the workspace's retained
//! `get_tree` snapshots into its `(bus_name, object_path, role, name)`
//! identity, then re-query the live node before acting: a vanished node
//! or a drifted role/name fails fast with `STALE_REF` (-32010) and a
//! re-snapshot hint. `click_node` drives a real pointer click at the
//! node's fused screen center through the compositor's targeted input
//! pipeline (which raises and focuses the owning window on press);
//! `node_action` verifies and invokes a named — or the primary default —
//! `org.a11y.atspi.Action` on the node.

use std::{sync::Arc, time::Duration};

use atspi::proxy::action::ActionProxy;
use serde::Deserialize;
use serde_json::{Value, json};
use zbus::proxy::CacheProperties;

use crate::{
    Rect, WindowInfo,
    daemon::{
        a11y::{self, LiveNode, RefTarget, StaleRef, TreeSnapshot},
        methods::input::{button_label, button_param},
        protocol::{ErrorResponse, code},
        registry::Workspace,
        router::{Call, MethodDef, Scope},
    },
    input::PointerTarget,
};

/// Whole-verb bound on the a11y verification + invocation when the
/// caller passes no `deadline_ms` — a hung peer must not pin a request.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

/// `click_node` — staleness-checked pointer click on a node's fused
/// screen center (mutating).
pub const CLICK_NODE: MethodDef = MethodDef {
    name: "click_node",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .clone()
                .expect("workspace scope resolves a workspace");
            click_node(ws, call).await
        })
    },
};

/// `node_action` — staleness-checked `org.a11y.atspi.Action` invocation
/// on a node (mutating).
pub const NODE_ACTION: MethodDef = MethodDef {
    name: "node_action",
    scope: Scope::Workspace { mutating: true },
    run: |call| {
        Box::pin(async move {
            let ws = call
                .workspace
                .clone()
                .expect("workspace scope resolves a workspace");
            node_action(ws, call).await
        })
    },
};

// -- parameter plumbing ---------------------------------------------------------

/// Parameters every ref-targeted verb shares.
#[derive(Deserialize)]
struct RefParams {
    /// The `eN` ref from a `get_tree` snapshot.
    #[serde(rename = "ref", alias = "node")]
    node_ref: String,
    /// The snapshot the ref belongs to (default: the workspace's
    /// latest).
    #[serde(default, alias = "snapshot_id")]
    snapshot: Option<u64>,
    /// Bound the a11y verification + action in milliseconds.
    #[serde(default)]
    deadline_ms: Option<u64>,
}

impl RefParams {
    /// The caller's deadline, or [`DEFAULT_DEADLINE`].
    fn deadline(&self) -> Duration {
        self.deadline_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_DEADLINE)
    }
}

#[derive(Deserialize)]
struct ClickNodeParams {
    #[serde(flatten)]
    ref_params: RefParams,
    /// Button name or evdev code (default `left`).
    #[serde(default)]
    button: Option<Value>,
    /// Press/release repetitions (1–64, default 1).
    #[serde(default)]
    count: Option<u32>,
}

#[derive(Deserialize)]
struct NodeActionParams {
    #[serde(flatten)]
    ref_params: RefParams,
    /// The action name to invoke (default: the node's primary action —
    /// the first `GetActions` entry).
    #[serde(default)]
    action: Option<String>,
}

/// Resolves `node_ref` against the workspace's retained snapshots into
/// its staleness identity.
fn resolve_ref(
    ws: &Workspace,
    ref_params: &RefParams,
) -> Result<(TreeSnapshot, RefTarget), ErrorResponse> {
    let snapshot = match ref_params.snapshot {
        Some(id) => ws.tree_snapshot(id).ok_or_else(|| {
            ErrorResponse::invalid_params_data(
                format!("snapshot {id} is unknown or has been evicted"),
                json!({
                    "snapshot": id,
                    "hint": "run get_tree to capture a fresh snapshot",
                }),
            )
        })?,
        None => ws.latest_tree_snapshot().ok_or_else(|| {
            ErrorResponse::invalid_params(format!(
                "ref {:?} cannot be resolved: no get_tree snapshot exists yet; \
                 run get_tree first",
                ref_params.node_ref
            ))
        })?,
    };
    let target = snapshot.lookup_ref(&ref_params.node_ref).ok_or_else(|| {
        ErrorResponse::invalid_params_data(
            format!(
                "ref {:?} does not exist in snapshot {}",
                ref_params.node_ref, snapshot.id
            ),
            json!({
                "ref": ref_params.node_ref,
                "snapshot_id": snapshot.id,
                "hint": "run get_tree for fresh refs",
            }),
        )
    })?;
    Ok((snapshot, target))
}

/// Connects to the workspace's private a11y bus.
async fn a11y_connection(ws: &Workspace) -> Result<zbus::Connection, ErrorResponse> {
    let address = ws.a11y_address().ok_or_else(|| {
        ErrorResponse::new(code::INTERNAL, "the workspace AT-SPI2 bus is not running")
    })?;
    zbus::connection::Builder::address(address.as_str())
        .map_err(|err| {
            ErrorResponse::new(code::INTERNAL, format!("invalid a11y bus address: {err}"))
        })?
        .build()
        .await
        .map_err(|err| {
            ErrorResponse::new(
                code::INTERNAL,
                format!("failed to connect to the workspace AT-SPI2 bus: {err}"),
            )
        })
}

/// The structured `STALE_REF` failure: what drifted, the live values
/// seen, and the re-snapshot hint.
fn stale_error(
    snapshot: &TreeSnapshot,
    node_ref: &str,
    target: &RefTarget,
    stale: StaleRef,
) -> ErrorResponse {
    let drift = match stale.mismatch {
        "role" => format!(
            "its role changed from {:?} to {:?}",
            target.role,
            stale.actual_role.as_deref().unwrap_or("?")
        ),
        "name" => format!(
            "its name changed from {:?} to {:?}",
            target.name,
            stale.actual_name.as_deref().unwrap_or("?")
        ),
        _ => format!(
            "the node no longer exists{}",
            stale
                .detail
                .as_deref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        ),
    };
    ErrorResponse::with_data(
        code::STALE_REF,
        format!(
            "ref {node_ref:?} is stale: {drift}; run get_tree to re-snapshot and use a fresh ref"
        ),
        json!({
            "ref": node_ref,
            "snapshot_id": snapshot.id,
            "mismatch": stale.mismatch,
            "bus_name": target.bus_name,
            "path": target.object_path,
            "expected": {"role": target.role, "name": target.name},
            "actual": {"role": stale.actual_role, "name": stale.actual_name},
            "hint": "run get_tree to re-snapshot and use fresh refs",
        }),
    )
}

/// Wraps a ref-targeted verb's a11y work in the standard cancellation +
/// deadline envelope shared by both verbs.
async fn bounded<T>(
    verb: &'static str,
    call: &Call,
    deadline: Duration,
    work: impl std::future::Future<Output = Result<T, ErrorResponse>>,
) -> Result<T, ErrorResponse> {
    tokio::select! {
        _ = call.cancel.cancelled() => {
            Err(ErrorResponse::with_data(
                code::CANCELLED,
                format!("{verb} cancelled: client disconnected"),
                json!({"reason": "disconnected"}),
            ))
        }
        outcome = tokio::time::timeout(deadline, work) => {
            outcome.map_err(|_| {
                ErrorResponse::with_data(
                    code::TIMEOUT,
                    format!("{verb} exceeded its a11y deadline"),
                    json!({"verb": verb}),
                )
            })?
        }
    }
}

// -- click_node ---------------------------------------------------------------------

async fn click_node(ws: Arc<Workspace>, call: Call) -> Result<Value, ErrorResponse> {
    let params: ClickNodeParams =
        serde_json::from_value(call.params.clone()).map_err(ErrorResponse::invalid_params)?;
    let button = button_param(params.button.as_ref())?;
    let count = params.count.unwrap_or(1);
    if !(1..=64).contains(&count) {
        return Err(ErrorResponse::invalid_params(format!(
            "click count must be between 1 and 64, got {count}"
        )));
    }
    let node_ref = params.ref_params.node_ref.clone();
    let (snapshot, target) = resolve_ref(&ws, &params.ref_params)?;

    let (center, reply) = bounded("click_node", &call, params.ref_params.deadline(), async {
        let conn = a11y_connection(&ws).await?;
        let live = a11y::verify_ref(&conn, &target)
            .await
            .map_err(|stale| stale_error(&snapshot, &node_ref, &target, stale))?;

        // The node's fused center: live extents re-anchored on the
        // window's current geometry when the peer serves `Component`,
        // else the snapshot's fused bounds.
        let reply = ws.call(|handle| handle.windows()).await?;
        let window = reply
            .value
            .iter()
            .find(|w| w.id == target.window_id)
            .cloned()
            .ok_or_else(|| window_gone_error(&snapshot, &node_ref, &target))?;
        let bounds = fused_bounds(&conn, &snapshot, &target, &live, &window).await?;
        let center = (
            f64::from(bounds.x) + f64::from(bounds.width) / 2.0,
            f64::from(bounds.y) + f64::from(bounds.height) / 2.0,
        );

        // Dispatch through the window-targeted pipeline: the owning
        // window receives the click even under occlusion, and the press
        // raises + focuses it (click-to-focus).
        let window_id = target.window_id;
        let local = (
            center.0 - f64::from(window.geometry.x),
            center.1 - f64::from(window.geometry.y),
        );
        let reply = ws
            .call(move |handle| {
                let mut ack =
                    handle.move_pointer(PointerTarget::Window(window_id), local.0, local.1)?;
                for _ in 0..count {
                    handle.press_pointer_button(button)?;
                    ack = handle.release_pointer_button(button)?;
                }
                Ok(ack)
            })
            .await?;
        Ok((center, reply))
    })
    .await?;

    Ok(json!({
        "ref": node_ref,
        "snapshot_id": snapshot.id,
        "window": target.window_id.0,
        "x": center.0,
        "y": center.1,
        "button": button_label(button),
        "clicked": count,
        "seq": reply.seq,
    }))
}

/// The node's fused absolute bounds: live screen extents re-anchored on
/// the window's current compositor geometry (via the fused root's live
/// extents — the same delta `get_tree` applied at capture), else the
/// snapshot's fused bounds.
async fn fused_bounds(
    conn: &zbus::Connection,
    snapshot: &TreeSnapshot,
    target: &RefTarget,
    live: &LiveNode,
    window: &WindowInfo,
) -> Result<Rect, ErrorResponse> {
    if let Some(extents) = live.extents {
        let root = snapshot
            .windows
            .iter()
            .find(|w| w.window_id == target.window_id)
            .and_then(|w| w.root.as_ref());
        if let Some(root) = root
            && let Some(root_extents) = a11y::extents_at(conn, &root.bus_name, &root.path).await
        {
            return Ok(Rect {
                x: extents.0 + (window.geometry.x - root_extents.0),
                y: extents.1 + (window.geometry.y - root_extents.1),
                width: extents.2,
                height: extents.3,
            });
        }
    }
    target.bounds.ok_or_else(|| {
        ErrorResponse::invalid_params(
            "ref exposes no component bounds; it cannot be clicked \
             (use node_action, or click coordinates with `click`)",
        )
    })
}

/// The node's window disappeared between snapshot and click — the ref's
/// fused coordinates are meaningless, so this reports `STALE_REF` too.
fn window_gone_error(snapshot: &TreeSnapshot, node_ref: &str, target: &RefTarget) -> ErrorResponse {
    stale_error(
        snapshot,
        node_ref,
        target,
        StaleRef {
            mismatch: "missing",
            actual_role: None,
            actual_name: None,
            detail: Some("the node's window is gone".to_string()),
        },
    )
}

// -- node_action --------------------------------------------------------------------

async fn node_action(ws: Arc<Workspace>, call: Call) -> Result<Value, ErrorResponse> {
    let params: NodeActionParams =
        serde_json::from_value(call.params.clone()).map_err(ErrorResponse::invalid_params)?;
    let node_ref = params.ref_params.node_ref.clone();
    let (snapshot, target) = resolve_ref(&ws, &params.ref_params)?;

    let (action_name, index) = bounded("node_action", &call, params.ref_params.deadline(), async {
        let conn = a11y_connection(&ws).await?;
        let live = a11y::verify_ref(&conn, &target)
            .await
            .map_err(|stale| stale_error(&snapshot, &node_ref, &target, stale))?;
        if !live.has_action {
            return Err(ErrorResponse::invalid_params_data(
                format!("ref {node_ref:?} exposes no org.a11y.atspi.Action interface"),
                json!({
                    "ref": node_ref,
                    "hint": "this node cannot perform actions; use click_node instead",
                }),
            ));
        }
        let proxy = ActionProxy::builder(&conn)
            .destination(target.bus_name.as_str())
            .and_then(|b| b.path(target.object_path.as_str()))
            .map_err(|err| {
                ErrorResponse::new(
                    code::INTERNAL,
                    format!("invalid action proxy target: {err}"),
                )
            })?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|err| {
                ErrorResponse::new(
                    code::INTERNAL,
                    format!("failed to reach the node's Action interface: {err}"),
                )
            })?;
        let actions = proxy.get_actions().await.map_err(|err| {
            ErrorResponse::new(
                code::INTERNAL,
                format!("failed to list the node's actions: {err}"),
            )
        })?;
        let (index, name) = match &params.action {
            Some(name) => {
                let index = actions
                    .iter()
                    .position(|action| action.name == *name)
                    .ok_or_else(|| {
                        ErrorResponse::invalid_params_data(
                            format!("ref {node_ref:?} does not offer action {name:?}"),
                            json!({
                                "ref": node_ref,
                                "action": name,
                                "available": actions
                                    .iter()
                                    .map(|action| action.name.clone())
                                    .collect::<Vec<_>>(),
                            }),
                        )
                    })?;
                (index, name.clone())
            }
            None => {
                let first = actions.first().ok_or_else(|| {
                    ErrorResponse::invalid_params_data(
                        format!("ref {node_ref:?} offers no actions"),
                        json!({
                            "ref": node_ref,
                            "available": Vec::<String>::new(),
                        }),
                    )
                })?;
                (0, first.name.clone())
            }
        };
        let performed = proxy.do_action(index as i32).await.map_err(|err| {
            ErrorResponse::new(
                code::INTERNAL,
                format!("DoAction({index}) on ref {node_ref:?} failed: {err}"),
            )
        })?;
        if !performed {
            return Err(ErrorResponse::with_data(
                code::INTERNAL,
                format!(
                    "action {name:?} on ref {node_ref:?} was rejected by the application \
                         (DoAction returned false)"
                ),
                json!({
                    "ref": node_ref,
                    "action": name,
                    "action_index": index,
                }),
            ));
        }
        Ok((name, index))
    })
    .await?;

    Ok(json!({
        "ref": node_ref,
        "snapshot_id": snapshot.id,
        "window": target.window_id.0,
        "action": action_name,
        "action_index": index,
        "performed": true,
    }))
}
