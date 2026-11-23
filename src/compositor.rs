//! [`Compositor`]: the owning lifecycle object for a headless scry instance.

use std::{sync::mpsc::channel, thread::JoinHandle, time::Duration};

use smithay::reexports::{calloop::channel, wayland_server::ListeningSocket};
use tempfile::TempDir;
use tracing::info;

use crate::{
    error::{Error, Result},
    handle::{Command, CompositorHandle},
    state,
};

/// Name of the socket created inside the isolated runtime directory.
const SOCKET_NAME: &str = "wayland-0";

/// How long startup waits for the compositor thread to report readiness.
const INIT_TIMEOUT: Duration = Duration::from_secs(30);

/// A running headless Wayland compositor.
///
/// `spawn` creates an isolated runtime directory containing a real
/// `WAYLAND_DISPLAY` unix socket, starts the smithay event loop on a dedicated
/// thread, and returns once every wayland global is advertised. Dropping the
/// value shuts the compositor down and joins the thread.
pub struct Compositor {
    handle: CompositorHandle,
    join: Option<JoinHandle<()>>,
    // Kept last so the directory outlives the thread's socket cleanup.
    _runtime_dir: TempDir,
}

impl Compositor {
    /// Starts a new isolated headless compositor.
    ///
    /// The instance binds `<runtime_dir>/wayland-0`, where `<runtime_dir>` is a
    /// fresh `0700` temporary directory, so any number of instances may run in
    /// parallel without colliding with each other or a host session.
    pub fn spawn() -> Result<Self> {
        let runtime_dir = tempfile::Builder::new()
            .prefix("scry-")
            .tempdir()
            .map_err(Error::Io)?;
        // XDG_RUNTIME_DIR semantics require 0700.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(runtime_dir.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(Error::Io)?;
        }

        let socket_path = runtime_dir.path().join(SOCKET_NAME);
        let socket =
            ListeningSocket::bind_absolute(socket_path.clone()).map_err(Error::SocketBind)?;
        info!(socket = %socket_path.display(), "wayland socket bound");

        let (command_tx, command_rx) = channel::channel::<Command>();
        let (init_tx, init_rx) = channel::<std::result::Result<(), String>>();
        let join = std::thread::Builder::new()
            .name("scry-compositor".to_string())
            .spawn(move || state::run(socket, command_rx, init_tx))
            .map_err(Error::Io)?;

        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = join.join();
                return Err(Error::Backend(err));
            }
            Err(_) => {
                let _ = join.join();
                return Err(Error::Disconnected);
            }
        }

        let handle = CompositorHandle::new(
            command_tx,
            socket_path,
            std::ffi::OsString::from(SOCKET_NAME),
            runtime_dir.path().to_owned(),
        );
        Ok(Self {
            handle,
            join: Some(join),
            _runtime_dir: runtime_dir,
        })
    }

    /// The typed control/observation handle for this compositor.
    pub fn handle(&self) -> &CompositorHandle {
        &self.handle
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // Ask the loop to stop, then join. If the loop is already gone the
        // command send fails fast and join still returns promptly.
        let _ = self.handle.shutdown();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl std::fmt::Debug for Compositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compositor")
            .field("socket_path", &self.handle.socket_path())
            .finish_non_exhaustive()
    }
}
