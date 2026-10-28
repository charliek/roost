//! `Terminal::memory_usage` over `GHOSTTY_TERMINAL_DATA_MEMORY_USAGE`.
#![cfg(feature = "ffi")]

use roost_vt::{Terminal, TerminalOptions};

fn new_terminal() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 2000,
        ..Default::default()
    })
    .expect("terminal")
}

/// A fresh screen already owns a page, so this measures growth.
#[test]
fn scrollback_output_grows_the_primary_screen() {
    let mut term = new_terminal();
    let before = term.memory_usage().expect("memory usage");
    for line in 0..1000 {
        term.vt_write(format!("line {line} of scrollback output\r\n").as_bytes());
    }

    let after = term.memory_usage().expect("memory usage");
    assert!(
        after.primary.pages > before.primary.pages,
        "{before:?} -> {after:?}"
    );
    assert!(
        after.primary.resident_bytes > before.primary.resident_bytes,
        "{before:?} -> {after:?}"
    );
    assert!(
        after.primary.virtual_bytes >= after.primary.resident_bytes,
        "{after:?}"
    );
    assert_eq!(
        after.alternate.pages, 0,
        "no alternate screen yet: {after:?}"
    );
    assert_eq!(after.alternate.resident_bytes, 0, "{after:?}");
}

#[test]
fn the_alternate_screen_counts_once_used() {
    let mut term = new_terminal();
    term.vt_write(b"\x1b[?1049hfull screen\r\n");

    let usage = term.memory_usage().expect("memory usage");
    assert!(usage.alternate.pages > 0, "{usage:?}");
    assert!(usage.alternate.resident_bytes > 0, "{usage:?}");
}
