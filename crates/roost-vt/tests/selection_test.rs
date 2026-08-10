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
    assert!(selection.set(&terminal, (0, 0), (4, 0)));
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
    assert!(selection.set(&terminal, (0, 0), (2, 0)));
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
    assert!(selection.set(&terminal, (0, 0), (2, 0)));
    assert!(!selection.begin(&terminal, 0, u16::MAX));
    assert!(!selection.is_active());
}

#[test]
fn multi_row_selection_yields_shared_spans_and_text() {
    let (mut terminal, mut render_state) = terminal();
    terminal.vt_write(b"abc\r\ndef\r\nghi");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (1, 0), (1, 2)));
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
    assert!(selection.begin(&terminal, 2, 1));
    assert!(selection.visible_spans(&terminal, 20, 5).is_empty());
    assert!(selection.update(&terminal, 4, 1));
    assert_eq!(
        selection.visible_spans(&terminal, 20, 5),
        vec![roost_vt::SelectionSpan {
            row: 1,
            col0: 2,
            col1: 5,
        }]
    );
    assert!(!selection.update(&terminal, 4, u16::MAX));
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
    assert!(selection.set(&terminal, (0, 0), (6, 2)));
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
    assert!(selection.set(&terminal, (0, 0), (4, 3)));

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
    assert!(selection.begin(&terminal, 0, 0));
    terminal.scroll_viewport(ScrollViewport::Bottom);
    assert!(selection.update(&terminal, 0, ROWS - 1));

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
    assert!(selection.set(&terminal, (6, 2), (0, 0)));
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
    assert!(selection.set(&terminal, (0, 0), (COLS - 1, 1)));
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
    assert!(selection.set(&terminal, (0, 0), (COLS - 1, 2)));
    scroll_out(&mut terminal, 30);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("alpha\n\nbravo".into())
    );
}

/// Honest limit: screen coordinates are relative to the top of the page
/// list, so an evicted row shifts every stored endpoint by one. libghostty
/// has tracked pins that would follow the content, but
/// `ghostty_terminal_grid_ref` hands back untracked pins and no tracking
/// symbol is exported. This fix cures clipping, not drift — the assertion
/// below pins the drift so a future libghostty bump that fixes it is
/// noticed.
#[test]
fn evicted_history_drifts_rather_than_failing() {
    let (mut terminal, mut render_state) = sized_terminal(0);
    terminal.vt_write(b"MARKER\r\n");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (0, 0), (COLS - 1, 0)));
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("MARKER".into())
    );

    write_lines(&mut terminal, 50);
    assert_eq!(
        text(&selection, &terminal, &mut render_state),
        Some("line46".into())
    );
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
    assert!(selection.set(&terminal, (0, 2), (COLS - 1, ROWS - 1)));
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
    assert!(selection.set(&terminal, (3, 0), (5, 0)));
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
    assert!(selection.set(&terminal, (COLS - 1, 0), (1, 1)));
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
    assert!(selection.set(&terminal, (0, 0), (COLS - 1, last_row)));
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
#[test]
fn formatter_struct_layouts_are_pinned() {
    use std::mem::size_of;

    assert_eq!(size_of::<roost_vt::ffi::GhosttyGridRef>(), 24);
    assert_eq!(size_of::<roost_vt::ffi::GhosttySelection>(), 64);
    assert_eq!(size_of::<roost_vt::ffi::GhosttyFormatterScreenExtra>(), 16);
    assert_eq!(
        size_of::<roost_vt::ffi::GhosttyFormatterTerminalExtra>(),
        32
    );
    assert_eq!(
        size_of::<roost_vt::ffi::GhosttyFormatterTerminalOptions>(),
        56
    );
}
