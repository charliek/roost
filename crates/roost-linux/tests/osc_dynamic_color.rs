//! Regression test for issue #145: OSC 10/11/12 query replies must
//! reflect a mid-session `OSC 10/11/12;rgb:…` set, not the static
//! theme value the terminal launched with. PR #144 wired up the
//! reply path but read from the theme; without this test, a refactor
//! could silently revert to that stale behavior.

use roost_osc::format_color_query_response;
use roost_vt::{ColorRgb, Terminal, TerminalOptions};

/// Push a complete theme (fg+bg+cursor) — libghostty's effective-color
/// getters return `NoValue` for any color that's never been set, so the
/// test mirrors the real boot path (every tab's session pushes fg+bg+
/// cursor at start) before exercising the OSC reply path.
fn set_starting_theme(term: &mut Terminal) {
    term.set_color_foreground(ColorRgb::new(0xff, 0xff, 0xff))
        .expect("set_color_foreground");
    term.set_color_background(ColorRgb::new(0x1c, 0x1c, 0x1c))
        .expect("set_color_background");
    term.set_color_cursor(ColorRgb::new(0x98, 0x98, 0x9d))
        .expect("set_color_cursor");
}

#[test]
fn osc_11_set_then_query_replies_with_new_bg() {
    let mut term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("Terminal::new");

    set_starting_theme(&mut term);

    let initial = term.live_colors().expect("live_colors initial");
    assert_eq!(
        initial.background,
        ColorRgb::new(0x1c, 0x1c, 0x1c),
        "theme bg should be the starting live bg"
    );

    // Feed an `OSC 11;rgb:00/11/22 BEL` mid-session set. libghostty
    // updates its internal default-bg from this; the scanner ignores
    // the set-color body, so the reply path is the only thing that
    // would have used the stale theme value pre-fix.
    term.vt_write(b"\x1b]11;rgb:00/11/22\x07");

    let updated = term.live_colors().expect("live_colors after set");
    assert_eq!(
        updated.background,
        ColorRgb::new(0x00, 0x11, 0x22),
        "live bg should reflect the OSC 11 set"
    );

    // Format the reply we'd send back to the app's OSC 11 query —
    // bytes must encode the *new* color, not the old theme bg.
    let reply = format_color_query_response(
        11,
        (
            updated.background.r,
            updated.background.g,
            updated.background.b,
        ),
    )
    .expect("format_color_query_response 11");
    let text = std::str::from_utf8(&reply).expect("reply is UTF-8");
    assert!(
        text.contains("0000/1111/2222"),
        "reply must encode the post-set bg (got {text:?})"
    );
    assert!(
        !text.contains("1c1c/1c1c/1c1c"),
        "reply must NOT encode the stale theme bg (got {text:?})"
    );
}

#[test]
fn osc_10_set_then_query_replies_with_new_fg() {
    let mut term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("Terminal::new");
    set_starting_theme(&mut term);

    term.vt_write(b"\x1b]10;rgb:aa/bb/cc\x07");

    let live = term.live_colors().expect("live_colors");
    assert_eq!(
        live.foreground,
        ColorRgb::new(0xaa, 0xbb, 0xcc),
        "live fg should reflect the OSC 10 set"
    );

    let reply = format_color_query_response(
        10,
        (live.foreground.r, live.foreground.g, live.foreground.b),
    )
    .expect("format_color_query_response 10");
    assert!(std::str::from_utf8(&reply)
        .unwrap()
        .contains("aaaa/bbbb/cccc"));
}
