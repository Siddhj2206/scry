//! `scry` binary: a thin, stateless `clap` CLI over the daemon's
//! Unix-socket JSON-RPC control API.
//!
//! `scry daemon` runs the control daemon in the foreground; every other
//! subcommand connects to a running daemon, issues one verb and exits.

mod cli;

fn main() {
    std::process::exit(cli::run());
}
