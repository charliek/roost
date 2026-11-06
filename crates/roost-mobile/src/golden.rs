//! The C6 golden fixtures under `mobile/test/fixtures/`: a frame from
//! known VT bytes in a 20x5 terminal, and a JSON file of the values its
//! decode must yield. The Dart decoder test asserts the same values from
//! the same bytes, so the two decoders cannot drift apart.
//!
//! Regenerate after a deliberate encoder change:
//! `ROOST_WRITE_FIXTURES=1 cargo test -p roost-mobile golden -- --ignored`.

use std::path::PathBuf;

use roost_client::attach::{AttachEvent, PayloadKind};
use roost_ipc::messages::AttachMode;
use roost_ui_model::theme::Theme;
use roost_vt::ColorRgb;
use serde_json::{json, Value};

use crate::frame::{attrs, decode, flags, row_text, Frame};
use crate::terminal::TabTerminal;

struct Fixture {
    name: &'static str,
    description: &'static str,
    input: &'static str,
}

const BASIC: Fixture = Fixture {
    name: "frame_v1_basic",
    description: "Plain text on three rows, the cursor after a prompt; a FULL first frame.",
    input: "hello, roost\r\nsecond line\r\n$ ",
};

const STYLES: Fixture = Fixture {
    name: "frame_v1_styles",
    description: "Bold, italic, underline; 256-color and truecolor fg/bg; inverse; two wide \
                  CJK characters; strikethrough and faint; blank cells with an SGR background; \
                  bracketed paste and mouse tracking on; a bar cursor at row 4, col 3.",
    input: "\x1b[1mbold\x1b[0m \x1b[3mital\x1b[0m \x1b[4mund\x1b[0m\r\n\
            \x1b[38;5;196mred\x1b[0m \x1b[38;2;10;20;30;48;2;200;210;220mtc\x1b[0m \x1b[7minv\x1b[0m\r\n\
            中文 \x1b[9mstrike\x1b[0m \x1b[2mfaint\x1b[0m\r\n\
            \x1b[41m  \x1b[0m end\
            \x1b[?2004h\x1b[?1000h\x1b[5 q\x1b[5;4H",
};

const COLS: u16 = 20;
const ROWS: u16 = 5;

/// Pinned here rather than taken from a bundled theme, so a theme edit
/// does not move the fixtures.
fn fixture_theme() -> Theme {
    let mut theme = Theme::roost_dark_fallback();
    theme.foreground = ColorRgb::new(0xd0, 0xd0, 0xd0);
    theme.background = ColorRgb::new(0x10, 0x10, 0x10);
    theme.cursor = ColorRgb::new(0x80, 0xc0, 0xff);
    theme.palette[1] = ColorRgb::new(0xcc, 0x37, 0x2e);
    theme
}

/// The first frame of a 20x5 tab that took `input` live.
fn render(input: &str) -> Vec<u8> {
    let mut tab = TabTerminal::new(fixture_theme()).expect("tab terminal");
    tab.apply(AttachEvent::Accepted {
        kind: PayloadKind::Vt,
        mode: AttachMode::Snapshot,
        cols: COLS,
        rows: ROWS,
        cell_w_px: 8,
        cell_h_px: 16,
        owner: None,
    });
    tab.apply(AttachEvent::Snap(Vec::new()));
    tab.apply(AttachEvent::Pty(input.as_bytes().to_vec()));
    tab.take_frame(false)
        .expect("take_frame")
        .expect("a first frame")
}

fn names(bits: u8, table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

const FLAG_NAMES: &[(u8, &str)] = &[
    (flags::FULL, "FULL"),
    (flags::CURSOR_VISIBLE, "CURSOR_VISIBLE"),
    (flags::MOUSE_TRACKING, "MOUSE_TRACKING"),
    (flags::ALT_SCREEN, "ALT_SCREEN"),
    (flags::BRACKETED_PASTE, "BRACKETED_PASTE"),
];

const ATTR_NAMES: &[(u8, &str)] = &[
    (attrs::BOLD, "BOLD"),
    (attrs::ITALIC, "ITALIC"),
    (attrs::UNDERLINE, "UNDERLINE"),
    (attrs::STRIKE, "STRIKE"),
    (attrs::FAINT, "FAINT"),
    (attrs::EXPLICIT_BG, "EXPLICIT_BG"),
    (attrs::WIDE, "WIDE"),
];

/// What the Dart test asserts: every decoded field, the bit names for
/// readers, and each row's text as a painter lays it out.
fn describe(fixture: &Fixture, frame: &Frame) -> Value {
    let h = &frame.header;
    let mut row_texts = vec![String::new(); usize::from(h.rows)];
    for row in &frame.rows {
        if let Some(text) = row_texts.get_mut(usize::from(row.index)) {
            *text = row_text(row, h.cols);
        }
    }
    json!({
        "description": fixture.description,
        "input": fixture.input,
        "version": crate::frame::VERSION,
        "flags": h.flags,
        "flag_names": names(h.flags, FLAG_NAMES),
        "cols": h.cols,
        "rows": h.rows,
        "default_fg": h.default_fg,
        "default_bg": h.default_bg,
        "cursor_color": h.cursor_color,
        "cursor_col": h.cursor_col,
        "cursor_row": h.cursor_row,
        "cursor_style": h.cursor_style,
        "scroll_total": h.scroll_total,
        "scroll_offset": h.scroll_offset,
        "scroll_len": h.scroll_len,
        "row_data": frame.rows.iter().map(|row| json!({
            "row_index": row.index,
            "runs": row.runs.iter().map(|run| json!({
                "col": run.col,
                "cells": run.cells,
                "fg": run.fg,
                "bg": run.bg,
                "attrs": run.attrs,
                "attr_names": names(run.attrs, ATTR_NAMES),
                "text": run.text,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "row_text": row_texts,
    })
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mobile/test/fixtures")
}

fn json_text(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("json");
    text.push('\n');
    text
}

#[test]
#[ignore = "writes mobile/test/fixtures; run with ROOST_WRITE_FIXTURES=1"]
fn write_golden_fixtures() {
    assert_eq!(
        std::env::var("ROOST_WRITE_FIXTURES").as_deref(),
        Ok("1"),
        "set ROOST_WRITE_FIXTURES=1 to overwrite the committed fixtures"
    );
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("fixtures dir");
    for fixture in [BASIC, STYLES] {
        let bytes = render(fixture.input);
        let frame = decode(&bytes).expect("decodes");
        std::fs::write(dir.join(format!("{}.bin", fixture.name)), &bytes).expect("write bin");
        std::fs::write(
            dir.join(format!("{}.json", fixture.name)),
            json_text(&describe(&fixture, &frame)),
        )
        .expect("write json");
    }
}

#[test]
fn committed_golden_fixtures_match_the_encoder() {
    let dir = fixtures_dir();
    for fixture in [BASIC, STYLES] {
        let committed = std::fs::read(dir.join(format!("{}.bin", fixture.name)))
            .expect("committed .bin (regenerate with ROOST_WRITE_FIXTURES=1)");
        assert_eq!(
            render(fixture.input),
            committed,
            "{}: the encoder no longer produces the committed frame",
            fixture.name
        );
        let expected: Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{}.json", fixture.name))).expect("committed .json"),
        )
        .expect("json parses");
        let frame = decode(&committed).expect("decodes");
        assert_eq!(describe(&fixture, &frame), expected, "{}", fixture.name);
    }
}

#[test]
fn the_basic_fixture_is_plain_text_and_a_cursor() {
    let frame = decode(&render(BASIC.input)).expect("decodes");
    let h = &frame.header;
    assert_eq!(h.flags, flags::FULL | flags::CURSOR_VISIBLE);
    assert_eq!((h.cols, h.rows), (COLS, ROWS));
    assert_eq!(
        (h.default_fg, h.default_bg, h.cursor_color),
        (0xd0d0d0, 0x101010, 0x80c0ff)
    );
    assert_eq!((h.cursor_col, h.cursor_row, h.cursor_style), (2, 2, 0));
    assert_eq!(frame.rows.len(), 5, "FULL carries every row");
    let first = &frame.rows[0].runs;
    assert_eq!(first.len(), 1, "one run for a line of words");
    assert_eq!((first[0].col, first[0].cells), (0, 12));
    assert_eq!(first[0].text, "hello, roost");
    assert_eq!(
        (first[0].fg, first[0].bg, first[0].attrs),
        (0xd0d0d0, 0x101010, 0)
    );
    assert_eq!(
        frame.rows[2].runs[0].text, "$",
        "the prompt's trailing blank is a gap"
    );
    assert!(frame.rows[3].runs.is_empty() && frame.rows[4].runs.is_empty());
}

#[test]
fn the_styles_fixture_pins_each_attribute() {
    let frame = decode(&render(STYLES.input)).expect("decodes");
    let h = &frame.header;
    assert_eq!(
        h.flags,
        flags::FULL | flags::CURSOR_VISIBLE | flags::MOUSE_TRACKING | flags::BRACKETED_PASTE
    );
    assert_eq!((h.cursor_col, h.cursor_row), (3, 4));
    assert_eq!(h.cursor_style, crate::frame::cursor_style::BAR);

    let run = |row: usize, text: &str| {
        frame.rows[row]
            .runs
            .iter()
            .find(|run| run.text == text)
            .unwrap_or_else(|| panic!("row {row} has no run {text:?}: {:?}", frame.rows[row]))
            .clone()
    };
    let (fg, bg) = (0xd0d0d0, 0x101010);
    assert_eq!((run(0, "bold").attrs, run(0, "bold").col), (attrs::BOLD, 0));
    assert_eq!(run(0, "ital").attrs, attrs::ITALIC);
    assert_eq!(run(0, "und").attrs, attrs::UNDERLINE);

    let red = run(1, "red");
    assert_eq!(
        (red.fg, red.bg, red.attrs),
        (0xff0000, bg, 0),
        "palette 196"
    );
    let tc = run(1, "tc");
    assert_eq!(
        (tc.fg, tc.bg, tc.attrs),
        (0x0a141e, 0xc8d2dc, attrs::EXPLICIT_BG)
    );
    let inv = run(1, "inv");
    assert_eq!((inv.fg, inv.bg, inv.attrs), (bg, fg, attrs::EXPLICIT_BG));

    let zhong = run(2, "中");
    assert_eq!((zhong.col, zhong.cells, zhong.attrs), (0, 2, attrs::WIDE));
    let wen = run(2, "文");
    assert_eq!((wen.col, wen.cells, wen.attrs), (2, 2, attrs::WIDE));
    assert_eq!(run(2, "strike").attrs, attrs::STRIKE);
    assert_eq!(run(2, "faint").attrs, attrs::FAINT);

    let blanks = run(3, "  ");
    assert_eq!(
        (blanks.col, blanks.cells, blanks.bg, blanks.attrs),
        (0, 2, 0xcc372e, attrs::EXPLICIT_BG)
    );
    assert_eq!(run(3, "end").col, 3);
    assert_eq!(row_text(&frame.rows[2], COLS), "中文 strike faint");
}
