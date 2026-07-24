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
//! Gated on the `ffi` feature like the other wrapper tests; run with:
//!
//!     cargo test -p roost-vt --features ffi
#![cfg(feature = "ffi")]

use std::sync::{Arc, Mutex};

use roost_vt::{Terminal, TerminalOptions};

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
