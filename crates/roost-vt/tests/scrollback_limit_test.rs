//! Pin `TerminalOptions::max_scrollback` to libghostty's **line** limit.
//!
//! At the previous pin the limit rode in on `GhosttyTerminalOptions`; it now
//! moves to `ghostty_terminal_set(OPT_SCROLLBACK_MAX_LINES)` after
//! construction. Two things can silently go wrong there and both are
//! catastrophic-but-quiet: wiring the value to `OPT_SCROLLBACK_MAX_BYTES`
//! instead (turning "2000 rows" into ~2 KB of history), or skipping the
//! `set` for `0` and leaving libghostty's default limit in place (tests and
//! callers that ask for "no scrollback" would quietly get some). Read the
//! configured limit back through `GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_LINES`
//! so either mistake fails here.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{ffi, Terminal, TerminalOptions};

/// Read back a configured scrollback limit. `None` mirrors
/// `GHOSTTY_NO_VALUE`, which the header documents as "the limit is
/// unlimited".
fn configured_limit(terminal: &Terminal, key: ffi::GhosttyTerminalData) -> Option<usize> {
    let mut out: usize = usize::MAX;
    // SAFETY: the handle is live for the borrow, and `out` is a real
    // local of the `size_t` type both scrollback data keys document.
    let rc = unsafe {
        ffi::ghostty_terminal_get(terminal.as_ffi(), key, (&mut out) as *mut usize as *mut _)
    };
    match rc {
        ffi::GhosttyResult_GHOSTTY_SUCCESS => Some(out),
        ffi::GhosttyResult_GHOSTTY_NO_VALUE => None,
        other => panic!("unexpected rc {other} reading scrollback limit"),
    }
}

fn configured_max_lines(terminal: &Terminal) -> Option<usize> {
    configured_limit(
        terminal,
        ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_LINES,
    )
}

/// `ghostty_terminal_new` installs a default byte limit (10 KB at this
/// pin) that outranks the line limit whenever it is hit first — always,
/// at these magnitudes. `Terminal::new` must clear it or "2000 lines"
/// quietly delivers a few hundred.
fn configured_max_bytes(terminal: &Terminal) -> Option<usize> {
    configured_limit(
        terminal,
        ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_BYTES,
    )
}

fn term(max_scrollback: usize) -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback,
        ..Default::default()
    })
    .expect("Terminal::new")
}

#[test]
fn zero_scrollback_round_trips() {
    assert_eq!(
        configured_max_lines(&term(0)),
        Some(0),
        "max_scrollback: 0 must be applied, not left at libghostty's default"
    );
}

#[test]
fn production_scrollback_round_trips_as_lines() {
    // 2000 is what both UIs pass. A byte limit of 2000 would report
    // back through the *bytes* key, never this one.
    assert_eq!(configured_max_lines(&term(2000)), Some(2000));
}

#[test]
fn constructor_clears_the_default_byte_limit() {
    assert_eq!(
        configured_max_bytes(&term(2000)),
        None,
        "the default byte limit must be cleared or it outranks the line limit"
    );
    assert_eq!(configured_max_bytes(&term(0)), None);
}
