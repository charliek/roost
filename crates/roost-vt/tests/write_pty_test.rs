//! Tests for the `write_pty` device-query reply buffer API.
//!
//! `set_write_pty_buffer` wires libghostty-vt's `write_pty` effects
//! callback to an owned `Arc<Mutex<Vec<u8>>>`; every reply the engine
//! emits for a device query (DA1/DA2, DSR, DECRQM, XTVERSION, the Kitty
//! keyboard query, mode-2048 in-band size reports) lands in that buffer.
//! These tests model the upstream C-API test
//! (`third_party/ghostty/src/src/terminal/c/terminal.zig`, "set write_pty
//! callback") plus roost's replacement/clear/drop semantics.
//!
//! Since the pin moved to ghostty `f2d5758f6` (upstream `14c829883`,
//! "terminal: report OSC color queries in lib-vt") the same callback
//! also carries the OSC 4 / 10 / 11 / 12 color-query replies Roost used
//! to synthesize itself. The `osc_*` cases below pin that: the exact
//! reply bytes, that they come from the colors Roost pushes through
//! `OPT_COLOR_*`, and that a SET is visible to a QUERY behind it in the
//! same write. Roost's own drain must stay silent on all of them —
//! `crates/roost-engine/tests/osc_drain_reply_test.rs`.
//!
//! Gated on the `ffi` feature like the other wrapper tests; run with:
//!
//!     cargo test -p roost-vt --features ffi
#![cfg(feature = "ffi")]

use std::sync::{Arc, Mutex};

use roost_vt::{ColorRgb, Terminal, TerminalOptions};

fn new_terminal() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("terminal")
}

/// Install a fresh reply buffer on `term`, returning the caller's Arc
/// clone (the drain side).
fn install_buffer(term: &mut Terminal) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    term.set_write_pty_buffer(buf.clone())
        .expect("install write_pty buffer");
    buf
}

/// Take + clear the buffer contents.
fn drain(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    let mut g = buf.lock().expect("lock");
    std::mem::take(&mut *g)
}

/// Feed a query and return exactly the bytes the engine emitted for it,
/// clearing any prior contents first so the result is just this query.
fn query(term: &mut Terminal, buf: &Arc<Mutex<Vec<u8>>>, bytes: &[u8]) -> Vec<u8> {
    let _ = drain(buf);
    term.vt_write(bytes);
    drain(buf)
}

#[test]
fn decrqm_reports_mode_state() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    // Mode 7 (wraparound) is set by default → reply state 1 (set).
    assert_eq!(query(&mut term, &buf, b"\x1b[?7$p"), b"\x1b[?7;1$y");

    // Reset mode 7, then re-query → state 2 (reset).
    term.vt_write(b"\x1b[?7l");
    assert_eq!(query(&mut term, &buf, b"\x1b[?7$p"), b"\x1b[?7;2$y");

    // Unknown mode → state 0 (not recognized).
    assert_eq!(query(&mut term, &buf, b"\x1b[?9999$p"), b"\x1b[?9999;0$y");
}

#[test]
fn da1_replies_with_device_attributes() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    // Engine-default attribute set at the pinned SHA (VT220 + ANSI
    // color). A change here means a deliberate Ghostty bump or #209
    // advertising-policy work.
    assert_eq!(query(&mut term, &buf, b"\x1b[c"), b"\x1b[?62;22c");
}

#[test]
fn dsr_status_and_cursor_position() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    // DSR 5n — operating status → "OK".
    assert_eq!(query(&mut term, &buf, b"\x1b[5n"), b"\x1b[0n");

    // DSR 6n — cursor position report. Fresh terminal → row 1, col 1.
    assert_eq!(query(&mut term, &buf, b"\x1b[6n"), b"\x1b[1;1R");
}

#[test]
fn xtversion_reports_libghostty() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    assert_eq!(
        query(&mut term, &buf, b"\x1b[>q"),
        b"\x1bP>|libghostty\x1b\\"
    );
}

#[test]
fn kitty_keyboard_query_reflects_pushed_flags() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    // No flags pushed → 0.
    assert_eq!(query(&mut term, &buf, b"\x1b[?u"), b"\x1b[?0u");

    // Push flags (bit 0 = disambiguate) → query reflects 1.
    term.vt_write(b"\x1b[>1u");
    assert_eq!(query(&mut term, &buf, b"\x1b[?u"), b"\x1b[?1u");

    // Pop → back to 0.
    term.vt_write(b"\x1b[<u");
    assert_eq!(query(&mut term, &buf, b"\x1b[?u"), b"\x1b[?0u");
}

#[test]
fn mode_2048_resize_emits_in_band_size_report() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    // Enable mode 2048 (in-band size reports). Clear anything the
    // enable itself emitted so the assertion covers only the resize.
    term.vt_write(b"\x1b[?2048h");
    let _ = drain(&buf);

    // Resize with NO intervening vt_write — the report must come from
    // inside ghostty_terminal_resize.
    term.resize(100, 30, 8, 16).expect("resize");
    let report = drain(&buf);

    // CSI 48 ; rows ; cols ; height_px ; width_px t
    assert_eq!(report, b"\x1b[48;30;100;480;800t");
}

/// Push the colors a tab launches with — exactly what
/// `TerminalTab::apply_theme_candidate` / `Theme.apply` do.
fn apply_theme(term: &mut Terminal) {
    term.set_color_foreground(ColorRgb::new(0xff, 0xff, 0xff))
        .expect("set fg");
    term.set_color_background(ColorRgb::new(0x1e, 0x1e, 0x1e))
        .expect("set bg");
    term.set_color_cursor(ColorRgb::new(0x98, 0x98, 0x9d))
        .expect("set cursor");
    let mut palette = [ColorRgb::new(0, 0, 0); 256];
    palette[5] = ColorRgb::new(0xde, 0xad, 0xbe);
    term.set_color_palette(&palette).expect("set palette");
}

#[test]
fn osc_color_queries_reply_from_the_pushed_theme() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);
    apply_theme(&mut term);

    assert_eq!(
        query(&mut term, &buf, b"\x1b]10;?\x07"),
        b"\x1b]10;rgb:ffff/ffff/ffff\x07"
    );
    assert_eq!(
        query(&mut term, &buf, b"\x1b]11;?\x07"),
        b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07"
    );
    assert_eq!(
        query(&mut term, &buf, b"\x1b]12;?\x07"),
        b"\x1b]12;rgb:9898/9898/9d9d\x07"
    );
    assert_eq!(
        query(&mut term, &buf, b"\x1b]4;5;?\x07"),
        b"\x1b]4;5;rgb:dede/adad/bebe\x07"
    );
}

/// The reply preserves the terminator the request used, so an
/// ST-terminated query gets an ST-terminated answer.
#[test]
fn an_osc_color_reply_preserves_the_request_terminator() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);
    apply_theme(&mut term);

    assert_eq!(
        query(&mut term, &buf, b"\x1b]11;?\x1b\\"),
        b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"
    );
}

/// Why the `OPT_COLOR_*` push is load-bearing: libghostty answers with
/// `override orelse default`, and a terminal with neither has nothing
/// to report, so the query goes unanswered.
#[test]
fn an_unseeded_color_query_gets_no_reply() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    assert!(query(&mut term, &buf, b"\x1b]11;?\x07").is_empty());
}

#[test]
fn an_osc_color_reply_follows_a_set_from_an_earlier_write() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);
    apply_theme(&mut term);

    term.vt_write(b"\x1b]11;rgb:00/11/22\x07");
    assert_eq!(
        query(&mut term, &buf, b"\x1b]11;?\x07"),
        b"\x1b]11;rgb:0000/1111/2222\x07"
    );
}

/// Sequential semantics: a SET and a QUERY in ONE write answer with
/// the just-set color, in one reply. Roost's drain used to answer such
/// a query from a chunk-start snapshot — the pre-chunk color — which
/// both contradicted xterm and put a second answer on the wire.
#[test]
fn a_same_write_set_and_query_reply_sequentially() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);
    apply_theme(&mut term);

    assert_eq!(
        query(&mut term, &buf, b"\x1b]11;rgb:00/11/22\x07\x1b]11;?\x07"),
        b"\x1b]11;rgb:0000/1111/2222\x07"
    );
    assert_eq!(
        query(&mut term, &buf, b"\x1b]4;5;rgb:11/22/33\x07\x1b]4;5;?\x07"),
        b"\x1b]4;5;rgb:1111/2222/3333\x07"
    );
}

/// The opencode/opentui gate: `OSC 4;0;?` with a 300 ms timeout is
/// what all of its color detection hangs on.
#[test]
fn the_osc4_probe_opencode_gates_on_is_answered() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);
    apply_theme(&mut term);

    assert_eq!(
        query(&mut term, &buf, b"\x1b]4;0;?\x07"),
        b"\x1b]4;0;rgb:0000/0000/0000\x07"
    );
}

#[test]
fn replacement_routes_to_new_buffer_and_drops_old_arc() {
    let mut term = new_terminal();

    let a = Arc::new(Mutex::new(Vec::new()));
    term.set_write_pty_buffer(a.clone()).expect("install A");
    // A receives the first reply.
    assert_eq!(query(&mut term, &a, b"\x1b[5n"), b"\x1b[0n");

    let b = Arc::new(Mutex::new(Vec::new()));
    term.set_write_pty_buffer(b.clone()).expect("install B");

    // After replacement the field no longer holds A → strong count is
    // back to just this test's `a`.
    assert_eq!(Arc::strong_count(&a), 1, "old Arc should have been dropped");

    let _ = drain(&a);
    let _ = drain(&b);
    term.vt_write(b"\x1b[5n");

    // The reply lands only in B.
    assert_eq!(drain(&b), b"\x1b[0n");
    assert!(drain(&a).is_empty(), "old buffer should receive nothing");
}

#[test]
fn clear_write_pty_stops_buffering() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);

    // Sanity: buffering works before clear.
    assert_eq!(query(&mut term, &buf, b"\x1b[5n"), b"\x1b[0n");

    term.clear_write_pty().expect("clear");

    // After clear, queries buffer nothing.
    term.vt_write(b"\x1b[5n");
    assert!(drain(&buf).is_empty(), "no bytes after clear");
}

#[test]
fn drop_with_buffer_installed_is_clean() {
    let mut term = new_terminal();
    let buf = install_buffer(&mut term);
    term.vt_write(b"\x1b[5n");
    assert_eq!(drain(&buf), b"\x1b[0n");

    // Dropping a terminal with a live buffer must not crash or leak
    // (callback cleared before free). The caller's Arc survives.
    drop(term);
    assert_eq!(Arc::strong_count(&buf), 1);
}

#[test]
fn no_callback_baseline_ignores_queries() {
    // A terminal with no write_pty buffer installed must silently drop
    // device queries — no crash, nothing observable.
    let mut term = new_terminal();
    term.vt_write(b"\x1b[c");
    term.vt_write(b"\x1b[6n");
    term.vt_write(b"\x1b[?u");
}
