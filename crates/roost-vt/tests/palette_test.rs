//! Live-palette FFI tests — `Terminal::live_palette()` must reflect a
//! mid-session `OSC 4;Ps;rgb:…` set so the OSC 4 query reply answers
//! with the app's current palette, not the stale theme push. This is
//! the OSC-4 analogue of the OSC 10/11/12 `live_colors` path.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{ColorRgb, Terminal, TerminalOptions};

fn term() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("Terminal::new")
}

#[test]
fn live_palette_reflects_osc4_set() {
    let mut t = term();

    // Seed the whole palette so every slot has a known starting value.
    let start = [ColorRgb::new(0x10, 0x20, 0x30); 256];
    t.set_color_palette(&start).expect("set_color_palette");

    let initial = t.live_palette().expect("live_palette initial");
    assert_eq!(initial[5], ColorRgb::new(0x10, 0x20, 0x30));

    // App changes palette slot 5 mid-session via OSC 4 set. libghostty
    // tracks the override; the OSC scanner ignores the set body, so the
    // reply path must read this live value (not the seeded default).
    t.vt_write(b"\x1b]4;5;rgb:de/ad/be\x07");

    let updated = t.live_palette().expect("live_palette after set");
    assert_eq!(
        updated[5],
        ColorRgb::new(0xde, 0xad, 0xbe),
        "slot 5 must reflect the OSC 4 set"
    );
    assert_eq!(
        updated[6],
        ColorRgb::new(0x10, 0x20, 0x30),
        "untouched slot keeps the seeded value"
    );
}

#[test]
fn live_palette_returns_all_256_entries() {
    let mut t = term();
    let mut start = [ColorRgb::new(0, 0, 0); 256];
    start[255] = ColorRgb::new(0xab, 0xcd, 0xef);
    t.set_color_palette(&start).expect("set_color_palette");

    let live = t.live_palette().expect("live_palette");
    assert_eq!(live.len(), 256);
    assert_eq!(live[0], ColorRgb::new(0, 0, 0));
    assert_eq!(live[255], ColorRgb::new(0xab, 0xcd, 0xef));
}
