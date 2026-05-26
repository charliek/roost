//! Key-encoder regression tests for the Kitty-keyboard-protocol path.
//!
//! The whole file is gated on the `ffi` feature — the wrapper types it
//! exercises (`Terminal`, `KeyEncoder`, `KeyEvent`) are only compiled in
//! when libghostty-vt is linked. Run with:
//!
//!     cargo test -p roost-vt --features ffi
//!
//! Regression context: Roost passed `unshifted_codepoint = 0` to the
//! encoder. Under the Kitty keyboard protocol the encoder builds CSI-u
//! entries for letter/digit keys only when the unshifted codepoint is
//! present (letters aren't in the functional entry table). With 0 the
//! encoder emitted NOTHING, so Ctrl+A / Ctrl+K (and every Ctrl+letter)
//! were silently dropped in Claude Code / opencode (which enable Kitty).
#![cfg(feature = "ffi")]

use roost_vt::{key_action, mods, KeyEncoder, KeyEvent, Terminal, TerminalOptions};

const KEY_A: u32 = roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_A;

/// Build a terminal, push the Kitty "disambiguate escape codes" flag
/// (`CSI > 1 u`), and hand back a terminal + encoder synced to it. The
/// encoder mirrors production by syncing options from the terminal.
fn kitty_terminal_and_encoder() -> (Terminal, KeyEncoder) {
    let mut term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("terminal");
    // CSI > 1 u — push Kitty flags (bit 0 = disambiguate). This is the
    // realistic path: production calls `sync_from_terminal` every encode
    // and picks up whatever flags the running app pushed.
    term.vt_write(b"\x1b[>1u");
    let mut enc = KeyEncoder::new().expect("encoder");
    enc.sync_from_terminal(&term);
    (term, enc)
}

#[test]
fn ctrl_a_under_kitty_emits_csi_u_with_unshifted_codepoint() {
    let (term, mut enc) = kitty_terminal_and_encoder();
    enc.sync_from_terminal(&term);

    let mut ev = KeyEvent::new().expect("event");
    ev.set_action(key_action::PRESS);
    ev.set_key(KEY_A);
    ev.set_mods(mods::CTRL);
    ev.set_composing(false);
    ev.set_unshifted_codepoint('a' as u32);

    let bytes = enc.encode(&ev).expect("encode");
    // Ctrl+A under Kitty disambiguate → CSI 97 ; 5 u (97 = 'a',
    // 5 = 1 + ctrl(4)). Exact bytes pinned in the test below; here we
    // assert the well-formed shape so a benign field-order change in
    // libghostty doesn't fail us, but an empty (dropped) encode does.
    assert!(!bytes.is_empty(), "Ctrl+A under Kitty must not be dropped");
    assert_eq!(&bytes[0..2], b"\x1b[", "should be a CSI sequence");
    assert_eq!(*bytes.last().unwrap(), b'u', "Kitty reports end with 'u'");
}

/// Pin the exact CSI-u bytes so a regression in the encoding (not just a
/// drop) is caught. Confirmed against the actual encoder output.
#[test]
fn ctrl_a_under_kitty_exact_bytes() {
    let (term, mut enc) = kitty_terminal_and_encoder();
    enc.sync_from_terminal(&term);

    let mut ev = KeyEvent::new().expect("event");
    ev.set_action(key_action::PRESS);
    ev.set_key(KEY_A);
    ev.set_mods(mods::CTRL);
    ev.set_composing(false);
    ev.set_unshifted_codepoint('a' as u32);

    let bytes = enc.encode(&ev).expect("encode");
    // ESC [ 9 7 ; 5 u
    assert_eq!(bytes, b"\x1b[97;5u");
}

/// The trap that caused the bug: with `unshifted_codepoint = 0` the
/// Kitty encoder has no entry to build for a letter key and emits
/// nothing. Documents *why* the fix sets the real codepoint.
#[test]
fn ctrl_a_under_kitty_with_zero_codepoint_is_dropped() {
    let (term, mut enc) = kitty_terminal_and_encoder();
    enc.sync_from_terminal(&term);

    let mut ev = KeyEvent::new().expect("event");
    ev.set_action(key_action::PRESS);
    ev.set_key(KEY_A);
    ev.set_mods(mods::CTRL);
    ev.set_composing(false);
    ev.set_unshifted_codepoint(0);

    let bytes = enc.encode(&ev).expect("encode");
    assert!(
        bytes.is_empty(),
        "with codepoint 0 the Kitty encoder drops the letter (the bug); got {bytes:?}"
    );
}

/// Legacy mode (no Kitty flags) derives the C0 byte from key + mods, so
/// Ctrl+A is 0x01 regardless of the unshifted codepoint. Proves the fix
/// leaves the bash/readline path untouched.
#[test]
fn ctrl_a_legacy_returns_soh() {
    let mut enc = KeyEncoder::new().expect("encoder");
    let term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("terminal");
    enc.sync_from_terminal(&term);

    let mut ev = KeyEvent::new().expect("event");
    ev.set_action(key_action::PRESS);
    ev.set_key(KEY_A);
    ev.set_mods(mods::CTRL);
    ev.set_composing(false);
    ev.set_unshifted_codepoint('a' as u32);

    let bytes = enc.encode(&ev).expect("encode");
    assert_eq!(bytes, vec![0x01], "Ctrl+A in legacy mode is SOH (0x01)");
}
