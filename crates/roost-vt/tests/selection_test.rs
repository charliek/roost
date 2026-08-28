#![cfg(feature = "ffi")]

use roost_vt::{RenderState, ScrollViewport, Terminal, TerminalOptions, TerminalSelection};

const COLS: u16 = 20;
const ROWS: u16 = 5;

fn terminal() -> (Terminal, RenderState) {
    sized_terminal(20)
}

fn sized_terminal(max_scrollback: usize) -> (Terminal, RenderState) {
    (
        Terminal::new(TerminalOptions {
            cols: COLS,
            rows: ROWS,
            max_scrollback,
        })
        .expect("terminal"),
        RenderState::new().expect("render state"),
    )
}

fn write_lines(terminal: &mut Terminal, count: usize) {
    for i in 0..count {
        terminal.vt_write(format!("line{i}\r\n").as_bytes());
    }
}

/// Push whatever is on screen up into scrollback, starting on a fresh row
/// so the filler never lands on the selected content.
fn scroll_out(terminal: &mut Terminal, count: usize) {
    terminal.vt_write(b"\r\n");
    write_lines(terminal, count);
}

/// Commit a selection between two viewport cells, asserting it landed.
/// `Err` from `set` is a libghostty failure, not a rejected coordinate.
fn select(
    selection: &mut TerminalSelection,
    terminal: &Terminal,
    anchor: (u16, u16),
    cursor: (u16, u16),
) {
    assert!(selection
        .set(terminal, anchor, cursor)
        .expect("set selection"));
}

fn text(
    selection: &TerminalSelection,
    terminal: &Terminal,
    render_state: &mut RenderState,
) -> Option<String> {
    selection
        .selected_text(terminal, render_state, COLS, ROWS)
        .expect("selected text")
}

#[test]
fn committed_selection_extracts_exact_text() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"hello world");
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 0), (4, 0));
    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, 20, 5)
            .expect("selected text"),
        Some("hello".into())
    );
}

#[test]
fn clear_returns_to_empty_snapshot() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"abc");
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 0), (2, 0));
    assert!(selection.clear());
    assert_eq!(
        selection
            .snapshot(&terminal, &mut render_state, 20, 5)
            .expect("snapshot"),
        None
    );
}

#[test]
fn out_of_range_begin_clears_stale_selection() {
    let (mut terminal, _) = terminal();
    terminal.vt_write(b"abc");
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 0), (2, 0));
    assert!(!selection
        .begin(&terminal, 0, u16::MAX)
        .expect("begin selection"));
    assert!(!selection.is_active());
}

#[test]
fn multi_row_selection_yields_shared_spans_and_text() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"abc\r\ndef\r\nghi");
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (1, 0), (1, 2));
    assert_eq!(
        selection.visible_spans(&terminal, 20, 5),
        vec![
            roost_vt::SelectionSpan {
                row: 0,
                col0: 1,
                col1: 20,
            },
            roost_vt::SelectionSpan {
                row: 1,
                col0: 0,
                col1: 20,
            },
            roost_vt::SelectionSpan {
                row: 2,
                col0: 0,
                col1: 2,
            },
        ]
    );
    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, 20, 5)
            .expect("selected text"),
        Some("bc\ndef\ngh".into())
    );
}

#[test]
fn drag_is_hidden_until_it_moves_and_invalid_update_clears_it() {
    let (terminal, _) = terminal();
    let mut selection = TerminalSelection::new();
    assert!(selection.begin(&terminal, 2, 1).expect("begin selection"));
    assert!(selection.visible_spans(&terminal, 20, 5).is_empty());
    assert!(selection.update(&terminal, 4, 1).expect("update selection"));
    assert_eq!(
        selection.visible_spans(&terminal, 20, 5),
        vec![roost_vt::SelectionSpan {
            row: 1,
            col0: 2,
            col1: 5,
        }]
    );
    assert!(!selection
        .update(&terminal, 4, u16::MAX)
        .expect("update selection"));
    assert!(!selection.is_active());
}

// ============================================================================
// Scrollback-spanning copy (issue #249)
// ============================================================================
//
// The render state can only see the viewport, so anything reaching into
// scrollback goes through libghostty's formatter instead. These tests
// drive the selection out of the viewport the way output does — by
// writing more rows — and by scrolling, and assert nothing is clipped.

#[test]
fn selection_scrolled_entirely_above_the_viewport_is_not_clipped() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"alpha\r\nbravo\r\ncharlie");
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 0), (6, 2));
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("alpha\nbravo\ncharlie".into())
    );

    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("alpha\nbravo\ncharlie".into())
    );
}

#[test]
fn selection_straddling_the_top_edge_keeps_its_scrolled_off_rows() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"alpha\r\nbravo\r\ncharlie\r\ndelta");
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 0), (4, 3));

    // Two rows scroll off; the rest stay visible.
    scroll_out(&mut terminal, 2);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("alpha\nbravo\ncharlie\ndelta".into())
    );
}

#[test]
fn selection_dragged_across_the_whole_history_copies_every_row() {
    let (mut terminal, mut render_state) = sized_terminal(4096);
    write_lines(&mut terminal, 200);

    terminal.scroll_viewport(ScrollViewport::Top);
    let mut selection = TerminalSelection::new();
    assert!(selection.begin(&terminal, 0, 0).expect("begin selection"));
    terminal.scroll_viewport(ScrollViewport::Bottom);
    assert!(selection
        .update(&terminal, 0, ROWS - 1)
        .expect("update selection"));

    let copied = text(&selection, &terminal, &mut render_state).expect("text");
    let lines: Vec<&str> = copied.split('\n').collect();
    assert_eq!(lines.first(), Some(&"line0"));
    assert_eq!(lines.last(), Some(&"line199"));
    assert_eq!(lines.len(), 200, "clipped: {copied:?}");
}

#[test]
fn reversed_drag_into_scrollback_copies_in_document_order() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"alpha\r\nbravo\r\ncharlie");
    let mut selection = TerminalSelection::new();
    // Anchor below, cursor above: libghostty orders the endpoints itself.
    select(&mut selection, &terminal, (6, 2), (0, 0));
    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("alpha\nbravo\ncharlie".into())
    );
}

#[test]
fn scrollback_copy_preserves_wide_and_combining_glyphs() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write("\u{4f60}\u{597d}\r\na\u{301}bc".as_bytes());
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 0), (COLS - 1, 1));
    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("\u{4f60}\u{597d}\na\u{301}bc".into())
    );
}

#[test]
fn scrollback_copy_keeps_interior_blank_rows() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"alpha\r\n\r\nbravo");
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 0), (COLS - 1, 2));
    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("alpha\n\nbravo".into())
    );
}

// ============================================================================
// Scrollback eviction (issue #334)
// ============================================================================
//
// Endpoints are tracked refs, so eviction has exactly two outcomes and
// no third: while the selected row survives the selection follows it,
// and once the row itself is pruned the selection resolves to nothing.
// It never lands on whatever content inherited the old coordinate —
// which is what stored screen-y endpoints used to do.
//
// The setup numbers are load-bearing. libghostty prunes at *page*
// granularity, so a scrollback limit smaller than one page evicts
// nothing until a whole page has filled and then drops the lot; to get
// a moment where older rows are gone and the selected row is not, the
// limit has to span several pages. Rows per page scale inversely with
// the column count (a page's byte budget comes from ghostty's standard
// capacity; the OS page size only rounds the allocation), so these
// tests run wide — 80 columns is ~594 rows/page on every target, and
// the 2000-line limit both UIs ship therefore covers 3-4 pages.

/// Columns for the eviction tests. Wide on purpose (see above).
const EVICT_COLS: u16 = 80;
/// The scrollback limit both UIs ship (`terminal.rs`'s doc, and
/// `TerminalView.defaultScrollback` on the Mac side).
const EVICT_SCROLLBACK: usize = 2000;
/// Filler written before the marker, so the marker is not in the very
/// first page — the page that gets pruned first.
const EVICT_PRE: usize = 1000;
/// Rows written after the marker for the "older rows pruned" case.
/// Above the limit (pruning has to have happened) and far enough below
/// the marker's own depth that its page survives: measured, the marker
/// is dropped at ~1595 on arm64-macos and ~2000 on Linux.
const EVICT_SURVIVES: usize = 1300;
/// Rows written after the marker for the "selected row pruned" case —
/// past the point where the marker's page can survive on either target.
const EVICT_PRUNED: usize = 3000;

fn eviction_terminal() -> (Terminal, RenderState) {
    (
        Terminal::new(TerminalOptions {
            cols: EVICT_COLS,
            rows: ROWS,
            max_scrollback: EVICT_SCROLLBACK,
        })
        .expect("terminal"),
        RenderState::new().expect("render state"),
    )
}

fn eviction_text(
    selection: &TerminalSelection,
    terminal: &Terminal,
    render_state: &mut RenderState,
) -> Option<String> {
    selection
        .selected_text(terminal, render_state, EVICT_COLS, ROWS)
        .expect("selected text")
}

/// Fill history, then put `MARKER` on the bottom row and select it.
fn anchor_marker(terminal: &mut Terminal) -> TerminalSelection {
    for i in 0..EVICT_PRE {
        terminal.vt_write(format!("filler{i}\r\n").as_bytes());
    }
    terminal.vt_write(b"MARKER");
    let mut selection = TerminalSelection::new();
    select(
        &mut selection,
        terminal,
        (0, ROWS - 1),
        (EVICT_COLS - 1, ROWS - 1),
    );
    selection
}

/// Assert history really was pruned, so a green test cannot mean "the
/// scenario never happened": libghostty is holding fewer rows than were
/// written.
fn assert_history_pruned(terminal: &Terminal, written: usize) {
    let total = terminal.scrollbar().expect("scrollbar").total;
    assert!(
        total < written as u64,
        "no history was pruned ({total} rows held, {written} written); \
         the eviction scenario did not run"
    );
}

/// Scenario 1: rows *older* than the selection are pruned. The tracked
/// endpoints move with their content, so the copy is unchanged.
#[test]
fn selection_follows_its_content_when_older_history_is_pruned() {
    let (mut terminal, mut render_state) = eviction_terminal();
    let selection = anchor_marker(&mut terminal);
    assert_eq!(
        eviction_text(&selection, &terminal, &mut render_state),
        Some("MARKER".into())
    );

    terminal.vt_write(b"\r\n");
    write_lines(&mut terminal, EVICT_SURVIVES);
    assert_history_pruned(&terminal, EVICT_PRE + 1 + EVICT_SURVIVES);

    assert_eq!(
        eviction_text(&selection, &terminal, &mut render_state),
        Some("MARKER".into()),
        "selection drifted off its content instead of following it"
    );
}

/// Scenario 2: the selected row itself is pruned. Tracked refs cannot
/// follow discarded content, so the selection reports nothing — never
/// some other row's text.
#[test]
fn selection_reports_nothing_once_its_own_row_is_pruned() {
    let (mut terminal, mut render_state) = eviction_terminal();
    let selection = anchor_marker(&mut terminal);

    terminal.vt_write(b"\r\n");
    write_lines(&mut terminal, EVICT_PRUNED);
    assert_history_pruned(&terminal, EVICT_PRE + 1 + EVICT_PRUNED);

    assert_eq!(
        eviction_text(&selection, &terminal, &mut render_state),
        None,
        "a pruned selection resolved to text"
    );
    assert!(selection
        .visible_spans(&terminal, EVICT_COLS, ROWS)
        .is_empty());
    let snapshot = selection
        .snapshot(&terminal, &mut render_state, EVICT_COLS, ROWS)
        .expect("snapshot")
        .expect("the selection is still held, it just resolves to nothing");
    assert_eq!(snapshot.text, None);
    assert!(!snapshot.anchor_visible);
    assert!(!snapshot.cursor_visible);
}

// ============================================================================
// Viewport path vs formatter path
// ============================================================================

/// Copy the same selection twice — once while it is fully visible (the
/// render-state walk) and once after scrolling it out of view (the
/// formatter) — and require the two to agree. A fast path that disagrees
/// with the slow one would make a copy depend on scroll position.
fn assert_paths_agree(lines: [&str; 3]) {
    let (mut terminal, mut render_state) = sized_terminal(4096);
    write_lines(&mut terminal, 40);
    terminal.vt_write(lines.join("\r\n").as_bytes());

    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (0, 2), (COLS - 1, ROWS - 1));
    let visible = text(&selection, &terminal, &mut render_state);

    terminal.scroll_viewport(ScrollViewport::Top);
    let scrolled = text(&selection, &terminal, &mut render_state);

    assert_eq!(visible, scrolled, "paths disagree for {lines:?}");
}

#[test]
fn both_paths_agree() {
    assert_paths_agree(["alpha", "bravo", "charlie"]);
    assert_paths_agree(["alpha", "", "bravo"]);
    assert_paths_agree(["alpha", "bravo", ""]);
    assert_paths_agree(["alpha", "   ", "bravo"]);
    assert_paths_agree(["alpha   ", "bravo", "charlie"]);
    assert_paths_agree(["a\tb", "c\td", "ef"]);
    assert_paths_agree(["a\u{301}bc", "bravo", "charlie"]);
    assert_paths_agree(["", "", ""]);
    // Cases that used to diverge.
    assert_paths_agree(["\u{4f60}\u{597d}", "bravo", "charlie"]);
    assert_paths_agree(["a\u{754c}b", "c\u{754c}", "\u{754c}d"]);
    assert_paths_agree(["", "alpha", "bravo"]);
    assert_paths_agree(["", "", "alpha"]);
    assert_paths_agree(["   ", "alpha", "bravo"]);
    assert_paths_agree(["alpha", "bravo", "   "]);
    assert_paths_agree(["   ", "   ", "   "]);
    // A wide glyph that cannot fit in the last column wraps, leaving a
    // spacer head behind on the row it did not fit on.
    assert_paths_agree(["1234567890123456789\u{754c}", "bravo", "charlie"]);
    // Soft-wrapped content: the rows the terminal broke for itself.
    assert_paths_agree(["abcdefghijklmnopqrstuvwxyz0123", "bravo", "charlie"]);
    assert_paths_agree(["alpha", "abcdefghijklmnopqrstuvwxyz0123", "charlie"]);
    assert_paths_agree(["alpha", "bravo", "abcdefghijklmnopqrstuvwxyz0123"]);
    assert_paths_agree([
        "alpha        ",
        "bravo",
        "\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}abc",
    ]);
}

// ============================================================================
// Soft-wrap unwrapping (plan 024 D4.4)
// ============================================================================

/// Write `input`, select `anchor..=cursor`, and require the same text
/// both while the selection is visible (the render-state walk) and once
/// it has been pushed into scrollback (libghostty's formatter).
///
/// `joined` is what the copy should be with
/// [`roost_vt::UNWRAP_SOFT_WRAPPED_LINES`] on, `per_row` with it off, so
/// every case pins the behavior either way the constant is set.
fn assert_wrapped_copy(
    input: &str,
    anchor: (u16, u16),
    cursor: (u16, u16),
    joined: &str,
    per_row: &str,
) {
    let expected = if roost_vt::UNWRAP_SOFT_WRAPPED_LINES {
        joined
    } else {
        per_row
    };
    let (mut terminal, mut render_state) = sized_terminal(4096);
    terminal.vt_write(input.as_bytes());
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, anchor, cursor);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some(expected.into()),
        "visible copy of {input:?}"
    );

    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some(expected.into()),
        "scrollback copy of {input:?}"
    );
}

/// 30 narrow cells at 20 columns: rows 0 and 1 are one logical line.
const WRAP_2: &str = "abcdefghijklmnopqrstuvwxyz0123";
/// 45 narrow cells: rows 0, 1 and 2 are one logical line.
const WRAP_3: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHI";

#[test]
fn a_line_wrapped_across_two_rows_copies_as_one_line() {
    assert_wrapped_copy(
        WRAP_2,
        (0, 0),
        (COLS - 1, 1),
        "abcdefghijklmnopqrstuvwxyz0123",
        "abcdefghijklmnopqrst\nuvwxyz0123",
    );
}

#[test]
fn a_line_wrapped_across_three_rows_copies_as_one_line() {
    assert_wrapped_copy(
        WRAP_3,
        (0, 0),
        (COLS - 1, 2),
        "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHI",
        "abcdefghijklmnopqrst\nuvwxyz0123456789ABCD\nEFGHI",
    );
}

/// The wrap lands inside a word, which is the normal case — a terminal
/// breaks on the column, not on whitespace. Rejoining has to put the
/// word back together with nothing inserted between the halves.
#[test]
fn a_wrap_inside_a_word_rejoins_the_word() {
    assert_wrapped_copy(
        "wrapped-word-boundary-check",
        (0, 0),
        (COLS - 1, 1),
        "wrapped-word-boundary-check",
        "wrapped-word-boundar\ny-check",
    );
}

/// A real newline after a soft-wrapped line still breaks the copy. Only
/// the wrap is absorbed.
#[test]
fn a_hard_newline_after_a_wrapped_line_survives() {
    assert_wrapped_copy(
        "abcdefghijklmnopqrstuvwxyz0123\r\ntail",
        (0, 0),
        (COLS - 1, 2),
        "abcdefghijklmnopqrstuvwxyz0123\ntail",
        "abcdefghijklmnopqrst\nuvwxyz0123\ntail",
    );
}

/// Starting the selection part-way through a wrapped line keeps the rest
/// of that line on one line rather than breaking it at the screen edge.
#[test]
fn a_selection_starting_mid_wrapped_line_still_joins_the_rest() {
    assert_wrapped_copy(
        WRAP_2,
        (5, 0),
        (COLS - 1, 1),
        "fghijklmnopqrstuvwxyz0123",
        "fghijklmnopqrst\nuvwxyz0123",
    );
}

/// A row of nothing but wide glyphs fills all 20 columns exactly and
/// wraps into the next row.
#[test]
fn a_wrapped_line_of_wide_glyphs_copies_as_one_line() {
    let cjk = "\u{4f60}\u{597d}".repeat(5);
    assert_wrapped_copy(
        &format!("{cjk}abc"),
        (0, 0),
        (COLS - 1, 1),
        &format!("{cjk}abc"),
        &format!("{cjk}\nabc"),
    );
}

/// A wide grapheme that does not fit in the last column wraps whole,
/// leaving a placeholder behind. The rejoined line must carry the
/// grapheme exactly once and put nothing where the placeholder was.
#[test]
fn a_wide_grapheme_straddling_the_wrap_boundary_copies_once() {
    assert_wrapped_copy(
        "1234567890123456789\u{754c}XY",
        (0, 0),
        (3, 1),
        "1234567890123456789\u{754c}XY",
        "1234567890123456789\n\u{754c}XY",
    );
}

/// Ending the selection ON that placeholder is the case libghostty
/// handles by reaching into the next row for the grapheme that wrapped
/// (`PageFormatter.formatWithState`'s spacer-head adjustment). The
/// viewport walk cannot mirror that reach — the limit is page-relative —
/// so it hands the selection back to the formatter, which is why the two
/// scroll positions still agree.
#[test]
fn a_selection_ending_on_a_wrapped_wide_placeholder_picks_up_the_grapheme() {
    assert_wrapped_copy(
        "1234567890123456789\u{754c}XY",
        (0, 0),
        (COLS - 1, 0),
        "1234567890123456789\u{754c}",
        "1234567890123456789",
    );
}

/// The three cases the viewport walk used to get wrong. Asserted as exact
/// values in both scroll positions so the behavior itself — not just the
/// agreement — is pinned.
#[test]
fn previously_divergent_cases_now_match_the_formatter() {
    // 1. Wide glyphs: no phantom space for the spacer cell.
    assert_copies_as("\u{4f60}\u{597d}", "\u{4f60}\u{597d}");
    // 2. Leading blank rows are preserved.
    assert_copies_as("\r\nalpha", "\nalpha");
    // 3. A trailing row of only spaces still ends the previous line.
    assert_copies_as("alpha\r\n   ", "alpha\n");
    // 4. A space carrying a combining mark keeps the mark. libghostty's
    //    own `trim` would drop it (it treats the cell as blank and
    //    re-emits a bare space), which is why the formatter is asked for
    //    untrimmed output.
    assert_copies_as("a \u{301}b", "a \u{301}b");
}

/// A selection whose start column lands on a wide grapheme's placeholder
/// reaches back to the grapheme itself; one that starts on the placeholder
/// left behind by a grapheme that wrapped skips that row entirely. Both
/// rules come from the formatter and the viewport walk mirrors them.
#[test]
fn selection_starting_on_a_wide_placeholder_matches_the_formatter() {
    let (mut terminal, mut render_state) = sized_terminal(4096);
    terminal.vt_write("ab\u{754c}cd".as_bytes());
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (3, 0), (5, 0));
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("\u{754c}cd".into())
    );
    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("\u{754c}cd".into())
    );

    let (mut terminal, mut render_state) = sized_terminal(4096);
    // 19 narrow cells then a wide grapheme that cannot fit: column 19
    // is left as a placeholder and the grapheme wraps.
    terminal.vt_write("1234567890123456789\u{754c}".as_bytes());
    let mut selection = TerminalSelection::new();
    select(&mut selection, &terminal, (COLS - 1, 0), (1, 1));
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("\u{754c}".into())
    );
    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("\u{754c}".into())
    );
}

/// Write `input` into a fresh terminal, select all of it, and require
/// `expected` both while it is visible and once it is in scrollback.
fn assert_copies_as(input: &str, expected: &str) {
    let (mut terminal, mut render_state) = sized_terminal(4096);
    terminal.vt_write(input.as_bytes());
    let mut selection = TerminalSelection::new();
    let last_row = u16::try_from(input.matches("\r\n").count()).expect("row count");
    select(&mut selection, &terminal, (0, 0), (COLS - 1, last_row));
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some(expected.into()),
        "visible copy of {input:?}"
    );

    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some(expected.into()),
        "scrollback copy of {input:?}"
    );
}

// ============================================================================
// FFI layout pins
// ============================================================================

/// The formatter structs are sized structs: libghostty reads the `size`
/// field we set but never validates it, so a layout change in a Ghostty
/// bump would be misread silently. Pin the sizes instead.
///
/// Only the structs `formatter.rs` actually fills in are pinned — the
/// one-shot `ghostty_terminal_selection_format_alloc` adoption retired
/// `GhosttyFormatterTerminal{Options,Extra}` and
/// `GhosttyFormatterScreenExtra` from Roost's call path, so pinning them
/// would fail on upstream churn Roost is no longer exposed to.
#[test]
fn formatter_struct_layouts_are_pinned() {
    use std::mem::size_of;

    assert_eq!(size_of::<roost_vt::ffi::GhosttyGridRef>(), 24);
    assert_eq!(size_of::<roost_vt::ffi::GhosttySelection>(), 64);
    assert_eq!(
        size_of::<roost_vt::ffi::GhosttyTerminalSelectionFormatOptions>(),
        24
    );
}
