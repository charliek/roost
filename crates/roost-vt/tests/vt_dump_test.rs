//! Fidelity corpus for [`vt_snapshot`] — the `vt` attach payload.
//!
//! Every case builds a server terminal, drives it with `vt_write`,
//! replays the payload into a fresh client terminal of the same
//! geometry, and asserts the two are the same terminal by every measure
//! a `vt` client renders from: each viewport row's `RenderedRow`
//! projection, the effective colours and the palette, the carried modes,
//! the cursor (position, visibility, pending wrap, shape, blink),
//! `scrollbar().total` and the scrollback text. Then it writes the same
//! further bytes to both and asserts all of it again — which for the
//! fence-cut cases is what completes the interrupted sequence.
//!
//! The payload's failure mode is a screen that is subtly wrong rather
//! than an exception, so the corpus is the specification: composition
//! order, the pad, the background refill and the origin-mode
//! re-position are each pinned by a case that fails loudly without them.
//!
//! Two divergences are asserted **as** divergences — per-cell hyperlinks
//! and background-only rows in history — so a libghostty pin bump that
//! fixes either one fails here instead of passing silently.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use std::sync::{Arc, Mutex};

use roost_vt::{
    key_action, mods, mouse_action, mouse_button, scrollback_text, vt_snapshot, ActiveScreen,
    ColorRgb, CursorVisualStyle, KeyEncoder, KeyEvent, MouseEncoder, MouseEvent, RenderState,
    RenderedRow, ScrollViewport, Terminal, TerminalColor, TerminalOptions,
};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const SCROLLBACK: usize = 2000;
/// What the server-side tab task retains, so a cut sequence of any
/// realistic length is available to the payload.
const CONTINUATION_MAX: usize = 1 << 20;

// ============================================================================
// The comparison
// ============================================================================

#[derive(Debug, PartialEq, Eq)]
struct Cell {
    col: u16,
    text: String,
    foreground: ColorRgb,
    background: ColorRgb,
    explicit_background: bool,
    bold: bool,
    italic: bool,
    inverse: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Row {
    text: String,
    cells: Vec<Cell>,
}

#[derive(Debug, PartialEq, Eq)]
struct Cursor {
    row: u32,
    col: u32,
    visible: bool,
    blinking: bool,
    style: CursorVisualStyle,
    pending_wrap: bool,
}

/// Everything the corpus compares, read in one pass so the halves cannot
/// describe two different instants.
#[derive(Debug)]
struct Mirror {
    rows: Vec<Row>,
    colors: [Option<ColorRgb>; 3],
    palette: Vec<ColorRgb>,
    modes: Vec<(String, bool)>,
    mouse_tracking: bool,
    screen: ActiveScreen,
    cursor: Option<Cursor>,
    total_rows: u64,
    history: Vec<String>,
    pwd: Option<String>,
}

/// Modes the corpus compares: the carried allowlist plus the three
/// handled outside it — the alternate screen, origin mode and IRM.
const COMPARED_DEC_MODES: &[u16] = &[
    1, 5, 6, 7, 9, 25, 45, 47, 66, 69, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016, 1036,
    1039, 1047, 1049, 2004, 2027,
];
const COMPARED_ANSI_MODES: &[u16] = &[4, 12, 20];

struct Vt {
    terminal: Terminal,
    render: RenderState,
    cols: u16,
    rows: u16,
}

impl Vt {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            terminal: Terminal::new(TerminalOptions {
                cols,
                rows,
                max_scrollback: SCROLLBACK,
                continuation_max_bytes: CONTINUATION_MAX,
            })
            .expect("terminal"),
            render: RenderState::new().expect("render state"),
            cols,
            rows,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    fn payload(&mut self) -> Vec<u8> {
        vt_snapshot(&self.terminal, &mut self.render)
            .expect("vt_snapshot")
            .expect("the parser is at ground, so a payload is encodable")
    }

    fn viewport(&mut self) -> Vec<Row> {
        self.render.update(&self.terminal).expect("update");
        let colors = self.render.colors().expect("colors");
        let defaults = (colors.foreground, colors.background);
        self.render.mark_full().expect("mark_full");
        let mut rows: Vec<Row> = Vec::new();
        self.render
            .walk_dirty(&self.terminal, |row, cells| {
                // `walk_dirty` hands the row's complete cell slice, so
                // its length is the grid width even after a DECCOLM.
                let built = RenderedRow::build(cells, defaults, cells.len() as u16);
                while rows.len() <= row as usize {
                    rows.push(Row::default());
                }
                rows[row as usize] = Row {
                    text: built.text,
                    cells: built
                        .cells
                        .into_iter()
                        .map(|cell| Cell {
                            col: cell.col,
                            text: cell.text,
                            foreground: cell.foreground,
                            background: cell.background,
                            explicit_background: cell.explicit_background,
                            bold: cell.bold,
                            italic: cell.italic,
                            inverse: cell.inverse,
                        })
                        .collect(),
                };
            })
            .expect("walk_dirty");
        rows
    }

    fn mirror(&mut self) -> Mirror {
        let rows = self.viewport();
        let colors = [
            TerminalColor::Foreground,
            TerminalColor::Background,
            TerminalColor::Cursor,
        ]
        .map(|which| self.terminal.color(which).expect("color"));
        let mut modes: Vec<(String, bool)> = COMPARED_DEC_MODES
            .iter()
            .map(|mode| (format!("?{mode}"), self.terminal.mode_get(*mode)))
            .collect();
        modes.extend(
            COMPARED_ANSI_MODES
                .iter()
                .map(|mode| (mode.to_string(), self.terminal.ansi_mode_get(*mode))),
        );
        let cursor = self.render.cursor().map(|cursor| Cursor {
            row: cursor.row,
            col: cursor.col,
            visible: cursor.visible,
            blinking: cursor.blinking,
            style: cursor.visual_style,
            pending_wrap: self.terminal.cursor_pending_wrap(),
        });
        Mirror {
            rows,
            colors,
            palette: self.terminal.live_palette().expect("palette").to_vec(),
            modes,
            mouse_tracking: self.terminal.mouse_tracking(),
            screen: self.terminal.active_screen(),
            cursor,
            total_rows: self.terminal.scrollbar().expect("scrollbar").total,
            history: scrollback_text(&self.terminal, SCROLLBACK as u32).expect("scrollback_text"),
            pwd: self.terminal.pwd().expect("pwd"),
        }
    }
}

/// Compares everything a `vt` client renders from — with one hole:
/// history is compared as the plain text [`scrollback_text`] reads, so a
/// style dropped in scrollback is invisible here. That is the reader's
/// shape, not an omission to fix.
fn assert_mirrors(server: &mut Vt, client: &mut Vt, label: &str) {
    let expected = server.mirror();
    let actual = client.mirror();
    assert_eq!(actual.rows, expected.rows, "{label}: viewport rows");
    assert_eq!(
        actual.colors, expected.colors,
        "{label}: effective fg/bg/cursor"
    );
    assert_eq!(actual.palette, expected.palette, "{label}: palette");
    assert_eq!(actual.modes, expected.modes, "{label}: modes");
    assert_eq!(
        actual.mouse_tracking, expected.mouse_tracking,
        "{label}: mouse tracking"
    );
    assert_eq!(actual.screen, expected.screen, "{label}: active screen");
    assert_eq!(actual.cursor, expected.cursor, "{label}: cursor");
    assert_eq!(
        actual.total_rows, expected.total_rows,
        "{label}: scrollbar total"
    );
    assert_eq!(actual.history, expected.history, "{label}: scrollback text");
    assert_eq!(actual.pwd, expected.pwd, "{label}: working directory");
}

/// Replay `server`'s payload into a fresh client of `geometry`, assert
/// the mirror, then write `more` to both and assert it again.
fn replays_at(server: &mut Vt, geometry: (u16, u16), more: &[u8], label: &str) -> Vt {
    let payload = server.payload();
    let mut client = Vt::new(geometry.0, geometry.1);
    client.write(&payload);
    assert_mirrors(server, &mut client, label);

    server.write(more);
    client.write(more);
    assert_mirrors(
        server,
        &mut client,
        &format!("{label}, after further input"),
    );
    client
}

fn replays(server: &mut Vt, more: &[u8], label: &str) -> Vt {
    let geometry = (server.cols, server.rows);
    replays_at(server, geometry, more, label)
}

fn fresh() -> Vt {
    Vt::new(COLS, ROWS)
}

// ============================================================================
// Content, alignment and history
// ============================================================================

#[test]
fn an_empty_terminal() {
    let mut server = fresh();
    replays(&mut server, b"first input", "empty");
}

/// The pad's reason for existing: the formatter trims trailing blank
/// rows, so a screen with history and eleven blank rows at its bottom
/// replays eleven rows short — the client scrolls eleven times too few,
/// its viewport shows history at the top, and every absolute address
/// after the content lands on the wrong row.
#[test]
fn a_screen_with_history_whose_bottom_rows_are_blank() {
    let mut server = fresh();
    for line in 1..=30 {
        server.write(format!("line {line}\r\n").as_bytes());
    }
    // Erase from row 14 down, leaving history above and blank rows below.
    server.write(b"\x1b[14;1H\x1b[0J");
    let scrollbar = server.terminal.scrollbar().expect("scrollbar");
    assert!(scrollbar.offset > 0, "there is history above the viewport");

    replays(&mut server, b"after the blanks", "bottom rows blank");
}

#[test]
fn a_history_past_eviction_with_a_blank_run_at_a_page_boundary() {
    let mut server = fresh();
    for line in 1..=1050 {
        server.write(format!("line {line}\r\n").as_bytes());
    }
    // A run of blank rows well inside the retained history: the
    // formatter defers a blank row's newline, so a mis-counted pad shows
    // up here as history that has slid by five rows.
    server.write(b"\r\n\r\n\r\n\r\n\r\n");
    for line in 1051..=2100 {
        server.write(format!("line {line}\r\n").as_bytes());
    }
    let scrollbar = server.terminal.scrollbar().expect("scrollbar");
    assert!(
        scrollbar.total < 2105 + u64::from(ROWS),
        "the corpus needs eviction to have happened, total was {}",
        scrollbar.total
    );
    assert!(scrollbar.offset > 1000, "and plenty of history to compare");

    replays(&mut server, b"tail", "2000+ lines with a blank run");
}

#[test]
fn wide_graphemes_including_one_at_the_last_column() {
    let mut server = fresh();
    server.write("中文 mixed 🙂 emoji\r\n".as_bytes());
    // 79 narrow cells leave one column, so the wide char takes a spacer
    // head at the right edge and wraps.
    server.write("x".repeat(79).as_bytes());
    server.write("漢".as_bytes());
    replays(&mut server, "字".as_bytes(), "wide graphemes");
}

// ============================================================================
// Cursor state
// ============================================================================

#[test]
fn a_cursor_with_pending_wrap_at_the_last_column() {
    let mut server = fresh();
    server.write("a".repeat(usize::from(COLS)).as_bytes());
    assert!(
        server.terminal.cursor_pending_wrap(),
        "a full row leaves the wrap pending"
    );
    replays(&mut server, b"B", "pending wrap");
}

/// A wide char that *fits* the last two columns leaves the wrap pending
/// on its spacer tail — the one state where the pre-position's widening
/// and the formatter's own pending-wrap re-print both have to start from
/// the wide char's head.
#[test]
fn a_wide_grapheme_ending_at_the_last_column_with_pending_wrap() {
    let mut server = fresh();
    server.write("x".repeat(usize::from(COLS) - 2).as_bytes());
    server.write("漢".as_bytes());
    assert!(
        server.terminal.cursor_pending_wrap(),
        "the wide char fills the row"
    );
    server.render.update(&server.terminal).expect("update");
    assert!(
        server.render.cursor().expect("cursor").wide_tail,
        "the cursor sits on the spacer tail"
    );
    replays(
        &mut server,
        "字".as_bytes(),
        "wide grapheme at the last column",
    );
}

#[test]
fn a_cursor_on_a_wide_tail() {
    let mut server = fresh();
    server.write("中".as_bytes());
    server.write(b"\x1b[1;2H");
    server.render.update(&server.terminal).expect("update");
    assert!(
        server.render.cursor().expect("cursor").wide_tail,
        "column 1 is the spacer tail of the wide char"
    );
    replays(&mut server, b"X", "cursor on a wide tail");
}

#[test]
fn decawm_off_with_the_cursor_at_the_last_column() {
    let mut server = fresh();
    server.write(b"\x1b[?7l");
    server.write("a".repeat(usize::from(COLS)).as_bytes());
    // libghostty flags the deferred wrap either way and consults DECAWM
    // only when the next glyph arrives, so the payload has to carry both
    // the flag and the mode: `Z` overwrites the last column here, where
    // with wraparound on it would start row 1.
    assert!(server.terminal.cursor_pending_wrap());
    let mut client = replays(&mut server, b"Z", "DECAWM off at the last column");
    assert_eq!(client.viewport()[1].text, "", "nothing wrapped");
}

#[test]
fn a_blinking_bar_cursor() {
    let mut server = fresh();
    server.write(b"\x1b[5 q");
    server.render.update(&server.terminal).expect("update");
    let cursor = server.render.cursor().expect("cursor");
    assert_eq!(cursor.visual_style, CursorVisualStyle::Bar);
    assert!(cursor.blinking);
    replays(&mut server, b"typed", "DECSCUSR blinking bar");
}

#[test]
fn a_hidden_cursor() {
    let mut server = fresh();
    server.write(b"\x1b[?25l");
    replays(&mut server, b"", "hidden cursor");
}

// ============================================================================
// Screens, regions and modes
// ============================================================================

#[test]
fn the_alternate_screen() {
    let mut server = fresh();
    for line in 1..=40 {
        server.write(format!("primary {line}\r\n").as_bytes());
    }
    server.write(b"\x1b[?1049h");
    server.write(b"alternate content\r\nsecond row");
    replays(&mut server, b" more", "alternate screen");
}

#[test]
fn a_scrolling_region() {
    let mut server = fresh();
    for line in 1..=20 {
        server.write(format!("\x1b[{line};1Hrow {line}").as_bytes());
    }
    server.write(b"\x1b[3;10r\x1b[10;1Hbottom of the region");
    // A linefeed at the region's last row must move rows 3..10 and leave
    // everything above and below alone — which only happens if pass B's
    // DECSTBM replayed.
    replays(&mut server, b"\r\nscrolled", "DECSTBM region");
}

/// Setting origin mode homes the cursor, so the position pass B
/// restored has to be re-issued — region-relative, because that is how a
/// CUP reads under DECOM.
#[test]
fn origin_mode_with_a_region_and_the_cursor_inside_it() {
    let mut server = fresh();
    server.write(b"\x1b[3;10r\x1b[?6h\x1b[2;5Hinside");
    server.render.update(&server.terminal).expect("update");
    let cursor = server.render.cursor().expect("cursor");
    assert_eq!((cursor.row, cursor.col), (3, 10), "row 3, past \"inside\"");
    replays(&mut server, b"\r\nnext", "DECOM inside a region");
}

#[test]
fn origin_mode_with_left_and_right_margins() {
    let mut server = fresh();
    server.write(b"\x1b[?69h\x1b[3;10r\x1b[20;60s\x1b[?6h\x1b[2;5Hboxed");
    // Both margins have to be *compared*, not just the origin they give
    // the CUPs: the digits are long enough to wrap at the right margin
    // (back to the left one, not to column 1) and the linefeeds carry
    // the text past the region's bottom row, so a wrong DECSLRM or a
    // wrong DECSTBM leaves a different screen.
    let mut more = "0123456789".repeat(12);
    more.push_str("\r\nmid\r\n\r\n\r\n\r\n\r\n\r\ntail");
    replays(&mut server, more.as_bytes(), "DECOM with DECSLRM");
}

/// Origin mode's re-position is the one CUP that must name the cursor's
/// *true* column. The pre-position and pass B's one-cell selection widen
/// a spacer tail to the wide char's head; pass B's cursor extra then
/// restores the true column, and re-issuing the widened one after
/// `CSI ?6 h` would land a column left of the server's cursor.
#[test]
fn origin_mode_with_the_cursor_on_a_wide_tail() {
    let mut server = fresh();
    server.write(b"\x1b[?6h");
    server.write("中".as_bytes());
    server.write(b"\x1b[1;2H");
    server.render.update(&server.terminal).expect("update");
    assert!(
        server.render.cursor().expect("cursor").wide_tail,
        "column 1 is the spacer tail of the wide char"
    );
    // `X` replaces the wide char from its tail on the server; from the
    // head on a client that landed one column left.
    replays(&mut server, b"X", "DECOM with the cursor on a wide tail");
}

#[test]
fn insert_mode() {
    let mut server = fresh();
    server.write(b"tail text\x1b[1;1H\x1b[4h");
    assert!(server.terminal.ansi_mode_get(4), "IRM is on");
    replays(&mut server, b"head ", "IRM");
}

#[test]
fn mouse_tracking_bracketed_paste_focus_events_and_margins() {
    let mut server = fresh();
    server.write(b"\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?69h\x1b[10;70s");
    assert!(server.terminal.mouse_tracking(), "1002 arms tracking");
    let mut client = replays(&mut server, b"content", "mouse + paste + focus + margins");

    // The mode bits alone would not catch this: libghostty collapses the
    // 1000/1002/1003 trio into one tracking mode and the four report
    // formats into one, so a replay that clears a family member after
    // setting it leaves the bits right and the encoder wrong.
    assert_eq!(
        mouse_report(&mut client.terminal),
        mouse_report(&mut server.terminal),
        "the client encodes a mouse press the way the server does"
    );
    assert!(
        mouse_report(&mut server.terminal).starts_with(b"\x1b[<"),
        "1006 selects the SGR form"
    );
}

/// What the terminal's negotiated tracking mode and report format turn a
/// left-button press into.
fn mouse_report(terminal: &mut Terminal) -> Vec<u8> {
    let mut encoder = MouseEncoder::new().expect("encoder");
    encoder.sync_from_terminal(terminal);
    encoder.set_size(800, 480, 10, 20);
    let mut event = MouseEvent::new().expect("event");
    event.set_action(mouse_action::PRESS);
    event.set_button(mouse_button::LEFT);
    event.set_mods(0);
    event.set_position(50.0, 40.0);
    encoder.encode(&event).unwrap_or_default()
}

#[test]
fn custom_tabstops() {
    let mut server = fresh();
    server.write(b"\x1b[3g\x1b[1;5H\x1bH\x1b[1;33H\x1bH\x1b[1;1H");
    // The further input is what proves the stops replayed: each tab has
    // to land on the same column on both sides.
    replays(&mut server, b"a\tb\tc", "custom tabstops");
}

#[test]
fn dec_special_graphics_g0() {
    let mut server = fresh();
    server.write(b"\x1b(0qqqlkmj");
    replays(&mut server, b"x", "DEC special graphics G0");
}

#[test]
fn a_working_directory_set_via_osc_7() {
    let mut server = fresh();
    server.write(b"\x1b]7;file://host/home/roost\x1b\\");
    replays(&mut server, b"", "working directory");
}

#[test]
fn kitty_keyboard_flags() {
    let mut server = fresh();
    server.write(b"\x1b[>5u");
    let mut client = replays(&mut server, b"", "kitty keyboard flags");

    assert_eq!(
        kitty_flags(&mut server.terminal),
        kitty_flags(&mut client.terminal),
        "the client answers the kitty keyboard query the way the server does"
    );
}

/// The other half of pass B's `keyboard` extra: `modifyOtherKeys`
/// changes what a plain Ctrl chord encodes to, so a client that missed
/// it sends different bytes for the same keystroke.
#[test]
fn modify_other_keys() {
    let mut server = fresh();
    server.write(b"\x1b[>4;2m");
    let client = replays(&mut server, b"", "modifyOtherKeys");

    let expected = ctrl_shift_h(&server.terminal);
    assert_eq!(ctrl_shift_h(&client.terminal), expected);
    assert_eq!(
        expected, b"\x1b[27;6;72~",
        "modifyOtherKeys 2 turns Ctrl+Shift+H into a CSI 27 report"
    );
}

fn ctrl_shift_h(terminal: &Terminal) -> Vec<u8> {
    let mut encoder = KeyEncoder::new().expect("encoder");
    encoder.sync_from_terminal(terminal);
    let mut event = KeyEvent::new().expect("event");
    event.set_action(key_action::PRESS);
    event.set_key(roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_H);
    event.set_mods(mods::CTRL | mods::SHIFT);
    event.set_composing(false);
    event.set_utf8(b"H");
    encoder.encode(&event).unwrap_or_default()
}

/// `CSI ? u` is the only readback for the flag stack; the reply lands in
/// the `write_pty` buffer.
fn kitty_flags(terminal: &mut Terminal) -> Vec<u8> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    terminal
        .set_write_pty_buffer(buf.clone())
        .expect("write_pty buffer");
    terminal.vt_write(b"\x1b[?u");
    let reply = std::mem::take(&mut *buf.lock().expect("lock"));
    terminal.clear_write_pty().expect("clear write_pty");
    reply
}

/// DECCOLM is deliberately outside the allowlist: replaying it would
/// resize the client away from the geometry it attached at.
#[test]
fn deccolm_is_not_carried() {
    let mut server = Vt::new(COLS, ROWS);
    server.write(b"\x1b[?40h\x1b[?3hwide screen");
    assert!(
        server.terminal.mode_get(3),
        "the server took the 132 columns"
    );

    let payload = server.payload();
    assert!(
        !contains(&payload, b"\x1b[?3h") && !contains(&payload, b"\x1b[?3l"),
        "the payload must never address DECCOLM"
    );

    // The client attaches at the server's current geometry and keeps it.
    let client = replays_at(&mut server, (132, ROWS), b" typed", "DECCOLM");
    assert!(
        !client.terminal.mode_get(3),
        "the mode itself is a pinned divergence: carrying it would resize \
         the client"
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

// ============================================================================
// Styles and colours
// ============================================================================

/// The pen is state, not content: nothing in the grid shows what the
/// next print will look like, so only pass B's `style` extra carries it.
#[test]
fn a_non_default_pen_at_the_snapshot() {
    let mut server = fresh();
    // Set *after* the content, so the grid the client rebuilds from is
    // all-default and the pen is the only thing left to get wrong.
    server.write(b"plain\x1b[1;3;7;31;44m");
    replays(&mut server, b" styled", "a non-default pen");
}

#[test]
fn sgr_colors_and_attributes() {
    let mut server = fresh();
    server.write(b"\x1b[1;31;44mbold red on blue\x1b[0m\r\n");
    server.write(b"\x1b[3mitalic\x1b[0m\r\n");
    server.write(b"\x1b[7minverse\x1b[0m\r\n");
    // Underline is outside the `DrawCell` projection (`Style` drops it),
    // so it rides along as content rather than as an assertion.
    server.write(b"\x1b[4munderline\x1b[0m\r\n");
    server.write(b"\x1b[38;2;10;20;30;48;2;40;50;60mtruecolor\x1b[0m");
    replays(&mut server, b"\r\nplain", "SGR colours and attributes");
}

/// A row erased under a background colour has no text at all, so the
/// formatter's blank-row predicate drops it. Viewport rows are refilled
/// with ECH runs.
#[test]
fn a_background_only_row_in_the_viewport_is_refilled() {
    let mut server = fresh();
    server.write(b"header\r\n\x1b[44m\x1b[2K\x1b[0m\x1b[3;1Hbody");
    let mut client = replays(&mut server, b"!", "background-only viewport row");

    let row = &client.viewport()[1];
    assert!(row.text.is_empty(), "the status row carries no text");
    assert_eq!(row.cells.len(), usize::from(COLS));
    assert!(row.cells.iter().all(|cell| cell.explicit_background));
}

/// The refill fires only where the formatter really dropped the row. A
/// row of *spaces* under a pen trims to empty `text` too, but a space is
/// text: the formatter emits those cells itself, and an ECH over them
/// would erase everything but their background.
#[test]
fn a_row_of_styled_spaces_is_left_to_the_formatter() {
    let mut server = fresh();
    server.write(b"header\r\n\x1b[31;44m");
    server.write(" ".repeat(40).as_bytes());
    server.write(b"\x1b[0m\x1b[3;1Hbody");
    let styled = server.viewport()[1].cells[0].foreground;

    let mut client = replays(&mut server, b"!", "a row of styled spaces");
    let row = &client.viewport()[1];
    assert!(row.text.is_empty(), "trailing spaces trim to nothing");
    assert_eq!(
        row.cells.len(),
        40,
        "the spaces are cells, background and all"
    );
    assert!(
        row.cells.iter().all(|cell| cell.foreground == styled),
        "an ECH over them would have dropped the pen's foreground"
    );
}

/// Step 0's reason, applied to the refill: a row erased under `SGR 44`
/// has to replay as slot 4 and pick up the *client's* blue, exactly as
/// the ordinary text rows around it (which pass A emits as `CSI 44 m`)
/// do. Emitting the server's resolved RGB would leave one row wearing
/// the server's theme on a client whose theme differs.
#[test]
fn a_background_only_row_replays_as_a_palette_slot_not_a_resolved_rgb() {
    let mut server = fresh();
    let server_theme = solid_palette(0x10);
    server
        .terminal
        .set_color_palette(&server_theme)
        .expect("palette");
    server.write(b"header\r\n\x1b[44m\x1b[2K\x1b[0m\x1b[3;1Hbody");
    let payload = server.payload();

    let client_theme = solid_palette(0x90);
    let mut client = fresh();
    client
        .terminal
        .set_color_palette(&client_theme)
        .expect("palette");
    client.write(&payload);

    assert_ne!(
        server_theme[4], client_theme[4],
        "the themes disagree on blue"
    );
    let row = &client.viewport()[1];
    assert_eq!(row.cells.len(), usize::from(COLS));
    assert!(
        row.cells
            .iter()
            .all(|cell| cell.background == client_theme[4]),
        "the refill wears the client's own slot 4"
    );
}

/// The same row in history is a **known limitation**: there is no cursor
/// addressing into scrollback, so nothing can refill it. Pinned so a
/// libghostty bump that starts emitting the fill fails here.
#[test]
fn a_background_only_row_in_history_is_a_known_divergence() {
    let mut server = fresh();
    server.write(b"\x1b[44m\x1b[2K\x1b[0m\r\n");
    for line in 1..=40 {
        server.write(format!("line {line}\r\n").as_bytes());
    }

    let mut client = replays(&mut server, b"", "background-only history row");

    // Scroll both to the very top, where the erased row is row 0.
    server.terminal.scroll_viewport(ScrollViewport::Top);
    client.terminal.scroll_viewport(ScrollViewport::Top);
    let expected = &server.viewport()[0];
    let actual = &client.viewport()[0];
    assert_eq!(
        expected.cells.len(),
        usize::from(COLS),
        "server keeps the fill"
    );
    assert!(
        actual.cells.is_empty(),
        "the fill is lost in history — if this now matches, the limitation \
         is gone and `vt_dump.rs` should say so"
    );
    assert_eq!(actual.text, expected.text, "the text still matches");
}

#[test]
fn program_color_overrides() {
    let mut server = fresh();
    server.write(b"\x1b]4;3;rgb:aa/bb/cc\x1b\\");
    server.write(b"\x1b]10;rgb:11/22/33\x1b\\");
    server.write(b"\x1b]11;rgb:44/55/66\x1b\\");
    server.write(b"\x1b]12;rgb:77/88/99\x1b\\");
    replays(&mut server, b"colored", "OSC 4 / 10 / 11 / 12 overrides");
}

/// The reason step 0 carries overrides and not the palette: the client
/// owns its theme, and only the slots the *program* changed may move.
#[test]
fn the_client_keeps_its_own_theme_except_the_slots_the_program_overrode() {
    let mut server = fresh();
    let server_theme = solid_palette(0x10);
    server
        .terminal
        .set_color_palette(&server_theme)
        .expect("palette");
    server
        .terminal
        .set_color_foreground(ColorRgb::new(0xcc, 0xcc, 0xcc))
        .expect("fg");
    server
        .terminal
        .set_color_background(ColorRgb::new(0x11, 0x11, 0x11))
        .expect("bg");
    // Only slot 3 and the background are the program's doing.
    server.write(b"\x1b]4;3;rgb:aa/bb/cc\x1b\\\x1b]11;rgb:44/55/66\x1b\\");
    let payload = server.payload();

    let client_theme = solid_palette(0x90);
    let client_fg = ColorRgb::new(0x33, 0x44, 0x55);
    let client_bg = ColorRgb::new(0x66, 0x77, 0x88);
    let mut client = fresh();
    client
        .terminal
        .set_color_palette(&client_theme)
        .expect("palette");
    client.terminal.set_color_foreground(client_fg).expect("fg");
    client.terminal.set_color_background(client_bg).expect("bg");
    client.write(&payload);

    let palette = client.terminal.live_palette().expect("palette");
    assert_eq!(
        palette[3],
        ColorRgb::new(0xaa, 0xbb, 0xcc),
        "the program's OSC 4 override rides the payload"
    );
    for slot in [0usize, 1, 2, 4, 200, 255] {
        assert_eq!(
            palette[slot], client_theme[slot],
            "slot {slot} is the client's own theme, untouched"
        );
    }
    assert_eq!(
        client
            .terminal
            .color(TerminalColor::Background)
            .expect("bg"),
        Some(ColorRgb::new(0x44, 0x55, 0x66)),
        "the program's OSC 11 override rides the payload"
    );
    assert_eq!(
        client
            .terminal
            .color(TerminalColor::Foreground)
            .expect("fg"),
        Some(client_fg),
        "the server's theme foreground must not travel"
    );
}

fn solid_palette(seed: u8) -> [ColorRgb; 256] {
    std::array::from_fn(|slot| ColorRgb::new(seed, slot as u8, seed ^ 0x5a))
}

// ============================================================================
// Hyperlinks
// ============================================================================

/// The pen's hyperlink is carried by pass B's `OSC 8` extra; the cells'
/// own links are not, because VT content emits `OSC 8` only for HTML.
/// Pinned as a divergence so a pin bump that emits them is noticed.
#[test]
fn a_hyperlink_pen_carries_but_hyperlinked_cells_are_a_known_divergence() {
    let mut server = fresh();
    server.write(b"\x1b]8;;https://example.com\x1b\\link");

    let payload = server.payload();
    let mut client = fresh();
    client.write(&payload);

    assert_eq!(
        server.terminal.hyperlink_at(0, 0).as_deref(),
        Some("https://example.com")
    );
    assert_eq!(
        client.terminal.hyperlink_at(0, 0),
        None,
        "per-cell links are lost — if this now matches, the limitation is \
         gone and `vt_dump.rs` should say so"
    );

    // The pen, though, is live on both: what either side prints next is
    // still inside the span.
    server.write(b"e");
    client.write(b"e");
    assert_eq!(
        client.terminal.hyperlink_at(4, 0).as_deref(),
        server.terminal.hyperlink_at(4, 0).as_deref(),
        "the cursor's own hyperlink state replays"
    );
    assert_eq!(
        client.terminal.hyperlink_at(4, 0).as_deref(),
        Some("https://example.com")
    );
}

// ============================================================================
// Fences that cut a sequence
// ============================================================================

/// The payload ends with libghostty's retained continuation, so the
/// client sits in the same parser state and the first live frame
/// finishes the sequence on both sides identically.
fn a_cut_sequence(cut: &[u8], rest: &[u8], label: &str) {
    let mut server = fresh();
    server.write(b"before the cut\r\n");
    server.write(cut);
    assert_eq!(
        server
            .terminal
            .continuation()
            .expect("continuation")
            .as_deref(),
        Some(cut),
        "{label}: libghostty retains exactly the cut bytes"
    );
    replays(&mut server, rest, label);
}

#[test]
fn a_fence_cutting_a_utf8_sequence() {
    a_cut_sequence(&"漢".as_bytes()[..2], &"漢".as_bytes()[2..], "cut UTF-8");
}

#[test]
fn a_fence_cutting_an_escape() {
    a_cut_sequence(b"\x1b", b"[31mred", "cut ESC");
}

#[test]
fn a_fence_cutting_a_csi() {
    a_cut_sequence(b"\x1b[3", b"1mred", "cut CSI");
}

#[test]
fn a_fence_cutting_an_osc() {
    a_cut_sequence(b"\x1b]0;win", b"dow\x1b\\after", "cut OSC");
}

#[test]
fn a_fence_cutting_a_dcs() {
    a_cut_sequence(b"\x1bP0;1|17", b"/ab\x1b\\after", "cut DCS");
}

#[test]
fn a_fence_cutting_an_apc() {
    a_cut_sequence(b"\x1b_Gf=100,a=T", b";payload\x1b\\after", "cut APC");
}

/// A cut longer than the retained cap is not an error and not an empty
/// payload: it is "not encodable yet", and the caller retries after the
/// next chunk.
#[test]
fn a_continuation_past_the_retained_cap_defers_the_encode() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: SCROLLBACK,
        continuation_max_bytes: 4,
    })
    .expect("terminal");
    let mut render = RenderState::new().expect("render state");
    terminal.vt_write(b"content\r\n");
    terminal.vt_write(b"\x1b]0;a title far longer than four bytes");

    assert_eq!(terminal.continuation().expect("continuation"), None);
    assert_eq!(
        vt_snapshot(&terminal, &mut render).expect("vt_snapshot"),
        None,
        "an unavailable continuation defers the encode, it does not fail it"
    );

    // Reaching ground again makes it encodable.
    terminal.vt_write(b"\x1b\\");
    assert!(vt_snapshot(&terminal, &mut render)
        .expect("vt_snapshot")
        .is_some());
}
