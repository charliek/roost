//! Mouse-encoder regression tests for the scroll-wheel-as-button path.
//!
//! Gated on the `ffi` feature (the wrapper types need libghostty-vt
//! linked). Run with:
//!
//!     cargo test -p roost-vt --features ffi
//!
//! Context: mouse-tracking TUIs (opencode, htop, …) enable DECSET
//! 1000/1002/1003 + SGR 1006. Before this wrapper existed Roost dropped
//! the wheel entirely when tracking was on. Ghostty/cmux encode the
//! wheel as button-4 (up) / button-5 (down) reports; these tests pin the
//! SGR output so the cell mapping (pixels → cell) stays correct.
#![cfg(feature = "ffi")]

use roost_vt::{mouse_action, mouse_button, MouseEncoder, MouseEvent, Terminal, TerminalOptions};

/// Terminal with normal mouse tracking + SGR (1006) format enabled, and
/// an encoder synced + sized to a known geometry (cell 10×20).
fn sgr_terminal_and_encoder() -> (Terminal, MouseEncoder) {
    let mut term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("terminal");
    // DECSET 1000 (normal button tracking) + 1006 (SGR extended coords).
    term.vt_write(b"\x1b[?1000h");
    term.vt_write(b"\x1b[?1006h");

    let mut enc = MouseEncoder::new().expect("encoder");
    enc.sync_from_terminal(&term);
    enc.set_size(800, 600, 10, 20);
    (term, enc)
}

#[test]
fn wheel_up_encodes_sgr_button_64() {
    let (term, mut enc) = sgr_terminal_and_encoder();
    enc.sync_from_terminal(&term);

    let mut ev = MouseEvent::new().expect("event");
    ev.set_action(mouse_action::PRESS);
    ev.set_button(mouse_button::FOUR); // wheel up
    ev.set_mods(0);
    // (50, 40) px with cell 10×20 → col 5 (1-based 6), row 2 (1-based 3).
    ev.set_position(50.0, 40.0);

    let bytes = enc.encode(&ev).expect("encode");
    // SGR wheel-up: ESC [ < 64 ; 6 ; 3 M (64 = button 4 + wheel bit 0x40).
    assert_eq!(bytes, b"\x1b[<64;6;3M");
}

#[test]
fn wheel_down_encodes_sgr_button_65() {
    let (term, mut enc) = sgr_terminal_and_encoder();
    enc.sync_from_terminal(&term);

    let mut ev = MouseEvent::new().expect("event");
    ev.set_action(mouse_action::PRESS);
    ev.set_button(mouse_button::FIVE); // wheel down
    ev.set_mods(0);
    ev.set_position(50.0, 40.0);

    let bytes = enc.encode(&ev).expect("encode");
    // SGR wheel-down: ESC [ < 65 ; 6 ; 3 M.
    assert_eq!(bytes, b"\x1b[<65;6;3M");
}

#[test]
fn wheel_is_well_formed_sgr() {
    // Shape guard independent of the exact button/coords, so a benign
    // libghostty field reorder doesn't fail us but a dropped/garbled
    // encode does.
    let (term, mut enc) = sgr_terminal_and_encoder();
    enc.sync_from_terminal(&term);

    let mut ev = MouseEvent::new().expect("event");
    ev.set_action(mouse_action::PRESS);
    ev.set_button(mouse_button::FOUR);
    ev.set_mods(0);
    ev.set_position(0.0, 0.0);

    let bytes = enc.encode(&ev).expect("encode");
    assert!(
        !bytes.is_empty(),
        "wheel under tracking must not be dropped"
    );
    assert_eq!(
        &bytes[0..3],
        b"\x1b[<",
        "SGR mouse reports start with ESC [ <"
    );
    let last = *bytes.last().unwrap();
    assert!(last == b'M' || last == b'm', "SGR reports end with M/m");
}

/// With mouse tracking OFF, the encoder reports nothing for the wheel —
/// the UI then handles it as local scrollback / alt-screen arrows. This
/// is why `TerminalView` only routes the wheel through the encoder when
/// tracking is active.
#[test]
fn wheel_with_tracking_off_is_dropped() {
    let term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("terminal");
    let mut enc = MouseEncoder::new().expect("encoder");
    enc.sync_from_terminal(&term); // tracking NONE
    enc.set_size(800, 600, 10, 20);

    let mut ev = MouseEvent::new().expect("event");
    ev.set_action(mouse_action::PRESS);
    ev.set_button(mouse_button::FOUR);
    ev.set_mods(0);
    ev.set_position(50.0, 40.0);

    let bytes = enc.encode(&ev).expect("encode");
    assert!(
        bytes.is_empty(),
        "no mouse report when tracking is off; got {bytes:?}"
    );
}
