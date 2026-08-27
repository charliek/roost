//! What a theme apply does to an OSC color override — the assumption
//! `roost-engine`'s drain-side `OscColorState` mirrors.
//!
//! iced answers OSC color queries from its own state (plan 026 D10),
//! because the drain runs ahead of `vt_write`. That state is only
//! coherent with the terminal if it moves the same way the terminal
//! does, and the one non-obvious case is a theme change landing on top
//! of a program's OSC override. libghostty's model (pinned source,
//! `terminal/color.zig`): `DynamicRGB { override, default }` with
//! `get() = override orelse default`; the `OPT_COLOR_*` option a theme
//! push writes only moves `default`, and `DynamicPalette.changeDefault`
//! preserves every entry whose mask bit an `OSC 4` set raised. So OSC
//! overrides SURVIVE a theme change, per channel and per palette entry.
//!
//! These tests pin that through the FFI rather than trusting the source
//! read, since a SHA bump could change it under us.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{ColorRgb, Terminal, TerminalOptions};

fn term() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("Terminal::new")
}

fn apply_theme(term: &mut Terminal, fg: ColorRgb, bg: ColorRgb, cursor: ColorRgb) {
    // Exactly what `TerminalTab::apply_theme_candidate` pushes.
    term.set_color_foreground(fg).expect("set fg");
    term.set_color_background(bg).expect("set bg");
    term.set_color_cursor(cursor).expect("set cursor");
}

#[test]
fn an_osc_color_override_survives_a_theme_apply() {
    let mut term = term();
    let theme_a = ColorRgb::new(0x1e, 0x1e, 0x1e);
    apply_theme(&mut term, ColorRgb::new(0xff, 0xff, 0xff), theme_a, theme_a);
    assert_eq!(term.live_colors().expect("live").background, theme_a);

    term.vt_write(b"\x1b]11;rgb:00/11/22\x07");
    let override_bg = ColorRgb::new(0x00, 0x11, 0x22);
    assert_eq!(term.live_colors().expect("live").background, override_bg);

    // The theme change a palette pick drives.
    let theme_b = ColorRgb::new(0x21, 0x21, 0x21);
    apply_theme(&mut term, ColorRgb::new(0xee, 0xee, 0xee), theme_b, theme_b);
    assert_eq!(
        term.live_colors().expect("live").background,
        override_bg,
        "an OSC 11 override must outlive a theme push (it writes `default`, not `override`)"
    );
    assert_eq!(
        term.live_colors().expect("live").foreground,
        ColorRgb::new(0xee, 0xee, 0xee),
        "a channel with no override still follows the theme"
    );
}

#[test]
fn an_osc_color_reset_returns_the_channel_to_the_theme() {
    let mut term = term();
    let theme_a = ColorRgb::new(0x1e, 0x1e, 0x1e);
    apply_theme(&mut term, ColorRgb::new(0xff, 0xff, 0xff), theme_a, theme_a);
    term.vt_write(b"\x1b]11;rgb:00/11/22\x07");
    term.vt_write(b"\x1b]111\x07");
    assert_eq!(
        term.live_colors().expect("live").background,
        theme_a,
        "OSC 111 drops the override back to the theme's value"
    );
}

#[test]
fn an_osc_palette_override_survives_a_theme_apply_per_entry() {
    let mut term = term();
    let mut theme_a = [ColorRgb::new(0, 0, 0); 256];
    theme_a[5] = ColorRgb::new(0x1c, 0x1c, 0x1c);
    theme_a[6] = ColorRgb::new(0x2c, 0x2c, 0x2c);
    term.set_color_palette(&theme_a).expect("palette a");

    term.vt_write(b"\x1b]4;5;rgb:de/ad/be\x07");
    let override_5 = ColorRgb::new(0xde, 0xad, 0xbe);
    assert_eq!(term.live_palette().expect("live")[5], override_5);

    let mut theme_b = [ColorRgb::new(0, 0, 0); 256];
    theme_b[5] = ColorRgb::new(0x3c, 0x3c, 0x3c);
    theme_b[6] = ColorRgb::new(0x4c, 0x4c, 0x4c);
    term.set_color_palette(&theme_b).expect("palette b");

    let live = term.live_palette().expect("live");
    assert_eq!(
        live[5], override_5,
        "an OSC 4 set entry keeps its override across a theme push"
    );
    assert_eq!(
        live[6], theme_b[6],
        "an entry with no override follows the new theme"
    );
}

#[test]
fn an_osc_104_reset_clears_the_entry_override() {
    let mut term = term();
    let mut theme_a = [ColorRgb::new(0, 0, 0); 256];
    theme_a[5] = ColorRgb::new(0x1c, 0x1c, 0x1c);
    term.set_color_palette(&theme_a).expect("palette a");
    term.vt_write(b"\x1b]4;5;rgb:de/ad/be\x07");
    term.vt_write(b"\x1b]104;5\x07");
    assert_eq!(term.live_palette().expect("live")[5], theme_a[5]);

    // The flag is genuinely cleared, not just the value restored: a
    // later theme moves the entry again.
    let mut theme_b = [ColorRgb::new(0, 0, 0); 256];
    theme_b[5] = ColorRgb::new(0x3c, 0x3c, 0x3c);
    term.set_color_palette(&theme_b).expect("palette b");
    assert_eq!(
        term.live_palette().expect("live")[5],
        theme_b[5],
        "OSC 104 unsets the entry's mask bit, so the next theme owns it again"
    );
}

/// An OSC 110/111/112 reset clears the channel's override outright, so
/// the channel goes back to following later theme pushes — the same
/// semantics `OSC 104` already had for a palette entry, and the same
/// clearing form `OscColorState` implements.
///
/// This used to be the one place libghostty and `OscColorState`
/// disagreed: at the previous pin `DynamicRGB.reset()` assigned
/// `override = default` rather than clearing `override`, which froze a
/// reset channel at whatever default it happened to see. That is fixed
/// upstream as of the tip this build pins, so the test now pins the
/// agreement instead of the divergence. It needs the exact sequence
/// reset → theme change → color query to be observable at all.
#[test]
fn a_reset_channel_follows_later_theme_changes() {
    let mut term = term();
    let theme_a = ColorRgb::new(0x1e, 0x1e, 0x1e);
    apply_theme(&mut term, ColorRgb::new(0xff, 0xff, 0xff), theme_a, theme_a);
    term.vt_write(b"\x1b]11;rgb:00/11/22\x07");
    term.vt_write(b"\x1b]111\x07");
    assert_eq!(
        term.live_colors().expect("live").background,
        theme_a,
        "the reset drops the override and falls back to the current default"
    );

    let theme_b = ColorRgb::new(0x21, 0x21, 0x21);
    apply_theme(&mut term, ColorRgb::new(0xee, 0xee, 0xee), theme_b, theme_b);
    assert_eq!(
        term.live_colors().expect("live").background,
        theme_b,
        "a reset channel has no override left, so the next theme owns it again"
    );
}
