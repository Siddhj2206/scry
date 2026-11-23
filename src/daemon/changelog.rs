//! Per-workspace change history: a bounded, sequence-indexed log of
//! compositor events plus an unbounded named-mark table, backing the
//! `get_changes_since`, `mark_set` and `mark_list` control verbs.
//!
//! The log retains the newest `capacity` events. When older entries are
//! evicted the `complete_from` watermark advances, so a
//! `get_changes_since(seq)` whose `seq` predates the retained history can
//! be flagged `truncated` with a resync hint instead of silently dropping
//! transitions.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;
use serde_json::{Value, json};

use crate::{CompositorEvent, Rect, Seq};

/// Default number of retained compositor events per workspace.
pub const DEFAULT_CAPACITY: usize = 4096;

/// A named mark on a workspace's sequence timeline (`mark_set`).
#[derive(Debug, Clone, Serialize)]
pub struct Mark {
    /// The caller-chosen label.
    pub name: String,
    /// The timeline position the mark occupies.
    pub seq: Seq,
    /// When the mark was set (ms since the Unix epoch).
    pub set_at_ms: u64,
    /// Optional caller-supplied metadata, echoed back verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// One workspace's bounded event history plus its named marks.
///
/// Marks live outside the bounded event buffer: `mark_list` always reports
/// every mark ever set, and `marks_added` in diffs stays complete even when
/// the event log has truncated.
pub struct ChangeLog {
    capacity: usize,
    /// Retained events in arrival (= seq) order.
    events: VecDeque<CompositorEvent>,
    /// The highest evicted seq: the log provably covers every change at
    /// seqs strictly greater than this. `0` while nothing was dropped.
    complete_from: Seq,
    /// Every named mark, in seq order.
    marks: Vec<Mark>,
}

impl ChangeLog {
    /// An empty log retaining the newest `capacity` events (minimum 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            events: VecDeque::new(),
            complete_from: 0,
            marks: Vec::new(),
        }
    }

    /// Appends a compositor event, evicting the oldest entries beyond
    /// `capacity` and advancing the truncation watermark.
    pub fn record(&mut self, event: &CompositorEvent) {
        self.events.push_back(event.clone());
        while self.events.len() > self.capacity {
            if let Some(evicted) = self.events.pop_front() {
                self.complete_from = self.complete_from.max(evicted.seq());
            }
        }
    }

    /// Attaches a named mark at `seq` and returns it.
    ///
    /// Callers must hold the log's lock while allocating `seq` so pushes
    /// stay in seq order.
    pub fn set_mark(
        &mut self,
        name: String,
        metadata: Option<Value>,
        seq: Seq,
        set_at_ms: u64,
    ) -> Mark {
        let mark = Mark {
            name,
            seq,
            set_at_ms,
            metadata,
        };
        self.marks.push(mark.clone());
        mark
    }

    /// Every mark on the timeline, in seq order (`mark_list`).
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    /// The seq of the oldest retained event (`None` while the log is empty).
    fn oldest_retained_seq(&self) -> Option<Seq> {
        self.events.front().map(CompositorEvent::seq)
    }

    /// Folds every retained event in `(since, current_seq]` into a compact,
    /// deduplicated structured diff.
    ///
    /// Sets `truncated` (plus a `resync_hint`) when `since` precedes the
    /// oldest retained history: the caller cannot tell what it missed and
    /// should take a fresh full snapshot.
    pub fn diff(&self, since: Seq, current_seq: Seq) -> Value {
        let truncated = since < self.complete_from;

        // Windows: deduplicated to each window's latest observed state in
        // the range — a window mapped then updated reports the newest info
        // under `mapped` only, and a window mapped then closed reports
        // under `closed` only.
        let mut mapped: BTreeMap<u64, Value> = BTreeMap::new();
        let mut updated: BTreeMap<u64, Value> = BTreeMap::new();
        let mut closed: BTreeSet<u64> = BTreeSet::new();
        let mut focus_changed = false;
        let mut focused: Option<u64> = None;
        let mut popups_mapped: BTreeMap<u64, Value> = BTreeMap::new();
        let mut popups_closed: BTreeSet<u64> = BTreeSet::new();
        // Damage coalesced per window (key `None` = commits on surfaces not
        // yet attributed to a window, e.g. pre-map commits).
        let mut damage: BTreeMap<Option<u64>, Vec<Rect>> = BTreeMap::new();
        let mut clipboard: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();

        for event in self.events.iter().filter(|e| e.seq() > since) {
            match event {
                CompositorEvent::WindowMapped { window, .. } => {
                    closed.remove(&window.id.0);
                    updated.remove(&window.id.0);
                    mapped.insert(window.id.0, json!(window));
                }
                CompositorEvent::WindowUpdated { window, .. } => {
                    if let std::collections::btree_map::Entry::Occupied(mut slot) =
                        mapped.entry(window.id.0)
                    {
                        slot.insert(json!(window));
                    } else {
                        updated.insert(window.id.0, json!(window));
                    }
                }
                CompositorEvent::WindowClosed { window_id, .. } => {
                    mapped.remove(&window_id.0);
                    updated.remove(&window_id.0);
                    closed.insert(window_id.0);
                }
                CompositorEvent::WindowFocused { window_id, .. } => {
                    focus_changed = true;
                    focused = window_id.map(|id| id.0);
                }
                CompositorEvent::PopupMapped { popup, .. } => {
                    popups_closed.remove(&popup.surface_id.0);
                    popups_mapped.insert(popup.surface_id.0, json!(popup));
                }
                CompositorEvent::PopupClosed { surface_id, .. } => {
                    popups_mapped.remove(&surface_id.0);
                    popups_closed.insert(surface_id.0);
                }
                CompositorEvent::SurfaceCommitted {
                    window_id,
                    damage: rects,
                    ..
                } => {
                    let entry = damage.entry(window_id.map(|id| id.0)).or_default();
                    for rect in rects {
                        if !entry.contains(rect) {
                            entry.push(*rect);
                        }
                    }
                }
                CompositorEvent::ClipboardUpdated {
                    selection,
                    mime_types,
                    ..
                } => {
                    clipboard.insert(
                        match selection {
                            crate::SelectionKind::Clipboard => "clipboard",
                            crate::SelectionKind::Primary => "primary",
                        },
                        mime_types.clone(),
                    );
                }
            }
        }

        let marks_added: Vec<&Mark> = self.marks.iter().filter(|m| m.seq > since).collect();
        let mut diff = json!({
            "since_seq": since,
            "current_seq": current_seq,
            "truncated": truncated,
            "oldest_retained_seq": self.oldest_retained_seq(),
            "windows": {
                "mapped": mapped.into_values().collect::<Vec<_>>(),
                "updated": updated.into_values().collect::<Vec<_>>(),
                "closed": closed.iter().collect::<Vec<_>>(),
            },
            "focus": {
                "changed": focus_changed,
                "window_id": focused,
            },
            "popups": {
                "mapped": popups_mapped.into_values().collect::<Vec<_>>(),
                "closed": popups_closed.iter().collect::<Vec<_>>(),
            },
            "clipboard": clipboard
                .iter()
                .map(|(selection, mime_types)| json!({
                    "selection": selection,
                    "mime_types": mime_types,
                    "cleared": mime_types.is_empty(),
                }))
                .collect::<Vec<_>>(),
            "damage": damage
                .iter()
                .map(|(window_id, rects)| json!({
                    "window_id": window_id,
                    "rects": rects,
                }))
                .collect::<Vec<_>>(),
            "marks_added": marks_added,
        });
        if truncated {
            diff["resync_hint"] = json!(format!(
                "seq {since} is older than the retained change history (history is complete \
                 for seq > {}): take a fresh snapshot (list_windows, output_list, status) and \
                 resume diffs from current_seq {current_seq}",
                self.complete_from
            ));
        }
        diff
    }
}
