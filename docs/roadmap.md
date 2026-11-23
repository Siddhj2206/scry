# scry Roadmap

`scry` is a headless Rust Wayland compositor built for AI computer use, automated
multi-app UI/UX testing, and live browser-based viewing with human-in-the-loop
takeover.

## 1. Overview & Core Principles

- **Structured-first, pixels second.** The primary interface to running apps is
  structured data: window metadata from the compositor plus accessibility trees
  over AT-SPI2. Pixel capture is a fallback and a verification tool, not the
  default observation channel.
- **Event-driven, never `sleep()`.** All synchronization is driven by
  `wl_surface.commit` and damage tracking. Waits block on commits, damage,
  window state changes, or idle conditions — never on wall-clock timeouts alone.
- **Headless by default.** `wl_shm` software compositing runs anywhere with no
  GPU, no display, no seat. GPU acceleration is an optional feature, never a
  requirement.
- **Human-visible & interruptible.** Every session can be streamed to a browser,
  and a human can take control from an agent at any time through an explicit
  takeover state machine.
- **Unmodified native Wayland apps.** Targets run the same binaries and toolkits
  they ship — no LD_PRELOAD tricks, no patched clients, no virtual X server
  pretending to be a display.

## 2. Architecture & Crate Stack

### Compositor core

- `smithay 0.7` with `default-features = false`, enabling `wayland_frontend`
  and `desktop`. Pure-Rust `wayland-backend`; `libxkbcommon` is the only
  mandatory C dependency.

### Software compositing & frame encoding

- `wl_shm` buffer access composited by a pure-Rust `tiny-skia` compositor.
- Optional `renderer_pixman` feature when `libpixman-1` is available.
- Pure-Rust `image` crate for `png` and `webp` frame encoding.

### Accessibility

- Private session D-Bus (`dbus-daemon` or `dbus-broker-launch`) plus
  `at-spi-bus-launcher` / `at-spi2-registryd`, spawned per session so AT-SPI
  traffic never touches the user's real bus.
- Consumed via `atspi 0.30` + `zbus 5.5`, fused with compositor geometry so
  every accessible node maps to real on-screen pixels.

### Control plane

- `tokio` runtime bridged to the `calloop` compositor thread via typed
  command/event channels.
- Unix socket JSON-RPC control API, a `clap` CLI, and an `rmcp 3.11` MCP server
  (stdio + streamable HTTP over `axum 0.8`).

## 3. Phase 1 — Core Headless Compositor, Control API & AT-SPI2

### M1 — Headless `smithay` compositor core

**Deliverables**

- `wl_compositor`, `wl_subcompositor`, `wl_shm`
- `xdg_shell`: toplevel + popup + decoration
- `wl_output`: virtual outputs with HiDPI scale
- `wl_seat`: pointer + keyboard
- `wl_data_device` + `zwp_primary_selection_v1` clipboard & primary selection
- `zwp_text_input_v3`: direct Unicode text injection with XKB-keysym fallback
- `xdg_activation_v1`: cross-app focus handoffs
- `viewporter` / `fractional_scale`
- `tiny-skia` software compositor producing frames from damaged regions
- Commit/damage event bus: every `wl_surface.commit` emits an event carrying
  surface id, serial, and damaged region
- Virtual frame clock pacing `wl_surface.frame` callbacks: realtime 60 Hz by
  default, plus a paused/stepped mode for deterministic frame-stepped tests
  (no stall, no spin)
- `SO_PEERCRED` client PID tracking on every `wl_client` — the foundation for
  exact window↔AT-SPI2 fusion in M3
- Occlusion-free per-window capture: each window's surface, subsurfaces, and
  popups composited directly, independent of overlap or off-screen placement
- Command-to-frame correlation: monotonic commit/frame sequence numbers on
  every event and ack
- Three-layer input semantics — physical keycodes, semantic/IME text, and
  compositor-global shortcuts — with strict validation at each layer

**Acceptance criteria**

- `weston-simple-shm` and a GTK4 app map, draw, and produce commit events
  headlessly.
- Captured frames match expected contents for a known test pattern.
- Damage events fire exactly on commit with correct regions.

### M2 — Workspaces, deterministic layout & Control API

**Deliverables**

- Multi-workspace isolation with deterministic window placement. Workspaces
  are lightweight idempotent handles, not heavyweight shared sessions:
  agent-chosen ids (e.g. `auth-flow`), options fixed at creation (`readOnly`,
  `keepOpen`), persisted metadata, idle auto-cleanup, and `client_pid` tagging
  so a dead client's in-flight work cancels cleanly.
- Unix socket JSON-RPC API + `clap` CLI covering:
  - `workspace_create`, `output_add`
  - `spawn_app`, `list_windows`, `place_window`
  - `capture_window` / `capture_workspace`
  - `get_damage`
  - Targeted input: `click`, `drag`, `type_text`, `key_combo`,
    `clipboard_get` / `clipboard_set`
  - Event-driven waits: `wait_for_window`, `wait_for_commit`, `wait_for_idle`
  - `status`: diagnostic verb narrating what is running, what is unhealthy,
    and what to do next
- Default Wayland + `wl_shm` + AT-SPI env profile injected by `spawn_app`
  (overridable): `GDK_BACKEND=wayland`, `GSK_RENDERER=cairo`,
  `QT_QPA_PLATFORM=wayland`, `MOZ_ENABLE_WAYLAND=1`,
  `ELECTRON_OZONE_PLATFORM_HINT=wayland`, `SDL_VIDEODRIVER=wayland`,
  `GTK_A11Y=atspi`, `ACCESSIBILITY_ENABLED=1`
- `get_changes_since(seq)`: structured diffs for low-token agent observation
- Command-id ↔ frame-seq acks plus named marks on the frame timeline
- CLI and MCP server are thin, stateless proxies over a single daemon that
  owns all state

**Acceptance criteria**

- A scripted multi-app scenario (two apps, two workspaces) runs start-to-finish
  with zero sleeps — every step gated on commit/window events.
- `place_window` is reproducible: same calls produce identical geometry and
  captures across runs.
- Synthetic input is delivered to the correct surface and verified via the
  app's observable response.

### M3 — Private D-Bus + AT-SPI2 fusion

**Deliverables**

- Per-session private D-Bus and AT-SPI registry lifecycle managed by scry
- `get_tree`: fused view of compositor window metadata + accessibility tree
  with absolute screen coordinates per node
- `click_node` and node-targeted actions
- `SO_PEERCRED` PID ↔ D-Bus `GetConnectionUnixProcessID` correlation for exact
  multi-instance window↔tree fusion
- Staleness-checked AT-SPI2 refs (`e12` = `(path, role, exact name)` triple):
  a ref whose node changed or disappeared fails fast with a descriptive
  re-snapshot hint
- Bounded tree snapshots: depth, node-count, and deadline caps degrade
  truncated subtrees to inline `- ... <reason>` notes
- `ROLE_PASSWORD_TEXT` values redacted by default in every snapshot
- Snapshot `find` (matching nodes with context lines) and LCS-based tree diffs
  for low-token agent observation
- Scripted XDG Desktop Portal stub (`FileChooser`, `OpenURI`) on the private
  session bus so agents/tests answer native file/URI dialogs deterministically
  instead of clicking a system picker
- Graceful fallback to pixel capture when a toolkit exposes a sparse or absent
  accessibility tree

**Acceptance criteria**

- GTK4 app exposes its widget tree through `get_tree` with correct bounds.
- `click_node` drives a real button click, verified by app behavior.
- Apps with no AT-SPI tree still return window geometry and capture pixels.

### M4 — MCP server

**Deliverables**

- `rmcp 3.11` server over stdio and streamable HTTP (`axum 0.8`)
- Full Control API surface exposed as MCP tools for AI agents
- Model the MCP UX and token-efficiency patterns closely on `anomalyco/browse`
  (thin stateless stdio/HTTP proxy over the daemon, `clientInfo`-aware typed
  tools vs. compact `status` / `workspaces` / `execute` escape hatch,
  inspect → act → verify workflow guidance, bounded/redacted snapshots,
  descriptive stale-ref errors, and diagnostic `status`)

**Acceptance criteria**

- An MCP client over stdio can spawn an app, wait for its window, read its
  AT-SPI tree, click a node, and capture the result.
- Same flow works over the HTTP transport.

## 4. Phase 2 — Headless GPU, Live Browser Viewer, Human Takeover & Flight Recorder

### M5 — Optional GPU acceleration (`zwp_linux_dmabuf_v1`) & rich input

- Headless EGL/GBM render-node path (`/dev/dri/renderD128`) with `llvmpipe`
  software fallback and `zwp_linux_dmabuf_v1` import so GPU-first apps
  (Chromium/Electron, Tauri/WebKitGTK, Firefox WebRender, Ghostty, wgpu)
  render smoothly before live streaming and recording.
- Strictly behind an opt-in Cargo feature (`dmabuf` / `renderer_gles`); the
  `wl_shm`/`tiny-skia` path remains the default zero-GPU baseline for CI and
  tests.
- Rich trackpad & touch input: expose `scroll` (`wl_pointer.axis` vertical and
  horizontal) over JSON-RPC/CLI/MCP, and add `zwp_pointer_gestures_v1` (swipe,
  pinch, hold) plus optional `wl_touch` in the compositor and Control API.
- Acceptance: GPU-backed native Wayland apps map, render, and capture cleanly
  when enabled, while default `cargo test` builds without GBM/EGL headers;
  scroll and trackpad gesture events deliver accurately to Wayland clients.

### M6 — Live streaming viewer

- Damaged-region / whole-workspace streaming over WebSocket / WebTransport to a
  minimal browser viewer.
- Separate post-compositing overlay pass (keycaps, pointer-press ripples)
  rendered into the live viewer and recordings only — excluded from agent
  captures and damage events.
- Acceptance: sub-100ms latency from commit to browser frame on localhost;
  viewer renders damage-only updates correctly.

### M7 — Human-in-the-loop takeover

- Explicit three-state control machine:
  - **Agent Active** — agent drives input, human observes.
  - **Human Takeover** — browser input forwards to the compositor; agent input
    is rejected.
  - **Hand Back** — control returns to the agent together with a fresh state
    summary (windows, tree, recent actions) so it resumes with accurate context.
- Blocking `handoff(reason)`: marks the workspace `WAIT` in the viewer and
  resumes the agent only on an explicit human click.
- `demonstrate` mode: records human clicks/typing in the live viewer into
  replayable steps, coalescing typing and redacting passwords.
- Acceptance: takeover is atomic, visible in the viewer, and every transition
  is logged; agent receives the state summary on hand-back.

### M8 — UI Flight Recorder

- Compact session trace recording inputs, window-tree changes, AT-SPI actions,
  and dirty frames.
- Frame-by-frame replay of a recorded session in the browser viewer.
- Named marks on the recording timeline and compact film-strip / stacked-crop
  failure artifacts.
- Off-thread worker encoding so PNG/video encoding never stalls the
  compositor loop.
- In-process Matroska/WebM muxing so video recording works without `ffmpeg`
  at runtime (`ffmpeg` stays an optional encoder).
- Acceptance: a recorded session replays deterministically; traces are small
  enough to attach to test failures.

## 5. Phase 3 — Ecosystem

### M9 — Optional rootless XWayland

- Rootless XWayland bridge for legacy X11-only applications.

### M10 — Pluto integration

- Native integration with Pluto for headless GUI CI and computer-use
  containers.

## 6. Verification & Testing Strategy

### Integration tests (`tests/`)

- Deterministic Rust `wayland-client` integration tests: real protocol clients
  against the running compositor, asserting on protocol state, commit/damage
  events, and captures.
- Fast, zero external GUI dependencies — the default `cargo test` suite.

### End-to-end tests

- Real unmodified Wayland applications in both host CI and the lightweight dev
  `Containerfile`:
  - `zenity` and GTK4 with `GSK_RENDERER=cairo` (software rendering)
  - `wl-copy` / `wl-paste` for clipboard
  - `foot` terminal
  - `weston-simple-shm`
  - Firefox as the heavyweight real-world target
- Every E2E scenario is event-driven: waits on commits, window states, and idle
  — never sleeps.
