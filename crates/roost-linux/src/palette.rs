//! Command palette — the pure, GTK-free model.
//!
//! Port of `mac/Sources/Roost/Palette.swift`: the items, the fuzzy
//! matcher, and the `PaletteState` navigation/filter/selection machine.
//! Kept split from the GTK overlay (`palette_ui.rs`) so the logic is
//! unit-tested in isolation. Themes, commands, and any future picker are
//! just different `PaletteFrame`s pushed onto the state.

use std::ops::Range;

use crate::keybind::KeybindAction;

/// One row in the palette. `id` is the stable handle the overlay maps
/// back to an action (a command id or a theme file name); `title` is
/// both what's shown and what the fuzzy matcher scores against, so
/// match ranges line up with the displayed text 1:1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub id: String,
    pub title: String,
    /// Right-aligned hint, e.g. a shortcut like "Alt+Shift+P".
    pub trailing_text: Option<String>,
}

impl PaletteItem {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            trailing_text: None,
        }
    }

    pub fn with_trailing(mut self, trailing: Option<String>) -> Self {
        self.trailing_text = trailing;
        self
    }
}

/// An item plus the title character offsets that matched the query, so
/// the overlay can bold them. `ranges` is empty for an empty query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteMatch {
    pub item: PaletteItem,
    /// Contiguous runs of matched **character** offsets into the title.
    pub ranges: Vec<Range<usize>>,
}

/// Case-insensitive subsequence match with light ranking. Returns
/// `None` when `query` is not a subsequence of `candidate`. Higher
/// score is a better match; ties are broken by the caller (stable, by
/// input order). Offsets in the returned ranges are character indices
/// into `candidate`.
///
/// Bonuses favor what feels right in a launcher: exact and prefix
/// matches win outright, consecutive runs and word-boundary hits score
/// higher, gaps cost a little. Verbatim port of `fuzzyMatch` in
/// `Palette.swift`.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i64, Vec<Range<usize>>)> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let c: Vec<char> = candidate.to_lowercase().chars().collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    if q.len() > c.len() {
        return None;
    }

    let mut matched: Vec<usize> = Vec::with_capacity(q.len());
    let mut ci = 0usize;
    for qc in &q {
        let mut found = false;
        while ci < c.len() {
            if c[ci] == *qc {
                matched.push(ci);
                ci += 1;
                found = true;
                break;
            }
            ci += 1;
        }
        if !found {
            return None;
        }
    }

    let mut score: i64 = 0;
    if c == q {
        score += 1000; // exact
    } else if c.starts_with(q.as_slice()) {
        score += 100; // prefix
    }
    let mut prev: i64 = -2;
    for &idx in &matched {
        let idx_i = idx as i64;
        if idx_i == prev + 1 {
            score += 10; // consecutive run
        } else if prev >= 0 {
            score -= (idx_i - prev - 1).min(5); // small gap penalty
        }
        if idx == 0 || is_boundary(c[idx - 1]) {
            score += 15; // start-of-word
        }
        prev = idx_i;
    }
    // Shorter candidates with the same hits read as tighter matches.
    score -= (c.len() / 10) as i64;

    Some((score, contiguous_ranges(&matched)))
}

fn is_boundary(ch: char) -> bool {
    ch == ' ' || ch == '-' || ch == '_' || ch == '/' || ch == '.'
}

/// Collapse sorted matched offsets into contiguous half-open ranges.
fn contiguous_ranges(offsets: &[usize]) -> Vec<Range<usize>> {
    let Some(&first) = offsets.first() else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    let mut start = first;
    let mut prev = first;
    for &idx in &offsets[1..] {
        if idx == prev + 1 {
            prev = idx;
        } else {
            ranges.push(start..(prev + 1));
            start = idx;
            prev = idx;
        }
    }
    ranges.push(start..(prev + 1));
    ranges
}

/// The first-cut command list, kept separate from `App` so its
/// alignment with the keybind namespace is unit-testable. Every spec id
/// is a `KeybindAction` id except `SELECT_THEME_ID` (a palette-only
/// command that drills into the theme list rather than firing once).
///
/// GTK parity deltas vs the Swift app: GTK has no `jump_to_unread`
/// action (omitted), and uses `delete_project` where the Mac app says
/// "Close Project".
pub struct PaletteCommands;

impl PaletteCommands {
    pub const SELECT_THEME_ID: &'static str = "select_theme";

    pub const SPECS: &'static [(&'static str, &'static str)] = &[
        (Self::SELECT_THEME_ID, "Select Theme…"),
        ("new_tab", "New Tab"),
        ("close_tab", "Close Tab"),
        ("rename_tab", "Rename Tab"),
        ("cycle_tab_next", "Next Tab"),
        ("cycle_tab_prev", "Previous Tab"),
        ("new_project", "New Project"),
        ("rename_project", "Rename Project"),
        ("delete_project", "Delete Project"),
        ("toggle_sidebar", "Toggle Sidebar"),
        ("font_increase", "Increase Font Size"),
        ("font_decrease", "Decrease Font Size"),
        ("font_reset", "Reset Font Size"),
    ];
}

/// One screen of the palette: a titled list with its own query +
/// selection. Pushing a sub-list (e.g. Select Theme…) starts fresh so
/// the parent's query doesn't carry in and filter everything away;
/// popping restores the parent's preserved query.
#[derive(Debug, Clone)]
pub struct PaletteFrame {
    pub id: String,
    pub placeholder: String,
    pub items: Vec<PaletteItem>,
    pub query: String,
    pub selection: usize,
}

impl PaletteFrame {
    pub fn new(
        id: impl Into<String>,
        placeholder: impl Into<String>,
        items: Vec<PaletteItem>,
    ) -> Self {
        Self {
            id: id.into(),
            placeholder: placeholder.into(),
            items,
            query: String::new(),
            selection: 0,
        }
    }

    /// Frame that opens pre-positioned on a given row (the theme list
    /// pre-highlights the active theme).
    pub fn with_selection(mut self, selection: usize) -> Self {
        self.selection = selection;
        self
    }
}

/// Pure navigation/filter/selection over a stack of frames. No GTK,
/// no callbacks, no side effects — the overlay reads `matches()` /
/// `selected_item()` and drives transitions; effects (preview, run,
/// revert) live in the overlay keyed off frame/item ids.
#[derive(Debug, Clone)]
pub struct PaletteState {
    stack: Vec<PaletteFrame>,
}

impl PaletteState {
    pub fn new(root: PaletteFrame) -> Self {
        Self { stack: vec![root] }
    }

    pub fn current(&self) -> &PaletteFrame {
        self.stack.last().expect("palette stack is never empty")
    }

    fn current_mut(&mut self) -> &mut PaletteFrame {
        self.stack.last_mut().expect("palette stack is never empty")
    }

    pub fn is_root(&self) -> bool {
        self.stack.len() == 1
    }

    /// Frames currently on the stack, bottom-up. Used by the overlay's
    /// dismissal path to fire `on_cancel` for each, top-down.
    pub fn frames(&self) -> &[PaletteFrame] {
        &self.stack
    }

    /// Filtered + ranked rows for the current frame's query. Empty
    /// query returns every item in input order (no highlight ranges).
    pub fn matches(&self) -> Vec<PaletteMatch> {
        let frame = self.current();
        let query = frame.query.trim();
        if query.is_empty() {
            return frame
                .items
                .iter()
                .map(|item| PaletteMatch {
                    item: item.clone(),
                    ranges: Vec::new(),
                })
                .collect();
        }
        let mut scored: Vec<(usize, i64, PaletteMatch)> = frame
            .items
            .iter()
            .enumerate()
            .filter_map(|(offset, item)| {
                fuzzy_match(query, &item.title).map(|(score, ranges)| {
                    (
                        offset,
                        score,
                        PaletteMatch {
                            item: item.clone(),
                            ranges,
                        },
                    )
                })
            })
            .collect();
        // Higher score first; stable by original order on ties.
        scored.sort_by(|a, b| {
            if a.1 != b.1 {
                b.1.cmp(&a.1)
            } else {
                a.0.cmp(&b.0)
            }
        });
        scored.into_iter().map(|(_, _, m)| m).collect()
    }

    /// The highlighted item, or `None` when the filter yields nothing.
    pub fn selected_item(&self) -> Option<PaletteItem> {
        let m = self.matches();
        m.get(self.current().selection).map(|m| m.item.clone())
    }

    /// Replace the current frame's query; reset selection to the top
    /// match (the best-ranked row).
    pub fn set_query(&mut self, query: impl Into<String>) {
        let frame = self.current_mut();
        frame.query = query.into();
        frame.selection = 0;
    }

    /// Set the highlight to an explicit row (a mouse click), clamped to
    /// the result bounds.
    pub fn set_selection(&mut self, index: usize) {
        let count = self.matches().len();
        if count == 0 {
            return;
        }
        self.current_mut().selection = index.min(count - 1);
    }

    /// Move the highlight, clamped to the result bounds (no wrap).
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.matches().len();
        if count == 0 {
            self.current_mut().selection = 0;
            return;
        }
        let next = (self.current().selection as isize + delta).clamp(0, count as isize - 1);
        self.current_mut().selection = next as usize;
    }

    /// Drill into a sub-list (starts with an empty query).
    pub fn push(&mut self, frame: PaletteFrame) {
        self.stack.push(frame);
    }

    /// Pop back to the parent frame, returning the frame that was
    /// removed (so the overlay can fire its cancel/revert exactly
    /// once). Returns `None` at the root.
    pub fn pop(&mut self) -> Option<PaletteFrame> {
        if self.is_root() {
            return None;
        }
        self.stack.pop()
    }
}

/// Build the curated command items, attaching each action's shortcut
/// hint via `shortcut_for`. The `select_theme` drill-in has no
/// shortcut. Mirrors `App.paletteCommandItems()` on the Mac side.
pub fn command_items(shortcut_for: impl Fn(KeybindAction) -> Option<String>) -> Vec<PaletteItem> {
    PaletteCommands::SPECS
        .iter()
        .map(|(id, title)| {
            let trailing = KeybindAction::from_name(id).and_then(&shortcut_for);
            PaletteItem::new(*id, *title).with_trailing(trailing)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- fuzzy matcher --------------------------------------------

    #[test]
    fn empty_query_matches_with_no_ranges() {
        let (score, ranges) = fuzzy_match("", "New Tab").unwrap();
        assert_eq!(score, 0);
        assert!(ranges.is_empty());
    }

    #[test]
    fn non_subsequence_returns_none() {
        assert!(fuzzy_match("xyz", "New Tab").is_none());
        // Query longer than candidate.
        assert!(fuzzy_match("newtabx", "New Tab").is_none());
    }

    #[test]
    fn exact_match_outscores_prefix() {
        let (exact, _) = fuzzy_match("new tab", "New Tab").unwrap();
        let (prefix, _) = fuzzy_match("new", "New Tab").unwrap();
        assert!(exact > prefix, "exact {exact} should beat prefix {prefix}");
    }

    #[test]
    fn ranges_are_contiguous_runs() {
        // "nt" matches N(0) and T(4) in "New Tab" → two singleton runs.
        let (_, ranges) = fuzzy_match("nt", "New Tab").unwrap();
        assert_eq!(ranges, vec![0..1, 4..5]);
        // "ne" matches N(0),e(1) → one run 0..2.
        let (_, ranges) = fuzzy_match("ne", "New Tab").unwrap();
        assert_eq!(ranges, vec![0..2]);
    }

    #[test]
    fn word_boundary_bonus_applies() {
        // The match after a space should score the boundary bonus.
        // "t" matching the T in "New Tab" (idx 4, preceded by space)
        // vs the same letter mid-word.
        let (boundary, _) = fuzzy_match("t", "New Tab").unwrap();
        let (midword, _) = fuzzy_match("e", "New Tab").unwrap();
        assert!(
            boundary > midword,
            "boundary {boundary} should beat midword {midword}"
        );
    }

    #[test]
    fn case_insensitive() {
        assert!(fuzzy_match("NEW", "new tab").is_some());
        assert!(fuzzy_match("new", "NEW TAB").is_some());
    }

    // ----- state machine --------------------------------------------

    fn cmd_frame() -> PaletteFrame {
        PaletteFrame::new(
            "commands",
            "Execute a command…",
            vec![
                PaletteItem::new("new_tab", "New Tab"),
                PaletteItem::new("close_tab", "Close Tab"),
                PaletteItem::new("new_project", "New Project"),
            ],
        )
    }

    #[test]
    fn empty_query_lists_all_in_input_order() {
        let state = PaletteState::new(cmd_frame());
        let matches = state.matches();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].item.id, "new_tab");
        assert_eq!(matches[2].item.id, "new_project");
    }

    #[test]
    fn set_query_filters_and_resets_selection() {
        let mut state = PaletteState::new(cmd_frame());
        state.move_selection(1);
        assert_eq!(state.current().selection, 1);
        state.set_query("new");
        // Selection reset to 0.
        assert_eq!(state.current().selection, 0);
        // "new" matches "New Tab" and "New Project" but not "Close Tab".
        let matches = state.matches();
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|m| m.item.title.starts_with("New")));
    }

    #[test]
    fn move_selection_clamps_no_wrap() {
        let mut state = PaletteState::new(cmd_frame());
        // Up from the top is a no-op (no wrap to bottom).
        state.move_selection(-1);
        assert_eq!(state.current().selection, 0);
        // Down past the end clamps to last.
        state.move_selection(100);
        assert_eq!(state.current().selection, 2);
        state.move_selection(100);
        assert_eq!(state.current().selection, 2);
    }

    #[test]
    fn set_selection_clamps() {
        let mut state = PaletteState::new(cmd_frame());
        state.set_selection(99);
        assert_eq!(state.current().selection, 2);
    }

    #[test]
    fn selected_item_none_when_filter_empty() {
        let mut state = PaletteState::new(cmd_frame());
        state.set_query("zzzz");
        assert!(state.matches().is_empty());
        assert!(state.selected_item().is_none());
    }

    #[test]
    fn push_starts_fresh_pop_restores_parent_query() {
        let mut state = PaletteState::new(cmd_frame());
        state.set_query("new");
        assert!(state.is_root());
        let sub = PaletteFrame::new(
            "themes",
            "Select a theme…",
            vec![PaletteItem::new("Dracula", "Dracula")],
        )
        .with_selection(0);
        state.push(sub);
        assert!(!state.is_root());
        // Sub-frame starts with an empty query.
        assert_eq!(state.current().query, "");
        assert_eq!(state.matches().len(), 1);
        // Pop returns the removed frame and restores the parent's query.
        let popped = state.pop().unwrap();
        assert_eq!(popped.id, "themes");
        assert!(state.is_root());
        assert_eq!(state.current().query, "new");
    }

    #[test]
    fn pop_at_root_returns_none() {
        let mut state = PaletteState::new(cmd_frame());
        assert!(state.is_root());
        assert!(state.pop().is_none());
    }

    #[test]
    fn frame_with_selection_preselects() {
        let frame = PaletteFrame::new(
            "themes",
            "Select a theme…",
            vec![
                PaletteItem::new("a", "a"),
                PaletteItem::new("b", "b"),
                PaletteItem::new("c", "c"),
            ],
        )
        .with_selection(2);
        let state = PaletteState::new(frame);
        assert_eq!(state.selected_item().unwrap().id, "c");
    }

    // ----- command registry / namespace sync ------------------------

    #[test]
    fn every_command_id_resolves_or_is_select_theme() {
        for (id, _title) in PaletteCommands::SPECS {
            if *id == PaletteCommands::SELECT_THEME_ID {
                // Sentinel — drills into the theme list, not a keybind.
                assert!(
                    KeybindAction::from_name(id).is_none(),
                    "select_theme should not be a real keybind action"
                );
                continue;
            }
            assert!(
                KeybindAction::from_name(id).is_some(),
                "command id {id:?} must map to a KeybindAction"
            );
        }
    }

    #[test]
    fn no_jump_to_unread_on_gtk() {
        // GTK parity delta: the Mac app has jump_to_unread; GTK omits it.
        assert!(PaletteCommands::SPECS
            .iter()
            .all(|(id, _)| *id != "jump_to_unread"));
    }

    #[test]
    fn uses_delete_project_not_close_project() {
        // GTK parity delta: delete_project, labelled "Delete Project".
        let entry = PaletteCommands::SPECS
            .iter()
            .find(|(id, _)| *id == "delete_project")
            .expect("delete_project present");
        assert_eq!(entry.1, "Delete Project");
        assert!(PaletteCommands::SPECS
            .iter()
            .all(|(id, _)| *id != "close_project"));
    }

    #[test]
    fn command_items_attach_shortcuts() {
        let items = command_items(|action| match action {
            KeybindAction::NewTab => Some("Ctrl+T".to_string()),
            _ => None,
        });
        let new_tab = items.iter().find(|i| i.id == "new_tab").unwrap();
        assert_eq!(new_tab.trailing_text.as_deref(), Some("Ctrl+T"));
        // The select_theme sentinel never carries a shortcut.
        let select_theme = items
            .iter()
            .find(|i| i.id == PaletteCommands::SELECT_THEME_ID)
            .unwrap();
        assert!(select_theme.trailing_text.is_none());
    }
}
