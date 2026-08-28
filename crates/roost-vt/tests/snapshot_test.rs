//! Pin the encode half of the snapshot wrapper (`Terminal::snapshot`)
//! and the continuation-tracking construction option it depends on.
//!
//! Two things here are quiet-but-fatal for host-sessions' attach path.
//! First, the envelope: a snapshot that does not start with the
//! `"GHOSTSNP"` magic and a u16 version is not a snapshot, and nothing
//! downstream would tell us so until a decoder existed. Second, the
//! encode precondition — libghostty refuses to encode a terminal whose
//! VT parser is mid-sequence *unless* continuation tracking was enabled
//! before the input that left it unfinished. Since the attach payload is
//! taken from a live terminal that may be interrupted anywhere, the
//! tracking knob has to be a construction-time option and it has to read
//! back.
//!
//! There is no decoder wrapper yet, so every assertion here is on the
//! encode side only.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{Error, Terminal, TerminalOptions};

/// `snapshot.h:53-118`: 8-byte magic then a u16 LE version.
const MAGIC: &[u8; 8] = b"GHOSTSNP";
const ENVELOPE_LEN: usize = 10;

fn term(options: TerminalOptions) -> Terminal {
    Terminal::new(options).expect("Terminal::new")
}

fn assert_envelope(bytes: &[u8]) {
    assert!(
        bytes.len() > ENVELOPE_LEN,
        "snapshot must carry records past the {ENVELOPE_LEN}-byte envelope, got {} bytes",
        bytes.len()
    );
    assert_eq!(&bytes[..8], MAGIC, "snapshot magic");
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    assert_eq!(version, 1, "snapshot format version at this pin");
}

#[test]
fn encodes_an_envelope_and_records() {
    let bytes = term(TerminalOptions::default())
        .snapshot()
        .expect("snapshot of a ground-state terminal");
    assert_envelope(&bytes);
}

#[test]
fn encodes_a_terminal_with_styled_content() {
    let options = TerminalOptions {
        cols: 40,
        rows: 8,
        max_scrollback: 200,
        ..Default::default()
    };
    let mut terminal = term(options);
    // Bold + red "hello", reset, a second line, and a wide codepoint —
    // enough that the active screen is not the empty default.
    terminal.vt_write(b"\x1b[1;31mhello\x1b[0m\r\nworld \xe4\xb8\xad\r\n");

    let bytes = terminal.snapshot().expect("snapshot with content");
    assert_envelope(&bytes);

    let empty = term(options)
        .snapshot()
        .expect("snapshot of an empty terminal");
    assert!(
        bytes.len() > empty.len(),
        "content must widen the stream: {} vs empty {}",
        bytes.len(),
        empty.len()
    );
}

/// Plan test 4b. `\x1b[3` is a CSI with no final byte, so the parser is
/// left mid-sequence.
const UNFINISHED_CSI: &[u8] = b"\x1b[3";

#[test]
fn unfinished_parser_without_tracking_is_rejected() {
    let mut terminal = term(TerminalOptions::default());
    assert_eq!(
        terminal.continuation_max_bytes().expect("read back"),
        0,
        "tracking must default off"
    );
    terminal.vt_write(UNFINISHED_CSI);

    match terminal.snapshot() {
        Err(Error::InvalidValue) => {}
        other => panic!("expected InvalidValue for an untracked unfinished parser, got {other:?}"),
    }
}

#[test]
fn unfinished_parser_with_tracking_encodes() {
    let mut terminal = term(TerminalOptions {
        continuation_max_bytes: 4096,
        ..Default::default()
    });
    terminal.vt_write(UNFINISHED_CSI);

    let bytes = terminal
        .snapshot()
        .expect("tracking enabled before the unfinished input makes encode legal");
    assert_envelope(&bytes);
}

/// D2 read-back. The two iced production constructors pass
/// `max_scrollback: 2000` with tracking explicitly off; this pins that
/// shape reading back 0. (0 is also libghostty's own default, so the
/// constructor *applying* the option is pinned by the nonzero
/// round-trip below, not here.)
#[test]
fn iced_production_shapes_read_back_tracking_off() {
    for (cols, rows) in [(80_u16, 24_u16), (120, 40)] {
        let terminal = term(TerminalOptions {
            cols,
            rows,
            max_scrollback: 2000,
            continuation_max_bytes: 0,
        });
        assert_eq!(
            terminal.continuation_max_bytes().expect("read back"),
            0,
            "{cols}x{rows} production shape must have tracking disabled"
        );
    }
}

#[test]
fn continuation_limit_round_trips() {
    let terminal = term(TerminalOptions {
        continuation_max_bytes: 4096,
        ..Default::default()
    });
    assert_eq!(terminal.continuation_max_bytes().expect("read back"), 4096);
}

#[test]
fn set_continuation_max_bytes_disables_tracking() {
    let mut terminal = term(TerminalOptions {
        continuation_max_bytes: 4096,
        ..Default::default()
    });
    terminal.vt_write(UNFINISHED_CSI);
    terminal
        .snapshot()
        .expect("tracked unfinished parser encodes before the disable");
    terminal
        .set_continuation_max_bytes(0)
        .expect("set_continuation_max_bytes(0)");
    assert_eq!(
        terminal.continuation_max_bytes().expect("read back"),
        0,
        "zeroing the limit must disable tracking — the header's cleanup rule"
    );
    // Disabling while unfinished discards the retained continuation
    // (terminal.h), so encode must now refuse — the functional proof
    // that the disable did more than flip the reported limit.
    match terminal.snapshot() {
        Err(Error::InvalidValue) => {}
        other => {
            panic!("expected InvalidValue after disabling tracking mid-sequence, got {other:?}")
        }
    }
}
