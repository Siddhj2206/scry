//! Integration tests for M2.6: `get_changes_since` structured diffs, named
//! timeline marks (`mark_set`/`mark_list`) and the diagnostic `status` verb.
//!
//! The seam under test is the daemon's real Unix-socket JSON-RPC 2.0
//! protocol; workspace-side transitions (window map/update/close, focus,
//! damage commits, clipboard updates) are produced by a real wayland-client
//! driving the workspace's `WAYLAND_DISPLAY` socket.

mod common;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use common::daemon::{RPC_TIMEOUT, RpcClient};
use common::{ClientState, TestClient};
use scry::daemon::{DaemonConfig, DaemonHandle};
use serde_json::{Value, json};
use tempfile::TempDir;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source},
};

/// A live daemon bound to a fresh temporary runtime/state directory.
struct TestDaemon {
    #[allow(dead_code)]
    handle: DaemonHandle,
    _dir: TempDir,
    socket_path: PathBuf,
}

fn spawn_daemon() -> TestDaemon {
    spawn_daemon_with(scry::daemon::DEFAULT_CHANGE_LOG_CAPACITY)
}

fn spawn_daemon_with(change_log_capacity: usize) -> TestDaemon {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let socket_path = dir.path().join("run/scry.sock");
    let state_dir = dir.path().join("state");
    let config = DaemonConfig {
        socket_path: socket_path.clone(),
        state_dir: state_dir.clone(),
        default_idle_ttl: Duration::from_secs(300),
        reaper_interval: Duration::from_millis(25),
        change_log_capacity,
    };
    let handle = DaemonHandle::spawn(config).expect("daemon failed to start");
    TestDaemon {
        handle,
        _dir: dir,
        socket_path,
    }
}

/// Creates workspace `id` and returns `(socket_path, current_seq)`.
fn create_workspace(rpc: &mut RpcClient, id: &str) -> (PathBuf, u64) {
    let created = rpc.call("workspace_create", json!({"id": id}));
    (
        PathBuf::from(created["workspace"]["socket_path"].as_str().unwrap()),
        created["seq"].as_u64().unwrap(),
    )
}

/// Polls `get_changes_since(since)` until `pred` holds and returns the diff.
///
/// Compositor events propagate through each workspace's event pump thread,
/// so transitions land in the change log just after the client roundtrip
/// that produced them.
fn get_changes_until(
    rpc: &mut RpcClient,
    workspace: &str,
    since: u64,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        let diff = rpc.call(
            "get_changes_since",
            json!({"workspace": workspace, "seq": since}),
        );
        if pred(&diff) {
            return diff;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for change-log condition: {diff}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Polls `status` until `pred` holds and returns the report.
fn status_until(rpc: &mut RpcClient, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + RPC_TIMEOUT;
    loop {
        let status = rpc.call("status", json!({}));
        if pred(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for status condition: {status}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The `status` entry for workspace `id`.
fn workspace_status<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .unwrap_or_else(|| panic!("workspace {id:?} missing from status: {status}"))
}

#[test]
fn get_changes_since_reports_a_compact_structured_diff() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (socket_path, before) = create_workspace(&mut rpc, "diffy");

    // --- Phase 1: map a window ---------------------------------------------
    let mut client = TestClient::connect(&socket_path);
    let top_a = client.map_toplevel(200, 100, "app-a", "alpha");
    let diff1 = get_changes_until(&mut rpc, "diffy", before, |d| {
        !d["windows"]["mapped"].as_array().unwrap().is_empty()
    });
    assert_eq!(diff1["truncated"], false);
    assert_eq!(diff1["since_seq"], before);
    let mapped = diff1["windows"]["mapped"].as_array().unwrap();
    assert_eq!(mapped.len(), 1);
    let win_a = mapped[0]["id"].as_u64().unwrap();
    assert_eq!(mapped[0]["app_id"], "app-a");
    // Mapping moved keyboard focus to the new window.
    assert_eq!(diff1["focus"]["changed"], true);
    assert_eq!(diff1["focus"]["window_id"], win_a);

    let mid = diff1["current_seq"].as_u64().unwrap();
    assert!(mid > before);

    // --- Phase 2: update, damage, second map+close, clipboard, mark ---------
    top_a.toplevel.set_title("alpha-renamed".to_string());
    top_a.surface.commit();
    let second = client.shm_buffer(200, 100, 0xff00_ff00);
    top_a.surface.attach(Some(&second), 0, 0);
    top_a.surface.damage(0, 0, 50, 25);
    top_a.surface.commit();
    client.roundtrip();

    let top_b = client.map_toplevel(50, 50, "app-b", "beta");
    let diff_b = get_changes_until(&mut rpc, "diffy", mid, |d| {
        !d["windows"]["mapped"].as_array().unwrap().is_empty()
    });
    let win_b = diff_b["windows"]["mapped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .find(|id| *id != win_a)
        .expect("second window did not map");
    top_b.toplevel.destroy();
    top_b.surface.destroy();
    client.roundtrip();

    // A real client-side clipboard update.
    let data_device_manager = client
        .globals
        .bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(&client.qh, 1..=3, ())
        .expect("wl_data_device_manager not advertised");
    let data_device =
        data_device_manager.get_data_device(client.seat.as_ref().unwrap(), &client.qh, ());
    let source = data_device_manager.create_data_source(&client.qh, ());
    source.offer("text/plain".to_string());
    data_device.set_selection(Some(&source), 0);
    client.roundtrip();

    let marked = rpc.call(
        "mark_set",
        json!({"workspace": "diffy", "name": "after-login", "metadata": {"phase": 2}}),
    );
    let mark_seq = marked["mark"]["seq"].as_u64().unwrap();

    let diff = get_changes_until(&mut rpc, "diffy", mid, |d| {
        d["marks_added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "after-login")
            && d["windows"]["closed"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id.as_u64() == Some(win_b))
            && !d["clipboard"].as_array().unwrap().is_empty()
    });

    // The envelope and diff agree on the latest seq.
    let current = diff["current_seq"].as_u64().unwrap();
    assert!(current >= mark_seq);
    assert_eq!(diff["seq"], current);
    assert_eq!(diff["truncated"], false);

    // Window A was updated (new title) — deduplicated to its latest info.
    let updated = diff["windows"]["updated"].as_array().unwrap();
    assert_eq!(updated.len(), 1, "expected only window A updated: {diff}");
    assert_eq!(updated[0]["id"], win_a);
    assert_eq!(updated[0]["title"], "alpha-renamed");
    // Window B mapped and closed inside the range: reported as closed only.
    assert!(diff["windows"]["mapped"].as_array().unwrap().is_empty());
    assert_eq!(
        diff["windows"]["closed"].as_array().unwrap(),
        &[json!(win_b)]
    );

    // Focus went B -> back to A when B closed; the latest wins.
    assert_eq!(diff["focus"]["changed"], true);
    assert_eq!(diff["focus"]["window_id"], win_a);

    // Damage is coalesced per window.
    let damage_a = diff["damage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["window_id"] == win_a)
        .expect("no damage recorded for window A");
    assert!(
        damage_a["rects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| *r == json!({"x":0,"y":0,"width":50,"height":25})),
        "damage rects missing the damaged region: {damage_a}"
    );

    // The client selection shows up as a clipboard update.
    let clipboard = diff["clipboard"].as_array().unwrap();
    assert!(
        clipboard.iter().any(|c| c["selection"] == "clipboard"
            && c["mime_types"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m == "text/plain")),
        "clipboard update missing: {diff}"
    );

    // The named mark lands on the timeline inside the range.
    let marks = diff["marks_added"].as_array().unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0]["name"], "after-login");
    assert_eq!(marks[0]["seq"], mark_seq);
    assert_eq!(marks[0]["metadata"], json!({"phase": 2}));
}

#[test]
fn get_changes_since_reports_empty_and_truncated_diffs() {
    // A tiny retention buffer so a handful of commits overflows it.
    let daemon = spawn_daemon_with(6);
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (socket_path, before) = create_workspace(&mut rpc, "flood");

    let mut client = TestClient::connect(&socket_path);
    let toplevel = client.map_toplevel(100, 100, "app", "t");
    // Overwhelm the retained history with damage commits.
    for _ in 0..16 {
        let buffer = client.shm_buffer(100, 100, 0xff11_2233);
        toplevel.surface.attach(Some(&buffer), 0, 0);
        toplevel.surface.damage(0, 0, 10, 10);
        toplevel.surface.commit();
        client.roundtrip();
    }

    // A seq older than the retained buffer is flagged with a resync hint.
    let diff = get_changes_until(&mut rpc, "flood", before, |d| d["truncated"] == true);
    assert!(
        diff["resync_hint"].is_string(),
        "truncated diff carries no resync hint: {diff}"
    );
    let oldest = diff["oldest_retained_seq"].as_u64().unwrap();
    assert!(oldest > before, "nothing was evicted: {diff}");

    // A diff at current_seq is empty and untruncated.
    let current = diff["current_seq"].as_u64().unwrap();
    let now = rpc.call(
        "get_changes_since",
        json!({"workspace": "flood", "seq": current}),
    );
    assert_eq!(now["truncated"], false);
    assert_eq!(now["current_seq"], current);
    assert!(now["windows"]["mapped"].as_array().unwrap().is_empty());
    assert!(now["windows"]["updated"].as_array().unwrap().is_empty());
    assert!(now["windows"]["closed"].as_array().unwrap().is_empty());
    assert!(now["popups"]["mapped"].as_array().unwrap().is_empty());
    assert!(now["clipboard"].as_array().unwrap().is_empty());
    assert!(now["damage"].as_array().unwrap().is_empty());
    assert!(now["marks_added"].as_array().unwrap().is_empty());
    assert_eq!(now["focus"]["changed"], false);

    // The retained tail still diffs correctly below the truncation line.
    let tail = rpc.call(
        "get_changes_since",
        json!({"workspace": "flood", "seq": oldest}),
    );
    assert_eq!(tail["truncated"], false);
    assert!(
        !tail["damage"].as_array().unwrap().is_empty()
            || !tail["windows"]["updated"].as_array().unwrap().is_empty(),
        "recent diff lost its retained changes: {tail}"
    );

    // Validation: `seq` is required; unknown workspaces error cleanly.
    let err = rpc.call_err("get_changes_since", json!({"workspace": "flood"}));
    assert_eq!(err.code, -32602);
    let err = rpc.call_err("get_changes_since", json!({"workspace": "ghost", "seq": 0}));
    assert_eq!(err.code, -32001);
}

#[test]
fn marks_attach_to_the_seq_timeline_in_order() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (_, before) = create_workspace(&mut rpc, "marked");

    let m1 = rpc.call(
        "mark_set",
        json!({"workspace": "marked", "name": "before-login"}),
    );
    let m1_seq = m1["mark"]["seq"].as_u64().unwrap();
    assert!(m1_seq >= before);
    assert_eq!(m1["mark"]["name"], "before-login");
    assert!(m1["mark"]["set_at_ms"].as_u64().is_some());
    // The mark attaches at the workspace's current seq.
    assert_eq!(m1["seq"], m1_seq);

    let m2 = rpc.call(
        "mark_set",
        json!({
            "workspace": "marked",
            "name": "after-login",
            "metadata": {"phase": "login", "ok": true},
        }),
    );
    let m2_seq = m2["mark"]["seq"].as_u64().unwrap();
    assert!(m2_seq >= m1_seq, "mark seqs are not monotonic");

    // mark_list returns every mark in monotonic sequence order.
    let listed = rpc.call("mark_list", json!({"workspace": "marked"}));
    let marks = listed["marks"].as_array().unwrap();
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[0]["name"], "before-login");
    assert_eq!(marks[1]["name"], "after-login");
    assert_eq!(marks[1]["metadata"], json!({"phase": "login", "ok": true}));
    let seqs: Vec<u64> = marks.iter().map(|m| m["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] <= w[1]));

    // Marks show up in get_changes_since over their seq range.
    let diff = rpc.call(
        "get_changes_since",
        json!({"workspace": "marked", "seq": m1_seq.saturating_sub(1)}),
    );
    let names: Vec<&str> = diff["marks_added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["before-login", "after-login"]);

    // Validation and the readOnly gate.
    let err = rpc.call_err("mark_set", json!({"workspace": "marked", "name": ""}));
    assert_eq!(err.code, -32602);
    let err = rpc.call_err(
        "mark_set",
        json!({"workspace": "marked", "name": "x".repeat(200)}),
    );
    assert_eq!(err.code, -32602);
    let err = rpc.call_err("mark_list", json!({"workspace": "ghost"}));
    assert_eq!(err.code, -32001);

    rpc.call(
        "workspace_create",
        json!({"id": "frozen", "read_only": true}),
    );
    let err = rpc.call_err("mark_set", json!({"workspace": "frozen", "name": "nope"}));
    assert_eq!(err.code, -32003);
    // mark_list is an observation verb: allowed on readOnly workspaces.
    let listed = rpc.call("mark_list", json!({"workspace": "frozen"}));
    assert_eq!(listed["marks"], json!([]));
}

#[test]
fn status_reports_a_healthy_daemon_and_workspaces() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    let (socket_path, _) = create_workspace(&mut rpc, "ops");

    let spawned = rpc.call(
        "spawn_app",
        json!({"workspace": "ops", "program": "sleep", "args": ["300"]}),
    );
    let app_pid = spawned["pid"].as_u64().unwrap();

    let mut client = TestClient::connect(&socket_path);
    let _toplevel = client.map_toplevel(120, 80, "ops-app", "Ops");

    let status = status_until(&mut rpc, |s| {
        workspace_status(s, "ops")["window_count"] == 1
    });

    // Daemon-level health.
    let daemon_info = &status["daemon"];
    assert_eq!(
        daemon_info["socket_path"],
        daemon.socket_path.to_str().unwrap()
    );
    assert_eq!(daemon_info["socket_listening"], true);
    assert!(daemon_info["uptime_ms"].as_u64().is_some());
    assert!(daemon_info["pid"].as_u64().is_some());
    assert_eq!(daemon_info["workspace_count"], 1);

    // Narrative fields.
    assert!(status["running"].as_str().unwrap().contains("ops"));
    assert_eq!(status["healthy"], true);
    assert_eq!(status["unhealthy"], json!([]));
    assert!(
        !status["next_steps"].as_array().unwrap().is_empty(),
        "status suggests no next steps: {status}"
    );

    // Workspace-level detail: compositor, outputs, windows, processes.
    let ws = workspace_status(&status, "ops");
    assert_eq!(ws["state"], "live");
    assert_eq!(ws["healthy"], true);
    assert_eq!(ws["socket_listening"], true);
    assert_eq!(ws["compositor_responsive"], true);
    assert!(
        ws["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["name"] == "Virtual-1"),
        "status output list missing Virtual-1: {ws}"
    );
    assert_eq!(ws["window_count"], 1);
    assert_eq!(ws["windows"][0]["app_id"], "ops-app");
    let processes = ws["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 1);
    assert_eq!(processes[0]["pid"], app_pid);
    assert_eq!(processes[0]["state"], "running");

    // Scoped status narrows to one workspace; unknown ids error cleanly.
    let scoped = rpc.call("status", json!({"workspace": "ops"}));
    assert_eq!(scoped["workspaces"].as_array().unwrap().len(), 1);
    let err = rpc.call_err("status", json!({"workspace": "ghost"}));
    assert_eq!(err.code, -32001);
}

#[test]
fn status_flags_crashed_spawned_apps() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);
    create_workspace(&mut rpc, "crashy");

    let spawned = rpc.call(
        "spawn_app",
        json!({"workspace": "crashy", "program": "sh", "args": ["-c", "exit 7"]}),
    );
    let pid = spawned["pid"].as_u64().unwrap();

    // The child monitor reaps asynchronously; wait for the crash to land.
    let status = status_until(&mut rpc, |s| {
        workspace_status(s, "crashy")["processes"][0]["state"] != "running"
    });

    assert_eq!(status["healthy"], false);
    let ws = workspace_status(&status, "crashy");
    assert_eq!(ws["healthy"], false);
    assert_eq!(ws["processes"][0]["state"], "exited");
    assert_eq!(ws["processes"][0]["exit_code"], 7);

    // The unhealthy narrative identifies the crashed process, and
    // next_steps points at a concrete action.
    let unhealthy = status["unhealthy"].as_array().unwrap();
    assert_eq!(unhealthy.len(), 1, "unexpected findings: {status}");
    let finding = unhealthy[0].as_str().unwrap();
    assert!(
        finding.contains(&pid.to_string()),
        "finding lacks pid: {finding}"
    );
    assert!(
        finding.contains("exit"),
        "finding lacks exit detail: {finding}"
    );
    let steps = status["next_steps"].as_array().unwrap();
    assert!(
        steps
            .iter()
            .any(|s| s.as_str().unwrap().contains("spawn_app")),
        "no actionable next step: {status}"
    );
}

// -- clipboard plumbing (mirrors tests/clipboard.rs, minimal) ---------------

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
        if let wl_data_device::Event::DataOffer { id } = event {
            state.clipboard.offer = Some((id, Vec::new()));
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
