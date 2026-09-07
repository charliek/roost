//! Contract battery for the history readers `scrollback_rows` and
//! `scrollback_text` — what `tab.dump`'s scrollback fields are made of.
//!
//! Three properties here are what a caller stitching history onto a
//! viewport depends on. The **anchor**: history is measured from the
//! current viewport, so the last row returned is always the one
//! immediately above the viewport's first row, scrolled or not. The
//! **count**: exactly `min(rows, scrollback_rows)` entries come back,
//! blank rows included — libghostty's formatter drops trailing blank
//! rows entirely, so a reader that forwarded its output unpadded would
//! renumber history without saying so. And the **trim**: rows come back
//! byte-identical to what a copy of the same row produces, including a
//! trailing space that carries a combining mark.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{
    scrollback_rows, scrollback_text, RenderState, ScrollViewport, Terminal, TerminalOptions,
};

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn terminal() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: 2000,
        ..Default::default()
    })
    .expect("terminal")
}

/// Numbered lines, each on its own row, cursor left on a fresh row.
fn write_lines(terminal: &mut Terminal, numbers: impl IntoIterator<Item = usize>) {
    for n in numbers {
        terminal.vt_write(format!("line {n}\r\n").as_bytes());
    }
}

fn history(terminal: &Terminal, rows: u32) -> Vec<String> {
    scrollback_text(terminal, rows).expect("scrollback_text")
}

fn history_len(terminal: &Terminal) -> u32 {
    scrollback_rows(terminal).expect("scrollback_rows")
}

/// The viewport as text, the way `tab.dump`'s `rows_text` reads it.
fn viewport(terminal: &Terminal) -> Vec<String> {
    let mut render = RenderState::new().expect("render state");
    render.update(terminal).expect("update");
    let mut rows: Vec<String> = Vec::new();
    render
        .walk(terminal, |row, cell| {
            let row = row as usize;
            if rows.len() <= row {
                rows.resize(row + 1, String::new());
            }
            if cell.text.is_empty() {
                rows[row].push(' ');
            } else {
                rows[row].push_str(&cell.text);
            }
        })
        .expect("walk");
    rows.iter().map(|row| row.trim_end().to_string()).collect()
}

fn numbered(numbers: impl IntoIterator<Item = usize>) -> Vec<String> {
    numbers.into_iter().map(|n| format!("line {n}")).collect()
}

#[test]
fn history_is_contiguous_and_ends_on_the_row_above_the_viewport() {
    let mut terminal = terminal();
    write_lines(&mut terminal, 1..=100);

    let rows = history(&terminal, 50);
    assert_eq!(rows.len(), 50);
    assert_eq!(rows, numbered(28..=77), "contiguous, top to bottom");
    assert_eq!(
        rows.last().expect("50 rows"),
        "line 77",
        "the last row of history sits directly above the viewport"
    );
    assert_eq!(viewport(&terminal)[0], "line 78");
}

#[test]
fn a_request_past_the_history_returns_every_row_there_is() {
    let mut terminal = terminal();
    write_lines(&mut terminal, 1..=100);

    let all = history(&terminal, 10_000);
    assert_eq!(all.len() as u32, history_len(&terminal));
    assert_eq!(all, numbered(1..=77));
}

#[test]
fn a_fresh_terminal_has_no_history() {
    let terminal = terminal();
    assert_eq!(history_len(&terminal), 0);
    assert_eq!(history(&terminal, 50), Vec::<String>::new());
}

#[test]
fn the_alternate_screen_has_no_history() {
    let mut terminal = terminal();
    write_lines(&mut terminal, 1..=100);
    terminal.vt_write(b"\x1b[?1049h");

    assert_eq!(history_len(&terminal), 0);
    assert_eq!(history(&terminal, 50), Vec::<String>::new());
}

/// Blank rows at the bottom of the requested range are the case
/// libghostty's formatter drops on the floor: it defers a row's newline
/// until a later non-blank row, so these come back as nothing at all.
#[test]
fn blank_rows_at_the_bottom_of_history_come_back_as_empty_strings() {
    let mut terminal = terminal();
    write_lines(&mut terminal, 1..=100);
    terminal.vt_write(b"\r\n\r\n\r\n\r\n\r\n");
    write_lines(&mut terminal, 101..=123);
    terminal.vt_write(b"line 124");

    let rows = history(&terminal, 10);
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[..5], numbered(96..=100)[..]);
    assert_eq!(rows[5..], ["", "", "", "", ""]);
    assert_eq!(viewport(&terminal)[0], "line 101");
}

#[test]
fn blank_rows_inside_history_keep_their_positions() {
    let mut terminal = terminal();
    write_lines(&mut terminal, 1..=60);
    terminal.vt_write(b"\r\n\r\n\r\n");
    write_lines(&mut terminal, 61..=99);
    terminal.vt_write(b"line 100");

    let mut expected = numbered(1..=60);
    expected.extend(["".to_string(), "".to_string(), "".to_string()]);
    expected.extend(numbered(61..=76));

    let rows = history(&terminal, 10_000);
    assert_eq!(rows.len() as u32, history_len(&terminal));
    assert_eq!(rows, expected);
}

#[test]
fn an_all_blank_history_is_empty_strings_not_an_empty_vector() {
    let mut terminal = terminal();
    terminal.vt_write(&b"\r\n".repeat(30));

    let top = history_len(&terminal);
    assert!(top > 0, "30 blank rows scroll some of them out of view");
    assert_eq!(history(&terminal, top), vec![String::new(); top as usize]);
    assert_eq!(history(&terminal, 3), vec![String::new(); 3]);
}

/// libghostty's own trim treats any cell whose base codepoint is a space
/// as blank, so it would drop this cell and its mark. The reader asks for
/// no trim and removes bare spaces itself, which keeps the mark and keeps
/// a dumped row equal to a copied one.
#[test]
fn a_trailing_space_carrying_a_combining_mark_survives() {
    let mut terminal = terminal();
    terminal.vt_write("abc \u{0301}\r\n".as_bytes());
    write_lines(&mut terminal, 1..=40);

    assert_eq!(history(&terminal, 10_000)[0], "abc \u{0301}");
}

#[test]
fn a_scrolled_viewport_moves_the_history_anchor_with_it() {
    let mut terminal = terminal();
    write_lines(&mut terminal, 1..=100);
    terminal.scroll_viewport(ScrollViewport::Delta(-40));

    let offset = terminal.scrollbar().expect("scrollbar").offset;
    assert_eq!(u64::from(history_len(&terminal)), offset);
    assert_eq!(viewport(&terminal)[0], "line 38");

    let rows = history(&terminal, 10);
    assert_eq!(rows.len(), 10);
    assert_eq!(rows, numbered(28..=37));
}

#[test]
fn scrolled_to_the_very_top_there_is_no_history_left() {
    let mut terminal = terminal();
    write_lines(&mut terminal, 1..=100);
    terminal.scroll_viewport(ScrollViewport::Top);

    assert_eq!(history_len(&terminal), 0);
    assert_eq!(history(&terminal, 50), Vec::<String>::new());
    assert_eq!(viewport(&terminal)[0], "line 1");
}
