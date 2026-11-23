//! Human-readable result rendering for the CLI, keyed off the typed
//! [`Verb`]/[`Outcome`] produced by `build_request` — never raw
//! method-name strings.

use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;

/// The control verb one CLI subcommand issues — the typed key every
/// per-verb decision (wire method, human formatting) is made on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `workspace_create`.
    WorkspaceCreate,
    /// `workspace_list`.
    WorkspaceList,
    /// `workspace_close`.
    WorkspaceClose,
    /// `output_add`.
    OutputAdd,
    /// `output_list`.
    OutputList,
    /// `output_remove`.
    OutputRemove,
    /// `spawn_app`.
    SpawnApp,
    /// `list_windows`.
    ListWindows,
    /// `place_window`.
    PlaceWindow,
    /// `capture_window`.
    CaptureWindow,
    /// `capture_workspace`.
    CaptureWorkspace,
    /// `get_damage`.
    GetDamage,
    /// `click`.
    Click,
    /// `drag`.
    Drag,
    /// `type_text`.
    TypeText,
    /// `key_combo`.
    KeyCombo,
    /// `clipboard_get`.
    ClipboardGet,
    /// `clipboard_set`.
    ClipboardSet,
    /// `wait_for_window`.
    WaitForWindow,
    /// `wait_for_commit`.
    WaitForCommit,
    /// `wait_for_idle`.
    WaitForIdle,
    /// `get_changes_since`.
    GetChangesSince,
    /// `mark_set`.
    MarkSet,
    /// `mark_list`.
    MarkList,
    /// `status`.
    Status,
}

impl Verb {
    /// The JSON-RPC method name sent on the wire.
    pub fn name(self) -> &'static str {
        match self {
            Self::WorkspaceCreate => "workspace_create",
            Self::WorkspaceList => "workspace_list",
            Self::WorkspaceClose => "workspace_close",
            Self::OutputAdd => "output_add",
            Self::OutputList => "output_list",
            Self::OutputRemove => "output_remove",
            Self::SpawnApp => "spawn_app",
            Self::ListWindows => "list_windows",
            Self::PlaceWindow => "place_window",
            Self::CaptureWindow => "capture_window",
            Self::CaptureWorkspace => "capture_workspace",
            Self::GetDamage => "get_damage",
            Self::Click => "click",
            Self::Drag => "drag",
            Self::TypeText => "type_text",
            Self::KeyCombo => "key_combo",
            Self::ClipboardGet => "clipboard_get",
            Self::ClipboardSet => "clipboard_set",
            Self::WaitForWindow => "wait_for_window",
            Self::WaitForCommit => "wait_for_commit",
            Self::WaitForIdle => "wait_for_idle",
            Self::GetChangesSince => "get_changes_since",
            Self::MarkSet => "mark_set",
            Self::MarkList => "mark_list",
            Self::Status => "status",
        }
    }
}

/// How a verb's result reaches the user beyond the standard formatters.
pub enum Outcome {
    /// Render with the per-verb human formatter / raw JSON.
    Default,
    /// `capture_*`: decode `data` and write it to a file when `--out` is
    /// given; otherwise print the summary (human) or full JSON (`--json`).
    Capture {
        /// The `--out` file target, if any.
        out: Option<PathBuf>,
    },
    /// `clipboard_get`: write the decoded payload bytes to stdout in
    /// human mode (pipe-friendly, like `wl-paste`).
    ClipboardGet,
}

/// The human-readable rendering of one verb's result envelope.
pub fn render(verb: Verb, r: &Value, outcome: &Outcome) -> String {
    let seq = r["seq"].as_u64();
    let at = seq.map(|s| format!(" (seq {s})")).unwrap_or_default();
    match verb {
        Verb::WorkspaceCreate => {
            let id = r["workspace"]["id"].as_str().unwrap_or("?");
            if r["created"].as_bool().unwrap_or(false) {
                format!("workspace \"{id}\" created{at}")
            } else {
                format!("workspace \"{id}\" already exists{at}")
            }
        }
        Verb::WorkspaceList => {
            let workspaces = r["workspaces"].as_array();
            match workspaces {
                None => return "no workspaces".to_string(),
                Some(list) if list.is_empty() => return "no workspaces".to_string(),
                _ => {}
            }
            workspaces
                .unwrap()
                .iter()
                .map(|ws| {
                    format!(
                        "{}  state={}  seq={}  apps={}  socket={}",
                        ws["id"].as_str().unwrap_or("?"),
                        ws["state"].as_str().unwrap_or("?"),
                        ws["seq"],
                        ws["processes"].as_array().map(Vec::len).unwrap_or(0),
                        ws["socket_path"].as_str().unwrap_or("?"),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Verb::WorkspaceClose => {
            format!(
                "workspace {:?} closed{at}",
                r["closed"].as_str().unwrap_or("?")
            )
        }
        Verb::OutputAdd => {
            let o = &r["output"];
            format!(
                "output {:?} {}x{}@{:.3}Hz scale={} at ({},{}){}",
                o["name"].as_str().unwrap_or("?"),
                o["size"][0],
                o["size"][1],
                o["refresh_millihertz"].as_f64().unwrap_or(0.0) / 1000.0,
                o["scale"],
                o["geometry"]["x"],
                o["geometry"]["y"],
                at,
            )
        }
        Verb::OutputList => {
            let outputs = r["outputs"].as_array();
            match outputs {
                None => return "no outputs".to_string(),
                Some(list) if list.is_empty() => return "no outputs".to_string(),
                _ => {}
            }
            outputs
                .unwrap()
                .iter()
                .map(|o| {
                    format!(
                        "{}  {}x{}@{:.3}Hz  scale={}  geometry=({},{ } {}x{})",
                        o["name"].as_str().unwrap_or("?"),
                        o["size"][0],
                        o["size"][1],
                        o["refresh_millihertz"].as_f64().unwrap_or(0.0) / 1000.0,
                        o["scale"],
                        o["geometry"]["x"],
                        o["geometry"]["y"],
                        o["geometry"]["width"],
                        o["geometry"]["height"],
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Verb::OutputRemove => {
            format!(
                "output {:?} removed{at}",
                r["removed"].as_str().unwrap_or("?")
            )
        }
        Verb::SpawnApp => {
            let proc = &r["process"];
            format!(
                "spawned {:?} pid {} {}{}",
                proc["program"].as_str().unwrap_or("?"),
                r["pid"],
                proc["args"]
                    .as_array()
                    .map(|args| format!("args={args:?}"))
                    .unwrap_or_default(),
                at,
            )
        }
        Verb::ListWindows => {
            let mut lines: Vec<String> = Vec::new();
            if let Some(windows) = r["windows"].as_array() {
                for w in windows {
                    lines.push(format_window(w));
                }
            }
            if let Some(popups) = r["popups"].as_array() {
                for p in popups {
                    lines.push(format!(
                        "popup  surface={}  window={}  geometry=({},{} {}x{})",
                        p["surface_id"],
                        p["window_id"],
                        p["geometry"]["x"],
                        p["geometry"]["y"],
                        p["geometry"]["width"],
                        p["geometry"]["height"],
                    ));
                }
            }
            if lines.is_empty() {
                "no windows".to_string()
            } else {
                lines.join("\n")
            }
        }
        Verb::PlaceWindow => {
            let w = &r["window"];
            let g = &w["geometry"];
            format!(
                "window {} placed at ({},{}) {}x{}{}",
                w["id"], g["x"], g["y"], g["width"], g["height"], at
            )
        }
        Verb::CaptureWindow | Verb::CaptureWorkspace => {
            let subject = if verb == Verb::CaptureWindow {
                format!("window {}", r["window_id"])
            } else {
                format!("output {:?}", r["output"].as_str().unwrap_or("?"))
            };
            let size = result_data_len(r);
            let written = match outcome {
                Outcome::Capture { out: Some(path) } => {
                    format!(", wrote {}", path.display())
                }
                _ => " (use --out FILE to save)".to_string(),
            };
            format!(
                "captured {subject}: {}x{} {}, {size} bytes{written}{at}",
                r["width"],
                r["height"],
                r["format"].as_str().unwrap_or("?"),
            )
        }
        Verb::GetDamage => {
            let entries = r["damage"].as_array();
            match entries {
                None => return "no damage".to_string(),
                Some(list) if list.is_empty() => return "no damage".to_string(),
                _ => {}
            }
            entries
                .unwrap()
                .iter()
                .map(|e| {
                    format!(
                        "seq={}  window={}  {}",
                        e["seq"],
                        e["window_id"],
                        e["damage"]
                            .as_array()
                            .map(|rects| rects
                                .iter()
                                .map(|rc| format!(
                                    "({},{} {}x{})",
                                    rc["x"], rc["y"], rc["width"], rc["height"]
                                ))
                                .collect::<Vec<_>>()
                                .join(" "))
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Verb::Click => format!(
            "clicked {} at ({},{}) x{}{}",
            r["button"], r["x"], r["y"], r["clicked"], at
        ),
        Verb::Drag => format!(
            "dragged {} from ({},{}) to ({},{}) in {} steps{}",
            r["button"], r["from"]["x"], r["from"]["y"], r["to"]["x"], r["to"]["y"], r["steps"], at
        ),
        Verb::TypeText => format!(
            "typed {} characters{}",
            r["text"].as_str().map(str::len).unwrap_or(0),
            at
        ),
        Verb::KeyCombo => {
            let mods = r["modifiers"]
                .as_array()
                .map(|mods| {
                    mods.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .unwrap_or_default();
            let combo = if mods.is_empty() {
                r["key"].as_str().unwrap_or("?").to_string()
            } else {
                format!("{mods}+{}", r["key"].as_str().unwrap_or("?"))
            };
            format!("pressed {combo}{at}")
        }
        Verb::ClipboardSet => format!(
            "set {} selection: {}, {} bytes{}",
            r["selection"].as_str().unwrap_or("clipboard"),
            r["mime"].as_str().unwrap_or("?"),
            r["size"],
            at
        ),
        Verb::WaitForWindow => {
            let w = &r["window"];
            format!("mapped {}", format_window(w))
        }
        Verb::WaitForCommit => {
            let window = r["window_id"]
                .as_u64()
                .map(|id| format!(" on window {id}"))
                .unwrap_or_default();
            format!("commit observed{window} at seq {}", r["seq"])
        }
        Verb::WaitForIdle => {
            format!("workspace idle after {}ms quiet{}", r["quiet_ms"], at)
        }
        Verb::GetChangesSince => serde_json::to_string_pretty(r).unwrap_or_default(),
        Verb::MarkSet => {
            let mark = &r["mark"];
            format!(
                "mark {:?} set at seq {}",
                mark["name"].as_str().unwrap_or("?"),
                mark["seq"]
            )
        }
        Verb::MarkList => {
            let marks = r["marks"].as_array();
            match marks {
                None => return "no marks".to_string(),
                Some(list) if list.is_empty() => return "no marks".to_string(),
                _ => {}
            }
            marks
                .unwrap()
                .iter()
                .map(|m| {
                    let meta = match m.get("metadata") {
                        Some(Value::Null) | None => String::new(),
                        Some(meta) => format!("  {meta}"),
                    };
                    format!(
                        "seq={}  {}{}",
                        m["seq"],
                        m["name"].as_str().unwrap_or("?"),
                        meta
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Verb::Status => format_status(r),
        Verb::ClipboardGet => serde_json::to_string_pretty(r).unwrap_or_default(),
    }
}

/// One `list_windows` line.
fn format_window(w: &Value) -> String {
    let g = &w["geometry"];
    let mut parts = vec![
        format!("id={}", w["id"]),
        format!(
            "app_id={}",
            w["app_id"]
                .as_str()
                .map(|s| format!("\"{s}\""))
                .unwrap_or("-".into())
        ),
        format!(
            "title={}",
            w["title"]
                .as_str()
                .map(|s| format!("\"{s}\""))
                .unwrap_or("-".into())
        ),
        format!("at ({},{}) {}x{}", g["x"], g["y"], g["width"], g["height"]),
        format!("scale={}", w["scale"]),
    ];
    if let Some(pid) = w["pid"].as_u64() {
        parts.push(format!("pid={pid}"));
    }
    if w["activated"].as_bool().unwrap_or(false) {
        parts.push("activated".to_string());
    }
    parts.join("  ")
}

/// The byte length of a capture's `data` field (base64 → decoded length).
fn result_data_len(r: &Value) -> usize {
    r["data"]
        .as_str()
        .and_then(|b64| BASE64.decode(b64).ok())
        .map(|b| b.len())
        .unwrap_or(0)
}

/// Human rendering of `status`: the narrative plus findings and next steps.
fn format_status(r: &Value) -> String {
    let mut out = String::new();
    out.push_str(r["running"].as_str().unwrap_or("scry daemon"));
    if let Some(workspaces) = r["workspaces"].as_array() {
        for ws in workspaces {
            out.push_str(&format!(
                "\n  {}: {} windows, compositor {}, {}",
                ws["id"].as_str().unwrap_or("?"),
                ws["window_count"],
                ws["compositor_responsive"]
                    .as_bool()
                    .map(|ok| if ok { "responsive" } else { "UNRESPONSIVE" })
                    .unwrap_or("?"),
                ws["healthy"]
                    .as_bool()
                    .map(|ok| if ok { "healthy" } else { "UNHEALTHY" })
                    .unwrap_or("?"),
            ));
        }
    }
    let findings = r["unhealthy"].as_array();
    match findings {
        Some(list) if !list.is_empty() => {
            out.push_str("\nunhealthy findings:");
            for finding in list {
                out.push_str(&format!("\n  - {}", finding.as_str().unwrap_or("?")));
            }
        }
        _ => out.push_str("\nno unhealthy findings"),
    }
    if let Some(steps) = r["next_steps"].as_array() {
        out.push_str("\nnext steps:");
        for step in steps {
            out.push_str(&format!("\n  - {}", step.as_str().unwrap_or("?")));
        }
    }
    out
}
