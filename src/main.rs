//! `scry` binary: runs a headless compositor and prints how to reach it.
//!
//! M1 keeps this minimal — a real CLI with subcommands arrives with the
//! control-plane milestone. For now it prints the socket environment a client
//! needs and stays alive until interrupted.

fn main() -> scry::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("scry=info")),
        )
        .init();

    let compositor = scry::Compositor::spawn()?;
    let handle = compositor.handle();

    println!("scry headless compositor running");
    println!();
    println!("  socket:      {}", handle.socket_path().display());
    println!("  runtime dir: {}", handle.runtime_dir().display());
    println!();
    println!("connect clients with:");
    println!(
        "  export XDG_RUNTIME_DIR={}",
        handle.runtime_dir().display()
    );
    println!(
        "  export WAYLAND_DISPLAY={}",
        handle.socket_name().to_string_lossy()
    );
    println!();
    println!("press Ctrl-C to stop");

    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])
    .expect("failed to install signal handlers");
    let _ = signals.forever().next();
    println!("shutting down");
    Ok(())
    // `compositor` drops here: shutdown + thread join + socket cleanup.
}
