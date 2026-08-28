//! `Terminal::mode_get` FFI tests — the DEC-private mode read backing
//! the DEC 2031 proactive color-scheme notification (C3). Mode 2031
//! (`COLOR_SCHEME_REPORT`) gates whether roost emits `CSI ? 997 ; Ps n`
//! on a runtime theme switch, so the getter must track `CSI ? 2031 h/l`.
//!
//! For a DEC private mode `ghostty_mode_new(2031, false)` packs to the
//! raw number (bit 15 = ANSI flag = 0), so `mode_get(2031)` maps
//! straight through — same as the existing 2004/1003/1004 callers.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{Terminal, TerminalOptions};

fn term() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
        ..Default::default()
    })
    .expect("Terminal::new")
}

#[test]
fn mode_2031_tracks_set_and_reset() {
    let mut t = term();

    // Default: an app that hasn't opted in → false.
    assert!(!t.mode_get(2031), "mode 2031 defaults to reset");

    // `CSI ? 2031 h` enables color-scheme reporting.
    t.vt_write(b"\x1b[?2031h");
    assert!(t.mode_get(2031), "mode 2031 set after CSI ? 2031 h");

    // `CSI ? 2031 l` disables it again.
    t.vt_write(b"\x1b[?2031l");
    assert!(!t.mode_get(2031), "mode 2031 reset after CSI ? 2031 l");
}

#[test]
fn mode_2031_is_independent_of_other_modes() {
    let mut t = term();
    // Enabling an unrelated DEC private mode must not flip 2031.
    t.vt_write(b"\x1b[?2004h");
    assert!(!t.mode_get(2031), "2031 stays reset when only 2004 is set");
    assert!(t.mode_get(2004), "2004 set (sanity)");
}
