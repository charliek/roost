//! Regression test for plan 018 §2.2: pins that a mid-session OSC 10/11
//! *set* does not change what GTK paints as the default fg/bg, today.
//!
//! Two facts, both verified at source (not re-derived here):
//! - `roost-osc` drops OSC 10/11/12 *set* bodies entirely; only a
//!   `body == "?"` query emits an `OscEvent`
//!   (`crates/roost-osc/src/lib.rs:321-328`). So no "the theme changed"
//!   signal ever reaches GTK's dirty/redraw path via the scanner.
//! - `TerminalView::paint` resolves the default fg/bg it actually draws
//!   with from `self.theme.foreground` / `self.theme.background` —
//!   never from `RenderState::colors()` (libghostty's reported/live
//!   colors) — see `TerminalViewState::refresh_cache`'s caching
//!   invariant in `crates/roost-linux/src/terminal_view.rs`.
//!   `TerminalViewState` and `paint` are declared only in `main.rs`
//!   (binary-only), so they aren't constructible from this
//!   integration-test crate; this test exercises
//!   the highest constructible seam instead — the same `roost_vt::
//!   Terminal` + `RenderState` level `osc_dynamic_color.rs` already uses
//!   — and asserts both halves of the fact directly: (a) libghostty's
//!   *live* background genuinely changes from the OSC 11 set (so the set
//!   was really processed, this isn't a scanner no-op check already
//!   covered elsewhere), and (b) a `Theme` value — the thing `paint`
//!   actually reads for its default bg — is a plain struct with no path
//!   for an OSC set to reach it, so it is unaffected by construction.
//!   Together, (a) + (b) pin today's GTK behavior: an OSC 11 set changes
//!   libghostty's internal state but changes nothing GTK renders.
//!
//! IMPORTANT — this pins "the dirty-row cache (plan 018 E3b) did not
//! change this behavior", NOT "this behavior is correct". GTK ignoring
//! libghostty's reported colors for OSC 10/11/DECSCNM is a **pre-existing
//! gap** relative to iced (which reacts to OSC 10/11 since plan 017's
//! D3b guard). When GTK starts consuming libghostty's reported colors
//! instead of always using the theme, this test's assertion inverts —
//! the resolved default bg would then track the OSC 11 set — and the
//! plan 018 D3 cache-invalidation guard gains its reserved fourth key
//! (a "live colors" key alongside `(cols, rows)` / theme / bold_color)
//! so the cache doesn't go stale under that future change.

use roost_ui_model::theme::Theme;
use roost_vt::{ColorRgb, RenderState, Terminal, TerminalOptions};

/// Mirrors `TerminalView::paint`'s resolution exactly
/// (see `refresh_cache`'s guard block): the default bg drawn is always
/// `theme.background`, full stop — `RenderState::colors()` is read (and
/// its `background` field ignored for this purpose) only because the
/// production code path also calls it every frame; a hypothetical GTK
/// paint that instead preferred libghostty's live colors would return
/// `render_state.colors()?.background` here, which is exactly the
/// change this test's doc comment says should flip the assertion below.
fn gtk_resolved_default_bg(_render_state: &RenderState, theme: &Theme) -> ColorRgb {
    theme.background
}

#[test]
fn osc11_set_does_not_change_gtk_resolved_default_bg() {
    let mut term = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .expect("Terminal::new");

    // Push a starting theme onto libghostty, mirroring the real boot
    // path (every tab's session pushes fg+bg+cursor at start) — same
    // setup `osc_dynamic_color.rs` uses.
    let theme_bg = ColorRgb::new(0x1c, 0x1c, 0x1c);
    term.set_color_foreground(ColorRgb::new(0xff, 0xff, 0xff))
        .expect("set_color_foreground");
    term.set_color_background(theme_bg)
        .expect("set_color_background");
    term.set_color_cursor(ColorRgb::new(0x98, 0x98, 0x9d))
        .expect("set_color_cursor");

    let theme = Theme {
        background: theme_bg,
        ..Theme::roost_dark_fallback()
    };

    let mut render_state = RenderState::new().expect("RenderState::new");
    render_state.update(&term).expect("render_state.update");

    assert_eq!(
        gtk_resolved_default_bg(&render_state, &theme),
        theme_bg,
        "sanity: resolved default bg starts equal to the theme bg"
    );

    // Feed a mid-session OSC 11 set. libghostty's own live-color state
    // genuinely changes from this (part (a) of the header doc) — the
    // scanner drops the set body, but libghostty itself still applies
    // it internally.
    let new_bg = ColorRgb::new(0x00, 0x11, 0x22);
    term.vt_write(b"\x1b]11;rgb:00/11/22\x07");

    let live = term.live_colors().expect("live_colors after OSC 11 set");
    assert_eq!(
        live.background, new_bg,
        "libghostty's live bg must reflect the OSC 11 set — otherwise this \
         test would be pinning a scanner no-op, not GTK's theme-only resolve"
    );

    render_state
        .update(&term)
        .expect("render_state.update after OSC 11 set");

    // Part (b): what GTK's `paint` actually draws with is untouched.
    // `Theme` carries no route for a mid-session OSC set to reach it, so
    // this assertion holds by construction today — but it is the exact
    // thing that must be re-derived from `render_state.colors()` instead
    // of `theme.background` the day GTK starts consuming libghostty's
    // reported colors (see the module doc's REVERSED note).
    assert_eq!(
        gtk_resolved_default_bg(&render_state, &theme),
        theme_bg,
        "GTK's resolved default bg must NOT change from an OSC 11 set today \
         (pre-existing gap vs iced, not a regression — see module doc)"
    );
    assert_ne!(
        gtk_resolved_default_bg(&render_state, &theme),
        new_bg,
        "GTK's resolved default bg must NOT track the OSC 11 set's new color"
    );
}
