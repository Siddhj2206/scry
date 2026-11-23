//! Integration tests for the scripted XDG Desktop Portal stub (M3.6):
//! `org.freedesktop.portal.FileChooser` and `org.freedesktop.portal.OpenURI`
//! served in-process on each workspace's private session bus, plus the
//! `portal_script_set` / `portal_script_clear` / `portal_history` verbs.
//!
//! The seam under test is the daemon's real Unix-socket JSON-RPC 2.0
//! protocol for control verbs, paired with real D-Bus client connections
//! (`zbus` blocking API) to each workspace's private session bus acting as
//! ordinary portal callers — never internal daemon state.

mod common;

use std::{
    collections::HashMap,
    path::Path,
    process::{Command, Output},
    sync::mpsc,
};

use common::daemon::{RPC_TIMEOUT, RpcClient, spawn_daemon};
use serde_json::{Value, json};
use zbus::zvariant;

/// `workspace_list`'s entry for `id`.
fn workspace_entry(rpc: &mut RpcClient, id: &str) -> Value {
    rpc.call("workspace_list", json!({}))["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("workspace {id:?} not listed"))
}

/// The workspace session-bus address reported by `workspace_list`.
fn session_address(rpc: &mut RpcClient, id: &str) -> String {
    workspace_entry(rpc, id)["bus"]["session"]["address"]
        .as_str()
        .expect("workspace has no session bus")
        .to_string()
}

/// A real D-Bus client connection to `address` (`unix:path=...`).
fn connect_bus(address: &str) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(address)
        .unwrap_or_else(|e| panic!("invalid bus address {address:?}: {e}"))
        .build()
        .unwrap_or_else(|e| panic!("failed to connect to bus {address:?}: {e}"))
}

/// A received `org.freedesktop.portal.Request::Response` signal.
#[derive(Debug)]
struct PortalResponse {
    /// The request object path the signal was emitted on.
    path: String,
    /// The `response` code (0 accepted, 1 cancelled, 2 closed).
    code: u32,
    /// The `results` dict, decoded to a JSON-shaped map for assertions.
    results: HashMap<String, zvariant::OwnedValue>,
}

/// Subscribes a fresh connection to every `Request::Response` signal on
/// the bus and streams decoded responses back over a channel. Mirrors a
/// real portal client that installs its match rule before calling:
/// returns only once the match rule is registered with the broker.
fn response_listener(address: &str) -> mpsc::Receiver<PortalResponse> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let address = address.to_string();
    std::thread::Builder::new()
        .name("portal-response-listener".into())
        .spawn(move || {
            let conn = connect_bus(&address);
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.portal.Request")
                .unwrap()
                .member("Response")
                .unwrap()
                .build();
            let iter = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(32))
                .expect("failed to subscribe to Response signals");
            let _ = ready_tx.send(());
            for msg in iter.flatten() {
                let path = msg
                    .header()
                    .path()
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let Ok((code, results)) = msg
                    .body()
                    .deserialize::<(u32, HashMap<String, zvariant::OwnedValue>)>()
                else {
                    continue;
                };
                if tx
                    .send(PortalResponse {
                        path,
                        code,
                        results,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("failed to spawn response listener");
    ready_rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("portal response listener never subscribed");
    rx
}

/// Receives the next portal `Response`, failing after `RPC_TIMEOUT`.
fn next_response(rx: &mpsc::Receiver<PortalResponse>) -> PortalResponse {
    rx.recv_timeout(RPC_TIMEOUT)
        .expect("timed out waiting for portal Response signal")
}

/// A decoded `a{sv}` value, for `deserialize` calls.
type Options = HashMap<String, zvariant::Value<'static>>;

/// Calls `method` on `interface` at `org.freedesktop.portal.Desktop` with
/// `args`, returning the request handle object path.
fn call_portal(
    conn: &zbus::blocking::Connection,
    interface: &str,
    method: &str,
    args: &(impl serde::Serialize + zvariant::DynamicType),
) -> zvariant::OwnedObjectPath {
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some(interface),
            method,
            args,
        )
        .unwrap_or_else(|e| panic!("{interface}.{method} failed: {e}"));
    reply
        .body()
        .deserialize::<zvariant::OwnedObjectPath>()
        .unwrap_or_else(|e| panic!("{interface}.{method} reply was not a handle: {e}"))
}

/// Options map containing just a `handle_token`.
fn token_options(token: &str) -> Options {
    HashMap::from([(
        "handle_token".to_string(),
        zvariant::Value::from(token.to_string()),
    )])
}

#[test]
fn unscripted_open_file_returns_immediate_cancel_and_is_recorded() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "portal"}));
    let address = session_address(&mut rpc, "portal");
    let responses = response_listener(&address);
    let conn = connect_bus(&address);

    // A portal client calling FileChooser::OpenFile with a handle_token:
    // the request path is predictable before the call returns.
    let handle = call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("window-1", "Pick a file", token_options("tok1")),
    );
    let handle = handle.as_str();
    assert!(
        handle.starts_with("/org/freedesktop/portal/desktop/request/"),
        "unexpected request path {handle}"
    );
    assert!(
        handle.ends_with("/tok1"),
        "token not in request path {handle}"
    );

    // Unscripted: immediate clean cancel, never a hang.
    let response = next_response(&responses);
    assert_eq!(response.path, handle);
    assert_eq!(response.code, 1, "unscripted call should be cancelled");
    assert!(response.results.is_empty());

    // And the call was recorded verbatim in portal_history.
    let history = rpc.call("portal_history", json!({"workspace": "portal"}));
    let invocations = history["invocations"].as_array().unwrap();
    assert_eq!(invocations.len(), 1, "history: {history}");
    let entry = &invocations[0];
    assert_eq!(entry["interface"], "org.freedesktop.portal.FileChooser");
    assert_eq!(entry["method"], "OpenFile");
    assert_eq!(entry["response"], 1);
    assert_eq!(entry["scripted"], false);
    assert_eq!(entry["handle"], handle);
    assert_eq!(entry["arguments"]["title"], "Pick a file");
    assert_eq!(entry["options"]["handle_token"], "tok1");
    assert!(entry["caller"].as_str().unwrap().starts_with(':'));
    assert!(entry["caller_pid"].as_u64().is_some());

    // The `version` property answers over org.freedesktop.DBus.Properties
    // the way toolkit callers probe it.
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.portal.FileChooser", "version"),
        )
        .expect("Properties.Get version failed");
    let version = reply
        .body()
        .deserialize::<zvariant::OwnedValue>()
        .expect("version reply was not a variant");
    let version = zvariant::Value::try_from(&version).unwrap();
    assert_eq!(version, zvariant::Value::U32(4));
}

/// The `uris` array out of a response's `results` dict.
fn uris_of(response: &PortalResponse) -> Vec<String> {
    match response.results.get("uris").map(zvariant::Value::try_from) {
        Some(Ok(zvariant::Value::Array(array))) => array
            .inner()
            .iter()
            .filter_map(|v| match v {
                zvariant::Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        other => panic!("missing/invalid uris in results: {other:?}"),
    }
}

#[test]
fn scripted_file_chooser_responses_drain_fifo() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "scripted"}));
    let address = session_address(&mut rpc, "scripted");
    let responses = response_listener(&address);
    let conn = connect_bus(&address);

    // Queue two scripts: an accept carrying two URIs, then a cancel.
    let set = rpc.call(
        "portal_script_set",
        json!({
            "workspace": "scripted",
            "interface": "filechooser",
            "response": "accept",
            "uris": ["file:///tmp/a.txt", "file:///tmp/b.txt"],
        }),
    );
    assert_eq!(set["queued"], 1);
    rpc.call(
        "portal_script_set",
        json!({
            "workspace": "scripted",
            "interface": "filechooser",
            "response": "cancel",
        }),
    );

    // First OpenFile drains the accept: Response(0) with the scripted URIs.
    let handle = call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "one", token_options("t1")),
    );
    let response = next_response(&responses);
    assert_eq!(response.path, handle.as_str());
    assert_eq!(response.code, 0);
    assert_eq!(
        uris_of(&response),
        ["file:///tmp/a.txt", "file:///tmp/b.txt"]
    );

    // SaveFile shares the FileChooser queue: it drains the cancel.
    call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "SaveFile",
        &("", "two", token_options("t2")),
    );
    assert_eq!(next_response(&responses).code, 1);

    // Queue empty: the next call falls back to the unscripted cancel.
    call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "three", token_options("t3")),
    );
    assert_eq!(next_response(&responses).code, 1);

    // SaveFiles is served as well.
    rpc.call(
        "portal_script_set",
        json!({"workspace": "scripted", "interface": "filechooser", "uris": ["file:///saved"]}),
    );
    call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "SaveFiles",
        &("", "save", token_options("t4")),
    );
    let response = next_response(&responses);
    assert_eq!(response.code, 0);
    assert_eq!(uris_of(&response), ["file:///saved"]);

    // History distinguishes scripted from unscripted answers.
    let history = rpc.call("portal_history", json!({"workspace": "scripted"}));
    let calls: Vec<&Value> = history["invocations"].as_array().unwrap().iter().collect();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0]["scripted"], true);
    assert_eq!(calls[0]["response"], 0);
    assert_eq!(calls[1]["scripted"], true);
    assert_eq!(calls[1]["method"], "SaveFile");
    assert_eq!(calls[2]["scripted"], false);
}

#[test]
fn portal_script_clear_empties_the_queue() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "cleared"}));
    let address = session_address(&mut rpc, "cleared");
    let responses = response_listener(&address);
    let conn = connect_bus(&address);

    for _ in 0..2 {
        rpc.call(
            "portal_script_set",
            json!({"workspace": "cleared", "interface": "filechooser", "uris": ["file:///x"]}),
        );
    }
    let cleared = rpc.call("portal_script_clear", json!({"workspace": "cleared"}));
    assert_eq!(cleared["cleared"], 2);

    // Nothing scripted remains: the call gets the unscripted cancel.
    call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "", token_options("t")),
    );
    assert_eq!(next_response(&responses).code, 1);

    // Interface-scoped clear leaves other interfaces' queues alone.
    rpc.call(
        "portal_script_set",
        json!({"workspace": "cleared", "interface": "openuri", "response": "accept"}),
    );
    rpc.call(
        "portal_script_set",
        json!({"workspace": "cleared", "interface": "filechooser", "response": "accept"}),
    );
    let cleared = rpc.call(
        "portal_script_clear",
        json!({"workspace": "cleared", "interface": "openuri"}),
    );
    assert_eq!(cleared["cleared"], 1);
    call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "", token_options("t2")),
    );
    assert_eq!(next_response(&responses).code, 0);
}

#[test]
fn open_uri_calls_are_answered_and_recorded() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "uri"}));
    let address = session_address(&mut rpc, "uri");
    let responses = response_listener(&address);
    let conn = connect_bus(&address);

    // Script an accept for OpenURI: the call resolves with Response(0).
    rpc.call(
        "portal_script_set",
        json!({"workspace": "uri", "interface": "openuri", "response": "accept"}),
    );
    let handle = call_portal(
        &conn,
        "org.freedesktop.portal.OpenURI",
        "OpenURI",
        &("", "https://example.com/docs", token_options("u1")),
    );
    let response = next_response(&responses);
    assert_eq!(response.path, handle.as_str());
    assert_eq!(response.code, 0);

    // The requested URI landed in portal_history verbatim.
    let history = rpc.call("portal_history", json!({"workspace": "uri"}));
    let entry = &history["invocations"][0];
    assert_eq!(entry["interface"], "org.freedesktop.portal.OpenURI");
    assert_eq!(entry["method"], "OpenURI");
    assert_eq!(entry["arguments"]["uri"], "https://example.com/docs");
    assert_eq!(entry["response"], 0);

    // OpenURI::OpenFile (fd-based) takes the same interface queue: a
    // scripted reject answers it too.
    rpc.call(
        "portal_script_set",
        json!({"workspace": "uri", "interface": "openuri", "response": "cancel"}),
    );
    let file = tempfile::tempfile().expect("tempfile failed");
    let fd = zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(file));
    call_portal(
        &conn,
        "org.freedesktop.portal.OpenURI",
        "OpenFile",
        &("", fd, token_options("u2")),
    );
    assert_eq!(next_response(&responses).code, 1);

    // Unscripted OpenURI calls cancel immediately — never a host launch.
    call_portal(
        &conn,
        "org.freedesktop.portal.OpenURI",
        "OpenURI",
        &("", "https://unscripted.example", token_options("u3")),
    );
    assert_eq!(next_response(&responses).code, 1);

    // SchemeSupported reports the stub handles every scheme.
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "SchemeSupported",
            &("https", HashMap::<String, zvariant::Value>::new()),
        )
        .expect("SchemeSupported failed");
    assert!(reply.body().deserialize::<bool>().unwrap());
}

#[test]
fn portals_are_isolated_per_workspace() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "pa"}));
    rpc.call("workspace_create", json!({"id": "pb"}));
    let addr_a = session_address(&mut rpc, "pa");
    let addr_b = session_address(&mut rpc, "pb");
    assert_ne!(addr_a, addr_b);
    let responses_a = response_listener(&addr_a);
    let responses_b = response_listener(&addr_b);
    let conn_a = connect_bus(&addr_a);
    let conn_b = connect_bus(&addr_b);

    // An accept script queued on workspace A only answers on A's bus.
    rpc.call(
        "portal_script_set",
        json!({"workspace": "pa", "interface": "filechooser", "uris": ["file:///only-a"]}),
    );
    call_portal(
        &conn_b,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "", token_options("b1")),
    );
    assert_eq!(next_response(&responses_b).code, 1);
    call_portal(
        &conn_a,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "", token_options("a1")),
    );
    let response = next_response(&responses_a);
    assert_eq!(response.code, 0);
    assert_eq!(uris_of(&response), ["file:///only-a"]);

    // Each workspace's history holds only its own calls.
    let history_a = rpc.call("portal_history", json!({"workspace": "pa"}));
    assert_eq!(history_a["count"], 1);
    let history_b = rpc.call("portal_history", json!({"workspace": "pb"}));
    assert_eq!(history_b["count"], 1);
    assert_ne!(history_a["invocations"][0]["caller"], "");
}

#[test]
fn read_only_workspaces_gate_mutating_portal_verbs() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "ro", "read_only": true}));

    let err = rpc.call_err(
        "portal_script_set",
        json!({"workspace": "ro", "interface": "filechooser"}),
    );
    assert_eq!(err.code, -32003, "portal_script_set: {err:?}");
    let err = rpc.call_err("portal_script_clear", json!({"workspace": "ro"}));
    assert_eq!(err.code, -32003, "portal_script_clear: {err:?}");

    // The read-only verb is allowed.
    let history = rpc.call("portal_history", json!({"workspace": "ro"}));
    assert_eq!(history["count"], 0);
}

#[test]
fn status_surfaces_portal_health_and_activity() {
    let daemon = spawn_daemon();
    let mut rpc = RpcClient::connect(&daemon.socket_path);

    rpc.call("workspace_create", json!({"id": "health"}));
    let address = session_address(&mut rpc, "health");
    let responses = response_listener(&address);
    let conn = connect_bus(&address);

    call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "", token_options("s1")),
    );
    assert_eq!(next_response(&responses).code, 1);

    let report = rpc.call("status", json!({"workspace": "health"}));
    let portal = &report["workspaces"][0]["portal"];
    assert_eq!(portal["state"], "running", "portal report: {portal}");
    assert!(portal["calls"].as_u64().unwrap() >= 1);
    let recent = portal["recent"].as_array().unwrap();
    assert!(
        recent.iter().any(|e| e["method"] == "OpenFile"),
        "no recent portal activity in status: {portal}"
    );
    assert_eq!(report["healthy"], true, "status report: {report}");
}

// -- CLI -----------------------------------------------------------------------

/// Runs `scry --socket <socket> <args>` and captures the result.
fn scry(socket: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .expect("failed to run scry")
}

/// Runs `scry --socket <socket> --json <args>`, asserting exit 0 and
/// parsing stdout as JSON.
fn scry_json(socket: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args(args)
        .output()
        .expect("failed to run scry");
    assert!(
        out.status.success(),
        "scry --json {args:?} failed (status {}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("scry --json stdout is not JSON")
}

#[test]
fn cli_portal_verbs_drive_scripts_and_history() {
    let daemon = spawn_daemon();
    let socket = &daemon.socket_path;
    let mut rpc = RpcClient::connect(socket);

    scry_json(socket, &["workspace", "create", "cliws"]);

    // `portal-set` human output reports the queued depth.
    let out = scry(
        socket,
        &[
            "portal-set",
            "-w",
            "cliws",
            "--interface",
            "filechooser",
            "--response",
            "accept",
            "--uri",
            "file:///tmp/cli-a",
            "--uri",
            "file:///tmp/cli-b",
        ],
    );
    assert!(out.status.success(), "portal-set: {:?}", out);
    let rendered = String::from_utf8_lossy(&out.stdout);
    assert!(rendered.contains("queued"), "portal-set output: {rendered}");
    assert!(rendered.contains("filechooser"));

    // --json returns the full wire result.
    let set = scry_json(
        socket,
        &["portal-set", "-w", "cliws", "--interface", "openuri"],
    );
    assert_eq!(set["queued"], 1);
    assert_eq!(set["response"], 0);

    // A real D-Bus portal call picks up the CLI-queued script.
    let address = session_address(&mut rpc, "cliws");
    let responses = response_listener(&address);
    let conn = connect_bus(&address);
    call_portal(
        &conn,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", "", token_options("c1")),
    );
    let response = next_response(&responses);
    assert_eq!(response.code, 0);
    assert_eq!(
        uris_of(&response),
        ["file:///tmp/cli-a", "file:///tmp/cli-b"]
    );

    // `portal-history` human output narrates the invocation.
    let out = scry(socket, &["portal-history", "-w", "cliws"]);
    assert!(out.status.success());
    let rendered = String::from_utf8_lossy(&out.stdout);
    assert!(rendered.contains("OpenFile"), "portal-history: {rendered}");
    assert!(rendered.contains("accept"), "portal-history: {rendered}");

    // `portal-clear` drains what remains.
    let cleared = scry_json(socket, &["portal-clear", "-w", "cliws"]);
    assert_eq!(cleared["cleared"], 1); // only the openuri script was left
    let cleared = scry_json(socket, &["portal-clear", "-w", "cliws"]);
    assert_eq!(cleared["cleared"], 0);

    // `portal-history --limit` bounds the list.
    let history = scry_json(socket, &["portal-history", "-w", "cliws", "--limit", "1"]);
    assert_eq!(history["count"], 1);

    // Errors surface cleanly: bad interface, unknown workspace.
    let out = scry(
        socket,
        &["portal-set", "-w", "cliws", "--interface", "bogus"],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown portal interface"),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
