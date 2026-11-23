//! The canonical line representation of a [`TreeSnapshot`] (M3.5): a
//! compact, deterministic `[eN] role "name" (x,y WxH)` line per node,
//! indented by depth, with inline `- ... <reason>` truncation markers —
//! the same shape `scry tree` prints, plus `states=[...]` so `tree_diff`
//! catches pure state flips (checked, focused) that leave role, name and
//! bounds untouched.
//!
//! `tree_find` filters the node-bearing lines and returns surrounding
//! context; `tree_diff` runs a line-oriented LCS over two snapshots'
//! renderings.

use crate::{
    Rect, WindowId,
    daemon::a11y::{NodeValue, TreeNode, TreeSnapshot},
};

/// One canonical line: its rendered text plus the node it came from (or
/// `None` for window headers and `- ...` marker lines).
#[derive(Debug)]
pub struct CanonLine {
    /// The rendered line text, indentation included.
    pub text: String,
    /// The window block the line belongs to.
    pub window_id: WindowId,
    /// Whether this line opens a window block (context extraction never
    /// crosses a header).
    pub is_window_header: bool,
    /// Node details when the line renders a tree node.
    pub node: Option<LineNode>,
}

/// The searchable fields of a node-bearing canonical line — a clone of
/// the snapshot node's observable surface (text/value already redacted).
#[derive(Debug, Clone)]
pub struct LineNode {
    /// The node's `eN` ref.
    pub node_ref: String,
    /// Its accessible role name.
    pub role: String,
    /// Its exact accessible name.
    pub name: String,
    /// AT-SPI state names present on the node.
    pub states: Vec<String>,
    /// Fused absolute workspace bounds.
    pub bounds: Option<Rect>,
    /// `Text` contents (`[REDACTED]` on password nodes).
    pub text: Option<String>,
    /// `Value` reading (`[REDACTED]` on password nodes).
    pub value: Option<NodeValue>,
    /// `Value::Text` contents (`[REDACTED]` on password nodes).
    pub value_text: Option<String>,
    /// Whether the node is a redacted `ROLE_PASSWORD_TEXT`.
    pub redacted: bool,
}

/// Renders a snapshot into canonical lines, one entry per window header,
/// node and inline `- ...` marker, in compositor/accessible order.
pub fn canonical_lines(snapshot: &TreeSnapshot) -> Vec<CanonLine> {
    let mut lines = Vec::new();
    for window in &snapshot.windows {
        let geometry = window.geometry;
        let mut header = format!(
            "window {} {}{}({},{ } {}x{})  a11y={}",
            window.window_id.0,
            window
                .app_id
                .as_deref()
                .map(|s| format!("{s:?} "))
                .unwrap_or_default(),
            window
                .title
                .as_deref()
                .map(|s| format!("{s:?} "))
                .unwrap_or_default(),
            geometry.x,
            geometry.y,
            geometry.width,
            geometry.height,
            status_name(window.a11y_status),
        );
        if let Some(error) = &window.a11y_error {
            header.push_str(&format!(" — {error}"));
        }
        if let Some(hint) = &window.hint {
            header.push_str(&format!(" [{hint}]"));
        }
        lines.push(CanonLine {
            text: header,
            window_id: window.window_id,
            is_window_header: true,
            node: None,
        });
        if let Some(truncated) = &window.truncated {
            lines.push(marker_line(window.window_id, 1, truncated.note));
        }
        if let Some(root) = &window.root {
            render_node(root, window.window_id, 1, &mut lines);
        }
    }
    lines
}

/// The `a11y_status` wire name (`snake_case`, as `get_tree` reports it).
fn status_name(status: crate::daemon::a11y::A11yStatus) -> &'static str {
    use crate::daemon::a11y::A11yStatus as S;
    match status {
        S::Ok => "ok",
        S::Empty => "empty",
        S::Unavailable => "unavailable",
        S::Error => "error",
    }
}

/// A `- ... <note>` marker line (truncation notes, node query failures).
fn marker_line(window_id: WindowId, depth: usize, note: &str) -> CanonLine {
    CanonLine {
        text: format!("{}{note}", "  ".repeat(depth)),
        window_id,
        is_window_header: false,
        node: None,
    }
}

/// Renders `node` and its subtree depth-first: the node line at `depth`,
/// then children at `depth + 1`, then any trailing `- ...` marker at
/// `depth + 1` (matching `format_tree_node`'s layout).
fn render_node(node: &TreeNode, window_id: WindowId, depth: usize, lines: &mut Vec<CanonLine>) {
    lines.push(CanonLine {
        text: node_line(node, depth),
        window_id,
        is_window_header: false,
        node: Some(LineNode {
            node_ref: node.node_ref.clone(),
            role: node.role.clone(),
            name: node.name.clone(),
            states: node.states.clone(),
            bounds: node.bounds,
            text: node.text.clone(),
            value: node.value,
            value_text: node.value_text.clone(),
            redacted: node.redacted,
        }),
    });
    for child in &node.children {
        render_node(child, window_id, depth + 1, lines);
    }
    if let Some(truncated) = &node.truncated {
        lines.push(marker_line(window_id, depth + 1, truncated.note));
    } else if node.error.is_some() {
        // Normalized: raw peer errors can embed per-connection bus names,
        // which would make every snapshot diff.
        lines.push(marker_line(window_id, depth + 1, "- ... node query failed"));
    }
}

/// The canonical text of one node line:
/// `{indent}[eN] role "name" (x,y WxH) text="…" value=… value_text="…"
/// states=[a,b]`.
fn node_line(node: &TreeNode, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let bounds = node
        .bounds
        .map(|b| format!(" ({},{ } {}x{})", b.x, b.y, b.width, b.height))
        .unwrap_or_default();
    let text = node
        .text
        .as_deref()
        .map(|t| format!(" text={t:?}"))
        .unwrap_or_default();
    let value = node
        .value
        .map(|v| format!(" value={}", serde_json::to_value(v).unwrap_or_default()))
        .unwrap_or_default();
    let value_text = node
        .value_text
        .as_deref()
        .map(|t| format!(" value_text={t:?}"))
        .unwrap_or_default();
    let states = if node.states.is_empty() {
        String::new()
    } else {
        format!(" states=[{}]", node.states.join(","))
    };
    format!(
        "{indent}[{}] {} {:?}{}{}{}{}{}",
        node.node_ref, node.role, node.name, bounds, text, value, value_text, states,
    )
}

// -- tree_find --------------------------------------------------------------------

/// The node-level filters `tree_find` applies; every criterion present
/// must match (AND semantics). `window_id` scoping happens before the
/// search and is not part of the query.
#[derive(Debug, Default)]
pub struct FindQuery {
    /// Exact accessible role name (`"push button"`, `"entry"`, ...).
    pub role: Option<String>,
    /// Match against the node's accessible name.
    pub name: Option<String>,
    /// Match against the node's `Text`/`Value::Text` contents.
    pub text: Option<String>,
    /// `name`/`text` compare by equality instead of substring.
    pub exact: bool,
    /// AT-SPI state names that must all be present on the node.
    pub states: Vec<String>,
}

impl FindQuery {
    /// Whether any node-level criterion was supplied — searching with
    /// none would just echo the whole tree back.
    pub fn has_criteria(&self) -> bool {
        self.role.is_some() || self.name.is_some() || self.text.is_some() || !self.states.is_empty()
    }

    /// One string comparison under the query's match mode.
    fn text_match(&self, haystack: &str, needle: &str) -> bool {
        if self.exact {
            haystack == needle
        } else {
            haystack.contains(needle)
        }
    }

    /// Whether `node` satisfies every supplied criterion. A redacted
    /// node's text fields hold the `[REDACTED]` sentinel, so secret
    /// contents can never match (nor be probed for).
    fn matches(&self, node: &LineNode) -> bool {
        if let Some(role) = &self.role
            && node.role != *role
        {
            return false;
        }
        if let Some(name) = &self.name
            && !self.text_match(&node.name, name)
        {
            return false;
        }
        if let Some(text) = &self.text {
            let hit = [node.text.as_deref(), node.value_text.as_deref()]
                .into_iter()
                .flatten()
                .any(|candidate| self.text_match(candidate, text));
            if !hit {
                return false;
            }
        }
        self.states
            .iter()
            .all(|state| node.states.iter().any(|s| s == state))
    }
}

/// The line indices matching `query`, in tree order.
pub fn find_matches(lines: &[CanonLine], query: &FindQuery) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.node.as_ref().is_some_and(|node| query.matches(node)))
        .map(|(index, _)| index)
        .collect()
}

/// The canonical context around the match at `index`: up to `context`
/// lines before and after, never crossing the window header into a
/// neighbouring block.
pub fn context_lines(
    lines: &[CanonLine],
    index: usize,
    context: usize,
) -> (Vec<String>, String, Vec<String>) {
    // The window block containing `index` runs from its header line to
    // the next header (or EOF); a node line always sits inside one.
    let mut block_start = index;
    while block_start > 0 && !lines[block_start].is_window_header {
        block_start -= 1;
    }
    let mut block_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(index + 1) {
        if line.is_window_header {
            block_end = i;
            break;
        }
    }
    let start = index.saturating_sub(context).max(block_start);
    let end = (index + context + 1).min(block_end);
    let texts = |range: std::ops::Range<usize>| {
        lines[range]
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
    };
    (
        texts(start..index),
        lines[index].text.clone(),
        texts(index + 1..end),
    )
}

// -- tree_diff --------------------------------------------------------------------

/// How a diff hunk reads: lines only added, only removed, or both (a
/// `changed` run — added and removed counts need not match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkKind {
    /// Only new lines.
    Added,
    /// Only old lines.
    Removed,
    /// Old lines replaced by new ones.
    Changed,
}

impl HunkKind {
    /// The wire name reported in `tree_diff` hunks.
    pub fn name(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Changed => "changed",
        }
    }
}

/// One run of consecutive differences between two canonical line lists.
#[derive(Debug)]
pub struct Hunk {
    /// The hunk's classification.
    pub kind: HunkKind,
    /// Old-side line index where the run began.
    pub old_start: usize,
    /// New-side line index where the run began.
    pub new_start: usize,
    /// Old lines that disappeared (matched the hunk's `removed` side).
    pub removed: Vec<String>,
    /// New lines that appeared.
    pub added: Vec<String>,
}

/// The outcome of an LCS diff over two canonical line lists.
#[derive(Debug)]
pub struct LineDiff {
    /// The difference runs, in order.
    pub hunks: Vec<Hunk>,
    /// Total added lines across hunks.
    pub added: usize,
    /// Total removed lines across hunks.
    pub removed: usize,
    /// Lines the two snapshots share.
    pub unchanged: usize,
}

/// One aligned step of the diff: a shared line, an old-side deletion,
/// or a new-side insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Ins,
}

/// Computes the line-oriented LCS diff of `old` → `new`.
///
/// The inner alignment is Hirschberg's linear-space LCS: O(min side)
/// memory, O(n·m) worst-case time, which the snapshot node budget bounds
/// comfortably — and common-prefix/suffix trimming keeps the usual case
/// (nearly identical trees) cheap.
pub fn lcs_diff(old: &[String], new: &[String]) -> LineDiff {
    let mut ops = Vec::with_capacity(old.len().max(new.len()));
    diff_rec(old, new, &mut ops);

    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let (mut added, mut removed, mut unchanged) = (0usize, 0usize, 0usize);
    let mut open: Option<Hunk> = None;
    let flush = |open: &mut Option<Hunk>, hunks: &mut Vec<Hunk>| {
        if let Some(mut hunk) = open.take() {
            hunk.kind = match (hunk.removed.is_empty(), hunk.added.is_empty()) {
                (true, false) => HunkKind::Added,
                (false, true) => HunkKind::Removed,
                _ => HunkKind::Changed,
            };
            hunks.push(hunk);
        }
    };
    for op in ops {
        match op {
            Op::Keep => {
                flush(&mut open, &mut hunks);
                i += 1;
                j += 1;
                unchanged += 1;
            }
            Op::Del => {
                let hunk = open.get_or_insert_with(|| Hunk {
                    kind: HunkKind::Changed,
                    old_start: i,
                    new_start: j,
                    removed: Vec::new(),
                    added: Vec::new(),
                });
                hunk.removed.push(old[i].clone());
                i += 1;
                removed += 1;
            }
            Op::Ins => {
                let hunk = open.get_or_insert_with(|| Hunk {
                    kind: HunkKind::Changed,
                    old_start: i,
                    new_start: j,
                    removed: Vec::new(),
                    added: Vec::new(),
                });
                hunk.added.push(new[j].clone());
                j += 1;
                added += 1;
            }
        }
    }
    flush(&mut open, &mut hunks);
    LineDiff {
        hunks,
        added,
        removed,
        unchanged,
    }
}

/// Emits the aligned op sequence for `a` → `b`, trimming common prefix
/// and suffix first.
fn diff_rec(a: &[String], b: &[String], ops: &mut Vec<Op>) {
    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        ops.push(Op::Keep);
        prefix += 1;
    }
    let a = &a[prefix..];
    let b = &b[prefix..];
    let mut suffix = 0;
    while suffix < a.len() && suffix < b.len() && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }
    diff_mid(&a[..a.len() - suffix], &b[..b.len() - suffix], ops);
    ops.extend(std::iter::repeat_n(Op::Keep, suffix));
}

/// Hirschberg's linear-space alignment of the (post-trim) middle of the
/// two sequences.
fn diff_mid(a: &[String], b: &[String], ops: &mut Vec<Op>) {
    if a.is_empty() {
        ops.extend(std::iter::repeat_n(Op::Ins, b.len()));
        return;
    }
    if b.is_empty() {
        ops.extend(std::iter::repeat_n(Op::Del, a.len()));
        return;
    }
    if a.len() == 1 {
        match b.iter().position(|line| line == &a[0]) {
            Some(at) => {
                ops.extend(std::iter::repeat_n(Op::Ins, at));
                ops.push(Op::Keep);
                ops.extend(std::iter::repeat_n(Op::Ins, b.len() - at - 1));
            }
            None => {
                ops.push(Op::Del);
                ops.extend(std::iter::repeat_n(Op::Ins, b.len()));
            }
        }
        return;
    }
    if b.len() == 1 {
        match a.iter().position(|line| line == &b[0]) {
            Some(at) => {
                ops.extend(std::iter::repeat_n(Op::Del, at));
                ops.push(Op::Keep);
                ops.extend(std::iter::repeat_n(Op::Del, a.len() - at - 1));
            }
            None => {
                ops.push(Op::Ins);
                ops.extend(std::iter::repeat_n(Op::Del, a.len()));
            }
        }
        return;
    }
    // Split `a` in half and find the `b` split that a longest common
    // subsequence would cross at: `q` maximizes LCS(a₁, b[..q]) +
    // LCS(a₂, b[q..]).
    let mid = a.len() / 2;
    let a1: Vec<&String> = a[..mid].iter().collect();
    let a2: Vec<&String> = a[mid..].iter().rev().collect();
    let b_fwd: Vec<&String> = b.iter().collect();
    let b_rev: Vec<&String> = b.iter().rev().collect();
    let q = {
        // left[j] = LCS(a₁, b[..j]); right_rev[b.len()-j] = LCS(a₂, b[j..])
        let left = lcs_row(&a1, &b_fwd);
        let right_rev = lcs_row(&a2, &b_rev);
        let (mut best, mut best_value) = (0usize, 0usize);
        for j in 0..=b.len() {
            let value = left[j] + right_rev[b.len() - j];
            if value > best_value {
                best_value = value;
                best = j;
            }
        }
        best
    };
    diff_mid(&a[..mid], &b[..q], ops);
    diff_mid(&a[mid..], &b[q..], ops);
}

/// The last DP row of the LCS-length table for `a` × `b`: `row[j]` is
/// `LCS(a, b[..j])`. Two rows of O(b) memory.
fn lcs_row(a: &[&String], b: &[&String]) -> Vec<usize> {
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for x in a {
        cur[0] = 0;
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev
}
