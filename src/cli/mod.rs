//! The thin, stateless `clap` CLI: argument definitions, dispatch onto the
//! daemon's JSON-RPC verbs, and human/JSON result rendering.
//!
//! The CLI holds no workspace state of its own: every invocation resolves
//! the control socket, issues one verb and exits. `scry daemon` runs the
//! daemon in the foreground; client commands auto-start it when the
//! *default* socket is not listening (overridable with `--autostart` /
//! `--no-autostart`).

mod client;
mod format;

use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

use format::{Outcome, Verb};
use scry::daemon::{DaemonConfig, DaemonHandle};

/// Process exit status: the call succeeded.
const EXIT_OK: i32 = 0;
/// Exit status: the daemon rejected the request (JSON-RPC error).
const EXIT_ERROR: i32 = 1;
/// Exit status: CLI-side misuse — matches clap's own parse-error code.
const EXIT_USAGE: i32 = 2;
/// Exit status: the daemon could not be reached.
const EXIT_NO_DAEMON: i32 = 3;

/// `scry` — thin CLI over the scry daemon's Unix-socket JSON-RPC API.
#[derive(Parser)]
#[command(
    name = "scry",
    version,
    about = "headless Wayland compositor control CLI",
    long_about = "A thin, stateless proxy over the scry daemon's Unix-socket JSON-RPC API.\n\
                  Every command connects, issues one verb and exits; the daemon owns\n\
                  all workspace, compositor and process state."
)]
struct Cli {
    /// Daemon control socket.
    /// Defaults to $XDG_RUNTIME_DIR/scry/control.sock.
    #[arg(long, global = true, env = "SCRY_SOCKET", value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Daemon state directory, used by `daemon` and by auto-start.
    /// Defaults to $XDG_STATE_HOME/scry.
    #[arg(long, global = true, env = "SCRY_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,

    /// Print the raw JSON-RPC result instead of human-readable output.
    #[arg(long, global = true)]
    json: bool,

    /// Auto-start the daemon when the socket is not listening
    /// (default when --socket is not given).
    #[arg(long, global = true, overrides_with = "no_autostart")]
    autostart: bool,

    /// Never auto-start the daemon; fail if the socket is not listening.
    #[arg(long, global = true)]
    no_autostart: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the scry daemon in the foreground.
    Daemon,
    /// Create, list and close workspaces.
    #[command(subcommand)]
    Workspace(WorkspaceCommands),
    /// Add, list and remove a workspace's virtual outputs.
    #[command(subcommand)]
    Output(OutputCommands),
    /// Spawn a managed app inside a workspace.
    Spawn(SpawnArgs),
    /// List a workspace's windows and popups.
    Windows(WorkspaceArg),
    /// Place a window at exact logical coordinates (and optionally resize).
    Place(PlaceArgs),
    /// Capture one window, occlusion-free, as PNG or WebP.
    CaptureWindow(CaptureWindowArgs),
    /// Capture a workspace output, composited in z-order.
    CaptureWorkspace(CaptureWorkspaceArgs),
    /// Report damaged regions, optionally filtered by window and seq.
    Damage(DamageArgs),
    /// Move the pointer and press/release a button.
    Click(ClickArgs),
    /// Press a button, sweep through interpolated motions, release.
    Drag(DragArgs),
    /// Inject UTF-8 text, optionally focusing a window first.
    Type(TypeArgs),
    /// Tap a key while holding modifiers, optionally focusing a window.
    Key(KeyArgs),
    /// Read or write a workspace's clipboard/primary selection.
    #[command(subcommand)]
    Clipboard(ClipboardCommands),
    /// Block on event-driven workspace conditions.
    #[command(subcommand)]
    Wait(WaitCommands),
    /// Fused compositor + AT-SPI2 accessibility tree snapshot.
    Tree(TreeArgs),
    /// Compact structured diff of everything that changed since a seq.
    Changes(ChangesArgs),
    /// Named marks on a workspace's seq timeline.
    #[command(subcommand)]
    Mark(MarkCommands),
    /// Queue a scripted response for the workspace portal stub.
    PortalSet(PortalSetArgs),
    /// Drop queued portal scripts (all, or one interface's).
    PortalClear(PortalClearArgs),
    /// List recorded portal invocations.
    PortalHistory(PortalHistoryArgs),
    /// Diagnostic status of the daemon and its workspaces.
    Status(StatusArgs),
}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// Create a workspace (idempotent on matching options).
    Create(WorkspaceCreateArgs),
    /// List live workspaces.
    List,
    /// Close a workspace and terminate its apps.
    Close(IdArg),
}

#[derive(Subcommand)]
enum OutputCommands {
    /// Add or reconfigure a virtual output.
    Add(OutputAddArgs),
    /// List a workspace's virtual outputs.
    List(WorkspaceArg),
    /// Withdraw a virtual output by name.
    Remove(OutputRemoveArgs),
}

#[derive(Subcommand)]
enum ClipboardCommands {
    /// Read a selection's payload (decoded bytes on stdout).
    Get(ClipboardGetArgs),
    /// Install a compositor-provided selection.
    Set(ClipboardSetArgs),
}

#[derive(Subcommand)]
enum WaitCommands {
    /// Block until a matching window maps in the workspace.
    Window(WaitWindowArgs),
    /// Block until a commit lands at seq > --after-seq.
    Commit(WaitCommitArgs),
    /// Block until the workspace is event-free for --quiet-ms.
    Idle(WaitIdleArgs),
}

#[derive(Subcommand)]
enum MarkCommands {
    /// Attach a named mark at the workspace's current seq.
    Set(MarkSetArgs),
    /// List every named mark on the timeline.
    List(WorkspaceArg),
}

// -- argument structs -----------------------------------------------------------

/// Flags every workspace-scoped verb shares.
#[derive(Args)]
struct WorkspaceArg {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
}

#[derive(Args)]
struct IdArg {
    /// The workspace id.
    id: String,
}

#[derive(Args)]
struct WorkspaceCreateArgs {
    /// Agent-chosen workspace id (1–64 chars of [A-Za-z0-9_-]).
    id: String,
    /// Reject mutating verbs (fixed at creation time).
    #[arg(long)]
    read_only: bool,
    /// Exempt the workspace from idle auto-cleanup.
    #[arg(long)]
    keep_open: bool,
    /// Idle TTL in milliseconds (overrides the daemon default).
    #[arg(long, value_name = "MS")]
    idle_ttl_ms: Option<u64>,
}

#[derive(Args)]
struct OutputAddArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Output name (`wl_output.name`).
    #[arg(long)]
    name: String,
    /// Physical mode width in pixels.
    #[arg(long)]
    width: i32,
    /// Physical mode height in pixels.
    #[arg(long)]
    height: i32,
    /// Refresh in millihertz (default 60000 = 60Hz).
    #[arg(long, value_name = "MHZ")]
    refresh_millihertz: Option<i32>,
    /// Advertised scale factor (default 1.0).
    #[arg(long)]
    scale: Option<f64>,
    /// Logical X position (default 0).
    #[arg(long, default_value_t = 0)]
    x: i32,
    /// Logical Y position (default 0).
    #[arg(long, default_value_t = 0)]
    y: i32,
}

#[derive(Args)]
struct OutputRemoveArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Output name to remove.
    #[arg(long)]
    name: String,
}

#[derive(Args)]
struct SpawnArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Working directory for the child.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Set an environment variable on top of the default profile
    /// (repeatable, KEY=VALUE).
    #[arg(long, value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Remove a variable from the default environment profile
    /// (repeatable).
    #[arg(long, value_name = "KEY")]
    unset_env: Vec<String>,
    /// The executable to run (PATH-resolved unless it contains '/').
    /// Options must come before PROGRAM; everything after it is passed
    /// through verbatim.
    program: String,
    /// Arguments passed to the program.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct PlaceArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Window id (from `scry windows`).
    #[arg(long)]
    window: u64,
    /// Logical X position.
    #[arg(long)]
    x: i32,
    /// Logical Y position.
    #[arg(long)]
    y: i32,
    /// New logical width (must be given with --height).
    #[arg(long)]
    width: Option<i32>,
    /// New logical height (must be given with --width).
    #[arg(long)]
    height: Option<i32>,
}

#[derive(Args)]
struct CaptureWindowArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Window id (from `scry windows`).
    #[arg(long)]
    window: u64,
    /// Image encoding.
    #[arg(long, value_enum, default_value_t = ImageFormat::Png)]
    format: ImageFormat,
    /// Write the decoded image to FILE instead of a summary/base64.
    #[arg(short, long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(Args)]
struct CaptureWorkspaceArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Virtual output to capture (default: the primary output).
    #[arg(long, value_name = "NAME")]
    output: Option<String>,
    /// Image encoding.
    #[arg(long, value_enum, default_value_t = ImageFormat::Png)]
    format: ImageFormat,
    /// Write the decoded image to FILE instead of a summary/base64.
    #[arg(short, long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ImageFormat {
    Png,
    Webp,
}

impl ImageFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }
}

#[derive(Args)]
struct DamageArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Restrict to damage on this window.
    #[arg(long)]
    window: Option<u64>,
    /// Only damage newer than this seq.
    #[arg(long, value_name = "SEQ", default_value_t = 0)]
    after_seq: u64,
}

#[derive(Args)]
struct ClickArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// X coordinate (output space, or window-local with --window).
    #[arg(long, allow_hyphen_values = true)]
    x: f64,
    /// Y coordinate (output space, or window-local with --window).
    #[arg(long, allow_hyphen_values = true)]
    y: f64,
    /// Make x/y window-local relative to this window's top-left corner.
    #[arg(long)]
    window: Option<u64>,
    /// Button name (left/right/middle/side/extra/forward/back/task) or a
    /// raw evdev button code.
    #[arg(long)]
    button: Option<String>,
    /// Press/release repetitions (1–64, default 1).
    #[arg(long)]
    count: Option<u32>,
}

#[derive(Args)]
struct DragArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Start point "x,y" (output space, or window-local with
    /// --from-window).
    #[arg(long, value_parser = parse_point)]
    from: (f64, f64),
    /// End point "x,y" (output space, or window-local with --to-window).
    #[arg(long, value_parser = parse_point)]
    to: (f64, f64),
    /// Interpret --from as window-local relative to this window.
    #[arg(long)]
    from_window: Option<u64>,
    /// Interpret --to as window-local relative to this window.
    #[arg(long)]
    to_window: Option<u64>,
    /// Button name or raw evdev code (default left).
    #[arg(long)]
    button: Option<String>,
    /// Motion events between press and release (default 8).
    #[arg(long)]
    steps: Option<u32>,
}

#[derive(Args)]
struct TypeArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// The UTF-8 text to inject.
    text: String,
    /// Focus this window before typing.
    #[arg(long)]
    window: Option<u64>,
}

#[derive(Args)]
struct KeyArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// The key to tap (XKB keysym name, e.g. "a", "Return", "Tab").
    key: String,
    /// Held modifiers (repeatable: ctrl, shift, alt, super...).
    #[arg(long = "mod", value_name = "MOD")]
    modifiers: Vec<String>,
    /// Focus this window before pressing.
    #[arg(long)]
    window: Option<u64>,
}

#[derive(Args)]
struct ClipboardGetArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// "clipboard" (default) or "primary".
    #[arg(long)]
    selection: Option<String>,
    /// The MIME type to negotiate (default: first offered).
    #[arg(long)]
    mime: Option<String>,
}

#[derive(Args)]
struct ClipboardSetArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// UTF-8 text payload.
    data: Option<String>,
    /// Base64-encoded binary payload (exclusive with DATA).
    #[arg(long, conflicts_with = "data", value_name = "BASE64")]
    data_base64: Option<String>,
    /// "clipboard" (default) or "primary".
    #[arg(long)]
    selection: Option<String>,
    /// The MIME type to offer (default text/plain).
    #[arg(long)]
    mime: Option<String>,
}

#[derive(Args)]
struct WaitWindowArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Exact xdg_toplevel.app_id match.
    #[arg(long)]
    app_id: Option<String>,
    /// Title match (substring by default).
    #[arg(long)]
    title: Option<String>,
    /// "substring" (default) or "exact".
    #[arg(long, value_parser = ["substring", "exact"])]
    title_match: Option<String>,
    /// Match the window owned by this client pid.
    #[arg(long)]
    pid: Option<u32>,
    /// Overall bound in milliseconds (default 30000).
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
}

#[derive(Args)]
struct WaitCommitArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Only commits strictly newer than this seq resolve the wait.
    #[arg(long, value_name = "SEQ", default_value_t = 0)]
    after_seq: u64,
    /// Restrict to commits on this window's surfaces.
    #[arg(long)]
    window: Option<u64>,
    /// Overall bound in milliseconds (default 30000).
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
}

#[derive(Args)]
struct WaitIdleArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Event-free quiet window in milliseconds (default 250).
    #[arg(long, value_name = "MS")]
    quiet_ms: Option<u64>,
    /// Overall bound in milliseconds (default 30000).
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
}

#[derive(Args)]
struct TreeArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Restrict the snapshot to one window id.
    #[arg(long)]
    window: Option<u64>,
    /// Bound the a11y capture in milliseconds.
    #[arg(long, value_name = "MS")]
    deadline_ms: Option<u64>,
}

#[derive(Args)]
struct ChangesArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// The caller's last-seen seq; the diff covers (seq, current].
    seq: u64,
}

#[derive(Args)]
struct MarkSetArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// The caller-chosen label (1–128 chars).
    name: String,
    /// Arbitrary JSON metadata echoed back by diffs and mark list.
    #[arg(long, value_parser = parse_json)]
    metadata: Option<Value>,
}

#[derive(Args)]
struct PortalSetArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Portal interface to script: "filechooser" or "openuri".
    #[arg(long)]
    interface: String,
    /// The scripted response: "accept" (default), "cancel", "close",
    /// or a raw code 0-2.
    #[arg(long, value_name = "RESPONSE")]
    response: Option<String>,
    /// A file:// URI to return on accept (repeatable).
    #[arg(long = "uri", value_name = "URI")]
    uris: Vec<String>,
}

#[derive(Args)]
struct PortalClearArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Only clear this interface's queue (default: all interfaces).
    #[arg(long)]
    interface: Option<String>,
}

#[derive(Args)]
struct PortalHistoryArgs {
    /// The workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: String,
    /// Only return the newest N invocations.
    #[arg(long, value_name = "N")]
    limit: Option<u64>,
}

#[derive(Args)]
struct StatusArgs {
    /// Restrict the workspace detail to one workspace id.
    #[arg(short, long, value_name = "ID")]
    workspace: Option<String>,
}

// -- entry point ------------------------------------------------------------------

/// Parses argv and runs one CLI invocation; returns the process exit code.
pub fn run() -> i32 {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Daemon => run_daemon(&cli),
        _ => run_verb(&cli),
    }
}

/// `scry daemon`: binds the control socket and serves until SIGINT/SIGTERM.
///
/// Stdout carries a single machine-readable readiness line; diagnostics go
/// to stderr. Every write tolerates EPIPE so a parent that grabbed the
/// readiness line and went away cannot panic the daemon.
fn run_daemon(cli: &Cli) -> i32 {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("scry=info")),
        )
        .init();

    let socket = cli.socket.clone().unwrap_or_else(client::default_socket);
    let state_dir = cli
        .state_dir
        .clone()
        .unwrap_or_else(client::default_state_dir);
    let config = DaemonConfig::new(socket, state_dir);
    let handle = match DaemonHandle::spawn(config) {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("scry: failed to start daemon: {err}");
            return EXIT_ERROR;
        }
    };
    let _ = writeln!(
        std::io::stdout(),
        "scry daemon listening on {}",
        handle.socket_path().display()
    );
    let _ = std::io::stdout().flush();
    eprintln!("scry daemon state dir: {}", handle.state_dir().display());
    match handle.wait() {
        Ok(()) => {
            eprintln!("scry daemon stopped");
            EXIT_OK
        }
        Err(err) => {
            eprintln!("scry daemon stopped with error: {err}");
            EXIT_ERROR
        }
    }
}

/// The dispatch half: connect, issue one verb, render the result.
fn run_verb(cli: &Cli) -> i32 {
    let socket = cli.socket.clone().unwrap_or_else(client::default_socket);
    let (verb, params, outcome) = match build_request(&cli.command) {
        Ok(request) => request,
        Err(msg) => {
            eprintln!("scry: {msg}");
            return EXIT_USAGE;
        }
    };

    let mut rpc = match connect(cli, &socket) {
        Ok(rpc) => rpc,
        Err(code) => return code,
    };
    match rpc.call(verb.name(), params) {
        Ok(result) => report(&outcome, verb, &result, cli.json),
        Err(client::Failure::Rpc(err)) => {
            if cli.json {
                eprintln!(
                    "{}",
                    json!({"error": {
                        "code": err.code,
                        "message": err.message,
                        "data": err.data,
                    }})
                );
            } else {
                eprintln!(
                    "scry: {} failed: {} (code {})",
                    verb.name(),
                    err.message,
                    err.code
                );
                if let Some(data) = &err.data {
                    eprintln!("  {data}");
                }
            }
            EXIT_ERROR
        }
        Err(client::Failure::Transport(err)) => {
            eprintln!(
                "scry: lost connection to daemon at {socket}: {err}",
                socket = socket.display()
            );
            EXIT_NO_DAEMON
        }
        Err(client::Failure::Protocol(msg)) => {
            eprintln!("scry: daemon protocol error: {msg}");
            EXIT_NO_DAEMON
        }
    }
}

/// Connects to the daemon, auto-starting it when allowed.
fn connect(cli: &Cli, socket: &Path) -> Result<client::Client, i32> {
    match client::Client::connect(socket) {
        Ok(rpc) => return Ok(rpc),
        Err(err) => {
            // Auto-start is on by default for the default socket; an
            // explicit --socket/--state-dir targets a private daemon and
            // fails fast instead. --autostart/--no-autostart override both.
            let autostart = cli.autostart || (!cli.no_autostart && cli.socket.is_none());
            if !autostart {
                eprintln!(
                    "scry: cannot connect to daemon at {}: {err}",
                    socket.display()
                );
                eprintln!("scry: start one with `scry daemon` or omit --socket to auto-start");
                return Err(EXIT_NO_DAEMON);
            }
        }
    }
    let state_dir = cli
        .state_dir
        .clone()
        .unwrap_or_else(client::default_state_dir);
    if let Err(msg) = client::autostart_daemon(socket, &state_dir) {
        eprintln!("scry: {msg}");
        return Err(EXIT_NO_DAEMON);
    }
    client::Client::connect(socket).map_err(|err| {
        eprintln!(
            "scry: daemon auto-started but still unreachable at {}: {err}",
            socket.display()
        );
        EXIT_NO_DAEMON
    })
}

// -- request building --------------------------------------------------------------

/// Maps a parsed subcommand onto `(verb, params, outcome)`; CLI-side
/// validation failures become `Err(usage message)`.
fn build_request(command: &Commands) -> Result<(Verb, Value, Outcome), String> {
    use Commands as C;
    let ws = |w: &String| json!({"workspace": w});
    Ok(match command {
        C::Daemon => unreachable!("daemon is dispatched before run_verb"),
        C::Workspace(WorkspaceCommands::Create(args)) => {
            let mut params = json!({
                "id": args.id,
                "read_only": args.read_only,
                "keep_open": args.keep_open,
            });
            if let Some(ttl) = args.idle_ttl_ms {
                params["idle_ttl_ms"] = json!(ttl);
            }
            (Verb::WorkspaceCreate, params, Outcome::Default)
        }
        C::Workspace(WorkspaceCommands::List) => (Verb::WorkspaceList, json!({}), Outcome::Default),
        C::Workspace(WorkspaceCommands::Close(args)) => (
            Verb::WorkspaceClose,
            json!({"workspace": args.id}),
            Outcome::Default,
        ),
        C::Output(OutputCommands::Add(args)) => {
            let mut params = ws(&args.workspace);
            params["name"] = json!(args.name);
            params["width"] = json!(args.width);
            params["height"] = json!(args.height);
            params["position"] = json!([args.x, args.y]);
            if let Some(scale) = args.scale {
                params["scale"] = json!(scale);
            }
            if let Some(mhz) = args.refresh_millihertz {
                params["refresh_millihertz"] = json!(mhz);
            }
            (Verb::OutputAdd, params, Outcome::Default)
        }
        C::Output(OutputCommands::List(args)) => {
            (Verb::OutputList, ws(&args.workspace), Outcome::Default)
        }
        C::Output(OutputCommands::Remove(args)) => {
            let mut params = ws(&args.workspace);
            params["name"] = json!(args.name);
            (Verb::OutputRemove, params, Outcome::Default)
        }
        C::Spawn(args) => {
            let mut env = serde_json::Map::new();
            for kv in &args.env {
                let Some((key, value)) = kv.split_once('=') else {
                    return Err(format!("--env expects KEY=VALUE, got {kv:?}"));
                };
                env.insert(key.to_string(), Value::String(value.to_string()));
            }
            for key in &args.unset_env {
                env.insert(key.clone(), Value::Null);
            }
            let mut params = ws(&args.workspace);
            params["program"] = json!(args.program);
            if !args.args.is_empty() {
                params["args"] = json!(args.args);
            }
            if let Some(cwd) = &args.cwd {
                params["cwd"] = json!(cwd);
            }
            if !env.is_empty() {
                params["env"] = Value::Object(env);
            }
            (Verb::SpawnApp, params, Outcome::Default)
        }
        C::Windows(args) => (Verb::ListWindows, ws(&args.workspace), Outcome::Default),
        C::Place(args) => {
            if args.width.is_some() != args.height.is_some() {
                return Err("--width and --height must be given together".into());
            }
            let mut params = ws(&args.workspace);
            params["window"] = json!(args.window);
            params["x"] = json!(args.x);
            params["y"] = json!(args.y);
            if let (Some(w), Some(h)) = (args.width, args.height) {
                params["width"] = json!(w);
                params["height"] = json!(h);
            }
            (Verb::PlaceWindow, params, Outcome::Default)
        }
        C::CaptureWindow(args) => (
            Verb::CaptureWindow,
            json!({
                "workspace": args.workspace,
                "window": args.window,
                "format": args.format.name(),
            }),
            Outcome::Capture {
                out: args.out.clone(),
            },
        ),
        C::CaptureWorkspace(args) => {
            let mut params = ws(&args.workspace);
            params["format"] = json!(args.format.name());
            if let Some(output) = &args.output {
                params["output"] = json!(output);
            }
            (
                Verb::CaptureWorkspace,
                params,
                Outcome::Capture {
                    out: args.out.clone(),
                },
            )
        }
        C::Damage(args) => {
            let mut params = ws(&args.workspace);
            params["after_seq"] = json!(args.after_seq);
            if let Some(window) = args.window {
                params["window"] = json!(window);
            }
            (Verb::GetDamage, params, Outcome::Default)
        }
        C::Click(args) => {
            let mut params = ws(&args.workspace);
            params["x"] = json!(args.x);
            params["y"] = json!(args.y);
            if let Some(window) = args.window {
                params["window"] = json!(window);
            }
            if let Some(button) = &args.button {
                params["button"] = button_param(button);
            }
            if let Some(count) = args.count {
                params["count"] = json!(count);
            }
            (Verb::Click, params, Outcome::Default)
        }
        C::Drag(args) => {
            let mut params = ws(&args.workspace);
            params["from"] = point_json(args.from, args.from_window);
            params["to"] = point_json(args.to, args.to_window);
            if let Some(button) = &args.button {
                params["button"] = button_param(button);
            }
            if let Some(steps) = args.steps {
                params["steps"] = json!(steps);
            }
            (Verb::Drag, params, Outcome::Default)
        }
        C::Type(args) => {
            let mut params = ws(&args.workspace);
            params["text"] = json!(args.text);
            if let Some(window) = args.window {
                params["window"] = json!(window);
            }
            (Verb::TypeText, params, Outcome::Default)
        }
        C::Key(args) => {
            let mut params = ws(&args.workspace);
            params["key"] = json!(args.key);
            if !args.modifiers.is_empty() {
                params["modifiers"] = json!(args.modifiers);
            }
            if let Some(window) = args.window {
                params["window"] = json!(window);
            }
            (Verb::KeyCombo, params, Outcome::Default)
        }
        C::Clipboard(ClipboardCommands::Get(args)) => {
            let mut params = ws(&args.workspace);
            if let Some(selection) = &args.selection {
                params["selection"] = json!(selection);
            }
            if let Some(mime) = &args.mime {
                params["mime"] = json!(mime);
            }
            (Verb::ClipboardGet, params, Outcome::ClipboardGet)
        }
        C::Clipboard(ClipboardCommands::Set(args)) => {
            if args.data.is_none() && args.data_base64.is_none() {
                return Err("clipboard set needs a DATA argument or --data-base64".into());
            }
            let mut params = ws(&args.workspace);
            if let Some(data) = &args.data {
                params["data"] = json!(data);
            }
            if let Some(b64) = &args.data_base64 {
                params["data_base64"] = json!(b64);
            }
            if let Some(selection) = &args.selection {
                params["selection"] = json!(selection);
            }
            if let Some(mime) = &args.mime {
                params["mime"] = json!(mime);
            }
            (Verb::ClipboardSet, params, Outcome::Default)
        }
        C::Wait(WaitCommands::Window(args)) => {
            if args.app_id.is_none() && args.title.is_none() && args.pid.is_none() {
                return Err("wait window needs at least one of --app-id, --title or --pid".into());
            }
            let mut params = ws(&args.workspace);
            if let Some(app_id) = &args.app_id {
                params["app_id"] = json!(app_id);
            }
            if let Some(title) = &args.title {
                params["title"] = json!(title);
            }
            if let Some(m) = &args.title_match {
                params["title_match"] = json!(m);
            }
            if let Some(pid) = args.pid {
                params["pid"] = json!(pid);
            }
            if let Some(ms) = args.timeout_ms {
                params["timeout_ms"] = json!(ms);
            }
            (Verb::WaitForWindow, params, Outcome::Default)
        }
        C::Wait(WaitCommands::Commit(args)) => {
            let mut params = ws(&args.workspace);
            params["after_seq"] = json!(args.after_seq);
            if let Some(window) = args.window {
                params["window"] = json!(window);
            }
            if let Some(ms) = args.timeout_ms {
                params["timeout_ms"] = json!(ms);
            }
            (Verb::WaitForCommit, params, Outcome::Default)
        }
        C::Wait(WaitCommands::Idle(args)) => {
            let mut params = ws(&args.workspace);
            if let Some(ms) = args.quiet_ms {
                params["quiet_ms"] = json!(ms);
            }
            if let Some(ms) = args.timeout_ms {
                params["timeout_ms"] = json!(ms);
            }
            (Verb::WaitForIdle, params, Outcome::Default)
        }
        C::Tree(args) => {
            let mut params = ws(&args.workspace);
            if let Some(window) = args.window {
                params["window_id"] = json!(window);
            }
            if let Some(ms) = args.deadline_ms {
                params["deadline_ms"] = json!(ms);
            }
            (Verb::GetTree, params, Outcome::Default)
        }
        C::Changes(args) => (
            Verb::GetChangesSince,
            json!({"workspace": args.workspace, "seq": args.seq}),
            Outcome::Default,
        ),
        C::Mark(MarkCommands::Set(args)) => {
            let mut params = ws(&args.workspace);
            params["name"] = json!(args.name);
            if let Some(metadata) = &args.metadata {
                params["metadata"] = metadata.clone();
            }
            (Verb::MarkSet, params, Outcome::Default)
        }
        C::Mark(MarkCommands::List(args)) => {
            (Verb::MarkList, ws(&args.workspace), Outcome::Default)
        }
        C::PortalSet(args) => {
            let mut params = ws(&args.workspace);
            params["interface"] = json!(args.interface);
            if let Some(response) = &args.response {
                // A bare number goes out as the raw code; names pass
                // through for the daemon to resolve.
                params["response"] = match response.parse::<u32>() {
                    Ok(code) => json!(code),
                    Err(_) => json!(response),
                };
            }
            if !args.uris.is_empty() {
                params["uris"] = json!(args.uris);
            }
            (Verb::PortalScriptSet, params, Outcome::Default)
        }
        C::PortalClear(args) => {
            let mut params = ws(&args.workspace);
            if let Some(interface) = &args.interface {
                params["interface"] = json!(interface);
            }
            (Verb::PortalScriptClear, params, Outcome::Default)
        }
        C::PortalHistory(args) => {
            let mut params = ws(&args.workspace);
            if let Some(limit) = args.limit {
                params["limit"] = json!(limit);
            }
            (Verb::PortalHistory, params, Outcome::Default)
        }
        C::Status(args) => {
            let params = match &args.workspace {
                Some(id) => json!({"workspace": id}),
                None => json!({}),
            };
            (Verb::Status, params, Outcome::Default)
        }
    })
}

/// A `--from/--to` point: `x,y` plus an optional window making it
/// window-local.
fn point_json(point: (f64, f64), window: Option<u64>) -> Value {
    let mut value = json!({"x": point.0, "y": point.1});
    if let Some(window) = window {
        value["window"] = json!(window);
    }
    value
}

/// Parses `--from/--to` `"x,y"` values.
fn parse_point(arg: &str) -> Result<(f64, f64), String> {
    let (x, y) = arg
        .split_once(',')
        .ok_or_else(|| format!("expected \"x,y\", got {arg:?}"))?;
    let x: f64 = x.parse().map_err(|_| format!("bad x in {arg:?}"))?;
    let y: f64 = y.parse().map_err(|_| format!("bad y in {arg:?}"))?;
    Ok((x, y))
}

/// `--metadata` for `mark set`: any JSON value.
fn parse_json(arg: &str) -> Result<Value, String> {
    serde_json::from_str(arg).map_err(|err| format!("invalid JSON {arg:?}: {err}"))
}

/// `--button`: an evdev code when numeric, else a friendly name.
fn button_param(arg: &str) -> Value {
    match arg.parse::<u32>() {
        Ok(code) => json!(code),
        Err(_) => json!(arg),
    }
}

// -- result rendering -----------------------------------------------------------------

/// Prints a successful result per the outcome mode; returns the exit code.
fn report(outcome: &Outcome, verb: Verb, result: &Value, json_mode: bool) -> i32 {
    // File output applies in both modes before anything is printed.
    if let Outcome::Capture { out: Some(path) } = outcome {
        let bytes = result["data"]
            .as_str()
            .and_then(|b64| BASE64.decode(b64).ok())
            .unwrap_or_default();
        if let Err(err) = std::fs::write(path, &bytes) {
            eprintln!("scry: failed to write {}: {err}", path.display());
            return EXIT_ERROR;
        }
    }
    if json_mode {
        println!("{}", serde_json::to_string(result).unwrap_or_default());
        return EXIT_OK;
    }
    match outcome {
        Outcome::ClipboardGet => {
            match result["data_base64"]
                .as_str()
                .and_then(|b64| BASE64.decode(b64).ok())
            {
                // Raw bytes on stdout: `scry clipboard get` behaves like
                // `wl-paste` for scripts.
                Some(bytes) => {
                    let _ = std::io::stdout().write_all(&bytes);
                    let _ = std::io::stdout().flush();
                }
                None => {
                    eprintln!("scry: clipboard_get response carried no data_base64");
                    return EXIT_ERROR;
                }
            }
            EXIT_OK
        }
        _ => {
            println!("{}", format::render(verb, result, outcome));
            EXIT_OK
        }
    }
}
