//! Shared helpers for integration tests: a minimal wayland-client that talks
//! to a live `scry::Compositor` over its real Unix socket.
#![allow(dead_code)] // each test binary uses a different subset of helpers

use std::{
    collections::VecDeque,
    io::Write,
    os::fd::AsFd,
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Waits for a compositor event matching `pred` and returns it.
pub fn wait_for_event(
    rx: &std::sync::mpsc::Receiver<scry::CompositorEvent>,
    pred: impl Fn(&scry::CompositorEvent) -> bool,
) -> scry::CompositorEvent {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for compositor event"));
        let ev = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for compositor event"));
        if pred(&ev) {
            return ev;
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub shm_formats: Vec<wl_shm::Format>,
    pub configure_serials: VecDeque<u32>,
    pub toplevel_configures: VecDeque<(i32, i32)>,
    pub close_requested: bool,
    pub outputs: Vec<String>,
    pub output_scales: Vec<i32>,
    pub enters: usize,
    pub frame_callbacks: usize,
}

/// A connected test client with the core globals bound.
pub struct TestClient {
    pub state: ClientState,
    pub globals: GlobalList,
    pub eq: EventQueue<ClientState>,
    pub qh: QueueHandle<ClientState>,
    pub compositor: wl_compositor::WlCompositor,
    pub shm: wl_shm::WlShm,
    pub subcompositor: Option<wayland_client::protocol::wl_subcompositor::WlSubcompositor>,
    pub wm_base: xdg_wm_base::XdgWmBase,
    pub output: Option<wl_output::WlOutput>,
}

impl TestClient {
    pub fn connect(socket_path: &Path) -> Self {
        let stream =
            UnixStream::connect(socket_path).expect("failed to connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("failed to create wayland connection");
        let (globals, eq) =
            registry_queue_init::<ClientState>(&conn).expect("registry init failed");
        let qh = eq.handle();

        let compositor = globals
            .bind::<wl_compositor::WlCompositor, _, _>(&qh, 4..=6, ())
            .expect("wl_compositor not advertised");
        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
            .expect("wl_shm not advertised");
        let wm_base = globals
            .bind::<xdg_wm_base::XdgWmBase, _, _>(&qh, 1..=6, ())
            .expect("xdg_wm_base not advertised");
        let subcompositor = globals
            .bind::<wayland_client::protocol::wl_subcompositor::WlSubcompositor, _, _>(
                &qh,
                1..=1,
                (),
            )
            .ok();
        let output = globals
            .bind::<wl_output::WlOutput, _, _>(&qh, 4..=4, ())
            .ok();

        let mut client = Self {
            state: ClientState::default(),
            globals,
            eq,
            qh,
            compositor,
            shm,
            subcompositor,
            wm_base,
            output,
        };
        client.roundtrip();
        client
    }

    /// Names of all advertised globals.
    pub fn global_names(&self) -> Vec<String> {
        self.globals
            .contents()
            .clone_list()
            .iter()
            .map(|g| g.interface.clone())
            .collect()
    }

    /// One server roundtrip, processing all pending events.
    pub fn roundtrip(&mut self) {
        self.eq
            .roundtrip(&mut self.state)
            .expect("roundtrip failed");
    }

    /// Dispatches events until `pred` holds or the timeout elapses.
    pub fn pump_until(&mut self, pred: impl Fn(&ClientState) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !pred(&self.state) {
            self.eq
                .blocking_dispatch(&mut self.state)
                .expect("dispatch failed");
            assert!(Instant::now() < deadline, "timed out pumping client events");
        }
    }

    /// Allocates a wl_shm buffer filled with a solid 0xAARRGGBB pixel.
    pub fn shm_buffer(&self, width: i32, height: i32, argb: u32) -> wl_buffer::WlBuffer {
        let mut file = tempfile::tempfile().expect("tempfile failed");
        let pixel = argb.to_le_bytes();
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&pixel);
        }
        file.write_all(&data).expect("write shm data failed");
        let pool = self
            .shm
            .create_pool(file.as_fd(), width * height * 4, &self.qh, ());
        pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        )
    }

    /// Creates an xdg toplevel, waits for the initial configure (auto-acked by
    /// the dispatch impl), attaches a buffer and commits. Returns once the
    /// commit request has been flushed to the server; callers should wait for
    /// `WindowMapped` on the event stream to observe the map.
    ///
    /// Note: the initial `surface.commit()` produces a `SurfaceCommitted`
    /// event with `window_id: None` — subscribers interested in the buffer
    /// commit should wait for the `SurfaceCommitted` that follows
    /// `WindowMapped`.
    pub fn map_toplevel(&mut self, width: i32, height: i32, app_id: &str, title: &str) -> Toplevel {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xsurface = self.wm_base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xsurface.get_toplevel(&self.qh, ());
        toplevel.set_app_id(app_id.to_string());
        toplevel.set_title(title.to_string());
        surface.commit();
        self.pump_until(|s| !s.configure_serials.is_empty());
        let buffer = self.shm_buffer(width, height, 0xff33_6699);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        self.roundtrip();
        Toplevel {
            surface,
            xsurface,
            toplevel,
            buffer,
        }
    }
}

pub struct Toplevel {
    pub surface: wl_surface::WlSurface,
    pub xsurface: xdg_surface::XdgSurface,
    pub toplevel: xdg_toplevel::XdgToplevel,
    pub buffer: wl_buffer::WlBuffer,
}

// -- dispatch boilerplate -----------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(ClientState: wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ClientState: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(ClientState: wayland_client::protocol::wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(ClientState: wayland_client::protocol::wl_subsurface::WlSubsurface);

impl Dispatch<wl_buffer::WlBuffer, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_buffer.release is expected once the compositor drops a buffer.
    }
}

impl Dispatch<wl_shm::WlShm, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_shm::WlShm,
        event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format {
            format: WEnum::Value(format),
        } = event
        {
            state.shm_formats.push(format);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::Enter { .. } = event {
            state.enters += 1;
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Name { name } => state.outputs.push(name),
            wl_output::Event::Scale { factor } => state.output_scales.push(factor),
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ClientState {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for ClientState {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.configure_serials.push_back(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.toplevel_configures.push_back((width, height));
            }
            xdg_toplevel::Event::Close => state.close_requested = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_callbacks += 1;
        }
    }
}
