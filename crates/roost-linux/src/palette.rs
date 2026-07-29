//! Command palette — the pure, GTK-free model.
//!
//! Port of `mac/Sources/Roost/Palette.swift`: the items, the fuzzy
//! matcher, and the `PaletteState` navigation/filter/selection machine.
//! Kept split from the GTK overlay (`palette_ui.rs`) so the logic is
//! unit-tested in isolation. Themes, commands, and any future picker are
//! just different `PaletteFrame`s pushed onto the state.

use std::ops::Range;

use crate::keybind::KeybindAction;

/// The agents frame's per-row payload (plan 005 §3.4): everything the
/// multi-column agent row renders. Built by [`crate::agent_palette`].
///
/// This *is* the wire type — the UI row and `palette.state`'s row carry
/// exactly the same six fields, so aliasing keeps `app.rs`'s snapshot
/// mapping a move instead of a hand-written field copy that a new
/// column could silently fall out of.
///
/// `effective_lifecycle` drives the dot colour, the status colour, and
/// the row order — the same value the tab pill and the sidebar rollup
/// render, so the palette can never disagree with them.
pub use roost_ipc::messages::PaletteAgentRow as AgentRowData;

/// One row in the palette. `id` is the stable handle the overlay maps
/// back to an action (a command id or a theme file name); `title` is
/// both what's shown and what the fuzzy matcher scores against, so
/// match ranges line up with the displayed text 1:1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub id: String,
    pub title: String,
    /// Optional second line under the title (the notification list uses
    /// it for the message body). `None` keeps the row single-line.
    pub subtitle: Option<String>,
    /// Right-aligned hint, e.g. a shortcut like "Alt+Shift+P".
    pub trailing_text: Option<String>,
    /// When `false`, the row renders but can't be confirmed — `confirm`
    /// skips it (no behavior fired, palette stays open). Used for empty /
    /// disabled states (e.g. a provider's "No results" row, the overflow
    /// hint). Defaults to `true`.
    pub actionable: bool,
    /// Present only on agents-frame rows; the overlay renders the
    /// multi-column agent layout instead of the title/subtitle one.
    pub agent: Option<AgentRowData>,
}

impl PaletteItem {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            trailing_text: None,
            actionable: true,
            agent: None,
        }
    }

    pub fn with_subtitle(mut self, subtitle: Option<String>) -> Self {
        self.subtitle = subtitle;
        self
    }

    pub fn with_trailing(mut self, trailing: Option<String>) -> Self {
        self.trailing_text = trailing;
        self
    }

    pub fn with_actionable(mut self, actionable: bool) -> Self {
        self.actionable = actionable;
        self
    }

    pub fn with_agent(mut self, agent: AgentRowData) -> Self {
        self.agent = Some(agent);
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
/// This list is kept identical to the Swift app's `PaletteCommands.specs`
/// (same ids, titles, and order) so the two UIs expose one command set.
pub struct PaletteCommands;

impl PaletteCommands {
    pub const SELECT_THEME_ID: &'static str = "select_theme";

    /// Palette-only drill-in into the monospace font family list.
    /// Same pattern as `SELECT_THEME_ID`: not a `KeybindAction`, pushes
    /// a sub-frame with live preview + Esc-to-revert. Mirrors the Mac
    /// app's `PaletteCommands.selectFontID`.
    pub const SELECT_FONT_ID: &'static str = "select_font";

    /// Palette-only drill-in into the live notification inbox. Like
    /// `SELECT_THEME_ID`, not a `KeybindAction` — built dynamically in
    /// `App::command_items` so its title can carry the live count.
    pub const VIEW_NOTIFICATIONS_ID: &'static str = "view_notifications";
    /// Palette-only drill-in into the agent switcher (plan 005 §3.1).
    /// Like the notification commands, built dynamically in
    /// `App::show_command_palette` — it sits directly under Select
    /// Font…, ahead of the notification rows.
    pub const VIEW_AGENTS_ID: &'static str = "view_agents";
    /// Palette-only command: empty the inbox + clear all pending dots.
    pub const CLEAR_NOTIFICATIONS_ID: &'static str = "clear_notifications";

    pub const SPECS: &'static [(&'static str, &'static str)] = &[
        (Self::SELECT_THEME_ID, "Select Theme…"),
        (Self::SELECT_FONT_ID, "Select Font…"),
        ("new_tab", "New Tab"),
        ("close_tab", "Close Tab"),
        ("rename_tab", "Rename Tab"),
        ("cycle_tab_next", "Next Tab"),
        ("cycle_tab_prev", "Previous Tab"),
        ("new_project", "New Project"),
        ("rename_project", "Rename Project"),
        ("close_project", "Close Project"),
        ("toggle_sidebar", "Toggle Sidebar"),
        ("jump_to_unread", "Jump to Unread"),
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
    /// Muted one-line hint bar under the list (the agents frame's
    /// "↑↓ move  ↵ go to tab  esc close"). `None` on every other frame,
    /// which renders no footer. Not exposed on the wire.
    pub footer_hints: Option<String>,
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
            footer_hints: None,
        }
    }

    /// Frame that opens pre-positioned on a given row (the theme list
    /// pre-highlights the active theme).
    pub fn with_selection(mut self, selection: usize) -> Self {
        self.selection = selection;
        self
    }

    pub fn with_footer_hints(mut self, hints: impl Into<String>) -> Self {
        self.footer_hints = Some(hints.into());
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

    /// Filtered + ranked rows for the current frame's query, **borrowed**.
    /// Empty query yields every item in input order (no highlight
    /// ranges).
    ///
    /// [`matches`](Self::matches) is this plus a clone per row. An agent
    /// row carries up to nine `String`s and the list is re-derived on
    /// every keystroke, arrow key, and live refresh, so the callers that
    /// only need a count or an id go through here instead.
    fn ranked(&self) -> Vec<(&PaletteItem, Vec<Range<usize>>)> {
        Self::ranked_of(self.current())
    }

    fn ranked_of(frame: &PaletteFrame) -> Vec<(&PaletteItem, Vec<Range<usize>>)> {
        let query = frame.query.trim();
        if query.is_empty() {
            return frame.items.iter().map(|item| (item, Vec::new())).collect();
        }
        let mut scored: Vec<(usize, i64, &PaletteItem, Vec<Range<usize>>)> = frame
            .items
            .iter()
            .enumerate()
            .filter_map(|(offset, item)| {
                fuzzy_match(query, &item.title).map(|(score, ranges)| (offset, score, item, ranges))
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
        scored
            .into_iter()
            .map(|(_, _, item, ranges)| (item, ranges))
            .collect()
    }

    /// Filtered + ranked rows for the current frame's query. Empty
    /// query returns every item in input order (no highlight ranges).
    pub fn matches(&self) -> Vec<PaletteMatch> {
        self.ranked()
            .into_iter()
            .map(|(item, ranges)| PaletteMatch {
                item: item.clone(),
                ranges,
            })
            .collect()
    }

    /// How many rows the current filter yields — [`matches`](Self::matches)
    /// without the per-row clone.
    pub fn match_count(&self) -> usize {
        self.ranked().len()
    }

    /// The highlighted item, or `None` when the filter yields nothing.
    pub fn selected_item(&self) -> Option<PaletteItem> {
        self.selected_ranked().map(|(item, _)| item.clone())
    }

    fn selected_ranked(&self) -> Option<(&PaletteItem, Vec<Range<usize>>)> {
        let selection = self.current().selection;
        self.ranked().into_iter().nth(selection)
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
        let count = self.match_count();
        if count == 0 {
            return;
        }
        self.current_mut().selection = index.min(count - 1);
    }

    /// Move the highlight, clamped to the result bounds (no wrap).
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.match_count();
        if count == 0 {
            self.current_mut().selection = 0;
            return;
        }
        let next = (self.current().selection as isize + delta).clamp(0, count as isize - 1);
        self.current_mut().selection = next as usize;
    }

    /// Replace the items of the frame with `frame_id` in place, leaving
    /// its query untouched and keeping the *same row* highlighted
    /// wherever it landed. Returns false when no frame on the stack has
    /// that id (the caller's target was popped).
    ///
    /// Selection is preserved by row **id**, not index: the agents frame
    /// rebuilds on every agent event and re-ranks, so the row under the
    /// cursor moves. A row that disappeared falls back to the old index,
    /// clamped.
    pub fn update_items(&mut self, frame_id: &str, items: Vec<PaletteItem>) -> bool {
        let Some(index) = self.stack.iter().position(|f| f.id == frame_id) else {
            return false;
        };
        // Preserve by id even for a frame under the top one: its
        // selection isn't visible now, but it is restored on pop, so a
        // refresh that reorders or removes rows must re-anchor (and
        // clamp) it the same way it does for the showing frame.
        let frame = &self.stack[index];
        let selected_id = Self::ranked_of(frame)
            .into_iter()
            .nth(frame.selection)
            .map(|(item, _)| item.id.clone());
        self.stack[index].items = items;
        let frame = &self.stack[index];
        let ranked_ids: Vec<&str> = Self::ranked_of(frame)
            .into_iter()
            .map(|(item, _)| item.id.as_str())
            .collect();
        let selection = selected_id
            .as_deref()
            .and_then(|id| ranked_ids.iter().position(|candidate| *candidate == id))
            .unwrap_or_else(|| frame.selection.min(ranked_ids.len().saturating_sub(1)));
        self.stack[index].selection = selection;
        true
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
    fn update_items_keeps_the_highlighted_row_by_id() {
        let mut state = PaletteState::new(cmd_frame());
        state.move_selection(2);
        assert_eq!(state.selected_item().unwrap().id, "new_project");
        // Rebuild with the rows re-ranked and one dropped: the same row
        // stays highlighted at its new position.
        assert!(state.update_items(
            "commands",
            vec![
                PaletteItem::new("new_project", "New Project"),
                PaletteItem::new("new_tab", "New Tab"),
            ],
        ));
        assert_eq!(state.current().selection, 0);
        assert_eq!(state.selected_item().unwrap().id, "new_project");
    }

    #[test]
    fn update_items_leaves_the_query_alone_and_clamps_a_vanished_row() {
        let mut state = PaletteState::new(cmd_frame());
        state.set_query("new");
        state.move_selection(1);
        assert_eq!(state.selected_item().unwrap().id, "new_project");
        state.update_items("commands", vec![PaletteItem::new("new_tab", "New Tab")]);
        // Query survives the rebuild; the vanished row falls back to the
        // old index, clamped to the new match count.
        assert_eq!(state.current().query, "new");
        assert_eq!(state.current().selection, 0);
        assert_eq!(state.selected_item().unwrap().id, "new_tab");
    }

    #[test]
    fn update_items_reaches_a_frame_under_the_top_one() {
        let mut state = PaletteState::new(cmd_frame());
        state.push(PaletteFrame::new(
            "themes",
            "Select a theme…",
            vec![PaletteItem::new("Dracula", "Dracula")],
        ));
        assert!(state.update_items("commands", vec![PaletteItem::new("only", "Only")]));
        // The visible frame is untouched…
        assert_eq!(state.matches().len(), 1);
        assert_eq!(state.selected_item().unwrap().id, "Dracula");
        // …and the parent carries the new rows once popped back to.
        state.pop();
        assert_eq!(state.selected_item().unwrap().id, "only");
    }

    #[test]
    fn update_items_re_anchors_a_covered_frames_selection_by_id() {
        let mut state = PaletteState::new(cmd_frame());
        state.move_selection(2);
        assert_eq!(state.selected_item().unwrap().id, "new_project");
        state.push(PaletteFrame::new(
            "themes",
            "Select a theme…",
            vec![PaletteItem::new("Dracula", "Dracula")],
        ));
        // Refresh the covered frame with its rows re-ranked and one
        // dropped: the remembered selection must follow the row id, not
        // point at whatever now sits at the old index.
        assert!(state.update_items(
            "commands",
            vec![
                PaletteItem::new("new_project", "New Project"),
                PaletteItem::new("new_tab", "New Tab"),
            ],
        ));
        state.pop();
        assert_eq!(state.selected_item().unwrap().id, "new_project");
        assert_eq!(state.current().selection, 0);
    }

    #[test]
    fn update_items_unknown_frame_is_a_no_op() {
        let mut state = PaletteState::new(cmd_frame());
        assert!(!state.update_items("agents", vec![PaletteItem::new("x", "X")]));
        assert_eq!(state.matches().len(), 3);
    }

    #[test]
    fn footer_hints_default_to_none() {
        let frame = cmd_frame();
        assert_eq!(frame.footer_hints, None);
        let with_hints = cmd_frame().with_footer_hints("esc close");
        assert_eq!(with_hints.footer_hints.as_deref(), Some("esc close"));
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
            if *id == PaletteCommands::SELECT_THEME_ID || *id == PaletteCommands::SELECT_FONT_ID {
                // Sentinel — drills into a sub-frame, not a keybind.
                assert!(
                    KeybindAction::from_name(id).is_none(),
                    "{id:?} should not be a real keybind action"
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
    fn has_jump_to_unread() {
        // Parity with the Mac app (was a GTK gap; ported in P8).
        assert!(PaletteCommands::SPECS
            .iter()
            .any(|(id, _)| *id == "jump_to_unread"));
    }

    #[test]
    fn uses_close_project_not_delete_project() {
        // Unified on "Close Project" across both UIs (was a GTK delta).
        let entry = PaletteCommands::SPECS
            .iter()
            .find(|(id, _)| *id == "close_project")
            .expect("close_project present");
        assert_eq!(entry.1, "Close Project");
        assert!(PaletteCommands::SPECS
            .iter()
            .all(|(id, _)| *id != "delete_project"));
    }

    #[test]
    fn item_subtitle_round_trips() {
        // Plain items have no subtitle; `with_subtitle` sets it and
        // leaves the title/id intact (the notification list relies on
        // this for the two-line body row).
        let plain = PaletteItem::new("a", "Title");
        assert_eq!(plain.subtitle, None);
        let withsub = PaletteItem::new("notif:7", "roost · claude")
            .with_subtitle(Some("needs your input".to_string()))
            .with_trailing(Some("2m".to_string()));
        assert_eq!(withsub.id, "notif:7");
        assert_eq!(withsub.title, "roost · claude");
        assert_eq!(withsub.subtitle.as_deref(), Some("needs your input"));
        assert_eq!(withsub.trailing_text.as_deref(), Some("2m"));
        // A `None` subtitle keeps the row single-line.
        let cleared = withsub.with_subtitle(None);
        assert_eq!(cleared.subtitle, None);
    }

    #[test]
    fn notification_command_ids_are_not_keybinds() {
        // The two notification commands are palette-only sentinels (like
        // select_theme), built dynamically rather than via SPECS, so
        // they must not collide with the keybind namespace.
        assert!(KeybindAction::from_name(PaletteCommands::VIEW_NOTIFICATIONS_ID).is_none());
        assert!(KeybindAction::from_name(PaletteCommands::CLEAR_NOTIFICATIONS_ID).is_none());
        assert!(PaletteCommands::SPECS
            .iter()
            .all(|(id, _)| *id != PaletteCommands::VIEW_NOTIFICATIONS_ID
                && *id != PaletteCommands::CLEAR_NOTIFICATIONS_ID));
    }

    #[test]
    fn view_agents_is_a_palette_only_sentinel() {
        // Same contract as the notification sentinels: dynamic row, not
        // in SPECS, and never a keybind id (`agent_palette` is the
        // keybind that opens the same frame).
        assert!(KeybindAction::from_name(PaletteCommands::VIEW_AGENTS_ID).is_none());
        assert!(PaletteCommands::SPECS
            .iter()
            .all(|(id, _)| *id != PaletteCommands::VIEW_AGENTS_ID));
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
