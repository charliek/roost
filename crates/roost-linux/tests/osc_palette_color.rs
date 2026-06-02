//! Regression test for the OSC 4 palette-query reply path (the
//! opencode/opentui fix). Mirrors `osc_dynamic_color.rs` for the
//! palette channel: a reply must reflect a mid-session
//! `OSC 4;Ps;rgb:…` set, not the static theme palette the terminal
//! launched with. opencode/opentui gate *all* terminal color detection
//! on a reply to `OSC 4;0;?`, so a regression here makes opencode (and
//! similar TUIs) fall back to an unreadable theme.

use roost_osc::{format_palette_query_response, OscEvent, OscScanner};
use roost_vt::{ColorRgb, Terminal, TerminalOptions};

fn term() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("Terminal::new")
}

/// Answer a `PaletteQuery` from the terminal's live palette — the exact
/// logic the UI drains run inline.
fn answer(term: &Terminal, indices: &[u8]) -> Vec<u8> {
    let live = term.live_palette().expect("live_palette");
    let mut reply = Vec::new();
    for &idx in indices {
        let c = live[idx as usize];
        reply.extend_from_slice(&format_palette_query_response(idx, (c.r, c.g, c.b)));
    }
    reply
}

#[test]
fn osc4_set_then_query_round_trips_live_palette() {
    let mut term = term();

    // Seed slot 2 with the theme push the terminal launches with.
    let mut start = [ColorRgb::new(0, 0, 0); 256];
    start[2] = ColorRgb::new(0x1c, 0x1c, 0x1c);
    term.set_color_palette(&start).expect("set_color_palette");

    // App changes slot 2 mid-session; libghostty tracks the override
    // while the scanner ignores the set body.
    term.vt_write(b"\x1b]4;2;rgb:de/ad/be\x07");

    // App queries slot 2 — the scanner surfaces it as PaletteQuery.
    let mut scanner = OscScanner::new();
    let events = scanner.feed(b"\x1b]4;2;?\x07");
    assert_eq!(events, vec![OscEvent::PaletteQuery(vec![2])]);

    // The reply must encode the *new* color, not the stale theme push.
    let reply = answer(&term, &[2]);
    let text = std::str::from_utf8(&reply).expect("utf8");
    assert!(
        text.contains("\x1b]4;2;rgb:dede/adad/bebe\x07"),
        "reply must encode the post-set color (got {text:?})"
    );
    assert!(
        !text.contains("1c1c/1c1c/1c1c"),
        "reply must NOT encode the stale theme color (got {text:?})"
    );
}

#[test]
fn osc4_gate_probe_round_trips() {
    // The exact probe opentui gates color detection on: `OSC 4;0;?`.
    let mut term = term();
    let mut start = [ColorRgb::new(0, 0, 0); 256];
    start[0] = ColorRgb::new(0x12, 0x34, 0x56);
    term.set_color_palette(&start).expect("set_color_palette");

    let mut scanner = OscScanner::new();
    let events = scanner.feed(b"\x1b]4;0;?\x07");
    assert_eq!(events, vec![OscEvent::PaletteQuery(vec![0])]);

    let reply = answer(&term, &[0]);
    assert_eq!(reply, b"\x1b]4;0;rgb:1212/3434/5656\x07");
}

#[test]
fn osc4_multi_index_query_answers_each() {
    let mut term = term();
    let mut start = [ColorRgb::new(0, 0, 0); 256];
    start[0] = ColorRgb::new(0x11, 0x11, 0x11);
    start[1] = ColorRgb::new(0x22, 0x22, 0x22);
    term.set_color_palette(&start).expect("set_color_palette");

    let mut scanner = OscScanner::new();
    let events = scanner.feed(b"\x1b]4;0;?;1;?\x07");
    assert_eq!(events, vec![OscEvent::PaletteQuery(vec![0, 1])]);

    let reply = answer(&term, &[0, 1]);
    let text = std::str::from_utf8(&reply).expect("utf8");
    assert!(text.contains("\x1b]4;0;rgb:1111/1111/1111\x07"));
    assert!(text.contains("\x1b]4;1;rgb:2222/2222/2222\x07"));
}
