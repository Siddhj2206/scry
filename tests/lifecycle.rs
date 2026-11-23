//! Startup/shutdown lifecycle: isolated socket, core globals, clean teardown.

mod common;

use common::TestClient;
use scry::Compositor;

#[test]
fn binds_unique_socket_and_advertises_core_globals() {
    let compositor = Compositor::spawn().expect("compositor failed to start");
    let handle = compositor.handle();

    // A real socket file exists inside an isolated runtime directory.
    assert!(handle.socket_path().exists());
    assert!(handle.socket_path().starts_with(handle.runtime_dir()));

    let client = TestClient::connect(handle.socket_path());
    let names = client.global_names();
    for interface in [
        "wl_compositor",
        "wl_subcompositor",
        "wl_shm",
        "xdg_wm_base",
        "wl_output",
    ] {
        assert!(
            names.contains(&interface.to_string()),
            "missing global {interface}"
        );
    }

    // wl_shm advertises the protocol-mandatory formats.
    assert!(
        client
            .state
            .shm_formats
            .contains(&wayland_client::protocol::wl_shm::Format::Argb8888)
    );
    assert!(
        client
            .state
            .shm_formats
            .contains(&wayland_client::protocol::wl_shm::Format::Xrgb8888)
    );
}

#[test]
fn instances_get_isolated_sockets() {
    let a = Compositor::spawn().unwrap();
    let b = Compositor::spawn().unwrap();
    assert_ne!(a.handle().socket_path(), b.handle().socket_path());
    assert_ne!(a.handle().runtime_dir(), b.handle().runtime_dir());
    // Both are independently connectable.
    let _ca = TestClient::connect(a.handle().socket_path());
    let _cb = TestClient::connect(b.handle().socket_path());
}

#[test]
fn shutdown_stops_thread_and_cleans_up() {
    let compositor = Compositor::spawn().unwrap();
    let handle = compositor.handle();
    let socket_path = handle.socket_path().to_owned();
    let runtime_dir = handle.runtime_dir().to_owned();
    assert!(socket_path.exists());

    let reply = handle.shutdown().expect("shutdown failed");
    let _ = reply.seq; // reply carries a sequence number; exact value is unspecified

    // After the reply the loop is stopped; dropping joins the thread and the
    // tempdir removes socket + lockfile.
    drop(compositor);
    assert!(!socket_path.exists());
    assert!(!runtime_dir.exists());
}

#[test]
fn dropping_compositor_shuts_down() {
    let socket_path;
    let runtime_dir;
    {
        let compositor = Compositor::spawn().unwrap();
        socket_path = compositor.handle().socket_path().to_owned();
        runtime_dir = compositor.handle().runtime_dir().to_owned();
    }
    assert!(!socket_path.exists());
    assert!(!runtime_dir.exists());
}
