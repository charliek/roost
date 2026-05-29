//! OSC 8 hyperlink readback through `Terminal::hyperlink_at`.
//!
//! `Terminal::hyperlink_at` wraps the two-call buffer pattern around
//! libghostty's `ghostty_grid_ref_hyperlink_uri`. Cells covered by an
//! OSC 8 hyperlink span report the URI verbatim; cells outside the
//! span return `None`. The Mac UI mirrors this through a small Swift
//! helper that calls the same C symbol — keeping both ports honest
//! against a real terminal fixture, not a mock.
//!
//! Run with:
//!
//!     cargo test -p roost-vt --features ffi --test hyperlink_at_test
#![cfg(feature = "ffi")]

use roost_vt::{Terminal, TerminalOptions};

/// Feeding `\e]8;;https://example.com\e\\link\e]8;;\e\\` makes cells
/// 0..4 (the four "link" letters) report `Some("https://example.com")`;
/// every other cell on row 0 reports `None`.
#[test]
fn hyperlink_at_reads_osc8_span() {
    let mut term = Terminal::new(TerminalOptions {
        cols: 40,
        rows: 5,
        max_scrollback: 100,
    })
    .expect("Terminal::new");
    // OSC 8 wraps the span "link" with explicit URI.
    term.vt_write(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");

    for col in 0..4u16 {
        assert_eq!(
            term.hyperlink_at(col, 0).as_deref(),
            Some("https://example.com"),
            "cell ({col}, 0) inside the OSC 8 span should report the URI"
        );
    }
    // Cell 4 is immediately past the closing "link" — outside the span.
    assert_eq!(
        term.hyperlink_at(4, 0),
        None,
        "cell (4, 0) is past the OSC 8 span and should not carry a URI"
    );
    // A row that hasn't been written to has no hyperlinks at all.
    assert_eq!(
        term.hyperlink_at(0, 2),
        None,
        "row 2 was never written; should report no hyperlink"
    );
}

/// A row with no OSC 8 should return `None` for every cell. Plain
/// terminal text never accidentally surfaces a fake URI.
#[test]
fn hyperlink_at_returns_none_for_plain_text() {
    let mut term = Terminal::new(TerminalOptions {
        cols: 40,
        rows: 5,
        max_scrollback: 100,
    })
    .expect("Terminal::new");
    term.vt_write(b"https://example.com");
    for col in 0..19u16 {
        assert_eq!(
            term.hyperlink_at(col, 0),
            None,
            "regex-style URL text without OSC 8 should not register as a hyperlink at col {col}"
        );
    }
}

/// Out-of-range coordinates collapse to `None` instead of corrupting
/// memory at the FFI boundary.
#[test]
fn hyperlink_at_rejects_out_of_range_cells() {
    let mut term = Terminal::new(TerminalOptions {
        cols: 10,
        rows: 3,
        max_scrollback: 0,
    })
    .expect("Terminal::new");
    term.vt_write(b"\x1b]8;;https://x.test\x1b\\hi\x1b]8;;\x1b\\");
    assert_eq!(
        term.hyperlink_at(99, 0),
        None,
        "col past width must be None"
    );
    assert_eq!(
        term.hyperlink_at(0, 99),
        None,
        "row past height must be None"
    );
}
