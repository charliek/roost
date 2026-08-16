"""Tab-strip band + sidebar-divider pixel guards — iced only.

Three chrome invariants that no dump op can see, each asserted off
`app.screenshot`:

1. Issue #281 — with enough wide tabs the strip's horizontal scrollable
   overflows, and iced's stock scrollbar painted a 10px filled rail +
   scroller straight across the 24px pills, a gray band across the tab
   row. The fix is a zero-width scrollbar (any visible indicator
   overlays the pills), which `tab.dump`/`tab.list` cannot see.
2. Plan 016 W1.3 — the 1px sidebar/content divider, which lives INSIDE
   the sidebar's own width (so `terminal_grid` keeps every pixel
   `sidebar_width` leaves it) and is not drawn at all when the sidebar
   is collapsed, matching the Mac's NSSplitView divider.
3. Issue #311 — both notification dots (the tab pill's badge and the
   sidebar project row's) must paint `NOTIFICATION_BADGE` #007aff, the
   Mac's `controlAccentColor`, not the generic `NOTIFICATION` blue they
   used to. `tab.list` exposes only the `has_notification` bit, so the
   colour, the size and the badge's place inside its own pill are all
   invisible to the op set.

*Asserted* — with the strip overflowing, no row of the tab band contains
a long horizontal run of a single color other than the band's own chrome
fills (`BAND` band background, `ACTIVE_TAB` pill fill — our own
constants in `chrome.rs`). The stock rail (~870px), its scroller
(~350px), and even the interim 2px hover sliver all form such runs; tab
title glyphs, status dots, and antialiasing never do. Color-agnostic on
purpose: a scrollbar reintroduced in ANY theme color is caught, not just
the iced-Dark grays observed in #281. Then: the sidebar's rightmost
column reads `DIVIDER` from the band rows down while expanded, and the
window's leading column carries no divider once collapsed. Then: with
one notified INACTIVE tab, exactly one accent-coloured blob in the tab
band and one in the sidebar, each exactly #007aff at its centre, roughly
`NOTIFICATION_DOT_SIZE` across, band-centred, and bracketed between its
own tab's title and the next pill's fill — with a clean baseline before
the notification and a clean frame after `tab.clear_notification`.

*Not asserted* — text position or metrics (font-dependent), the clipped
right-most pill's exact edge, and hover states (headless capture has no
pointer inside the window; the zero-width scrollbar draws nothing in any
state anyway). The divider is sampled at a handful of rows, not scanned:
it is one container fill, so a full scan buys no signal.

Iced-only: the mac/gtk strips are native scroll views with overlay
scrollers and have never shown this artifact, mac's divider is
NSSplitView's own and gtk has none; their band colors differ.

`window.resize` is test-mode-gated, so the file skips without
`ROOST_TEST_MODE=1` — same convention as `test_sidebar_pixels.py`.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from client import Timeout
from test_sidebar_collapse_persistence import _toggle_to_collapsed, _toggle_to_visible
from test_sidebar_pixels import COLOR_TOL, _blobs, _capture, _pixel
from util import BARE_SHELL_ARGV

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "screenshot"))
import pngtool  # noqa: E402  — pure stdlib PNG decoder, imported not shelled out

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

WINDOW_W, WINDOW_H = 1100.0, 700.0

# Verbatim from `crates/roost-iced/src/chrome.rs`: `BAND_HEIGHT` bounds the
# scan; `BAND` (every chrome band — header, footer, tab strip) and
# `ACTIVE_TAB` are the only fills allowed to run wide inside the band, and
# `DIVIDER` is the sidebar's own trailing hairline.
BAND_HEIGHT = 32
BAND = (0x24, 0x29, 0x2C)
ACTIVE_TAB = (0x24, 0x37, 0x51)
DIVIDER = (0x1A, 0x1D, 0x1E)
ALLOWED_WIDE = {BAND, ACTIVE_TAB}

# The tab title color, verbatim from `chrome.rs` MUTED_TEXT — used only as
# the readiness/overflow probe (glyph pixels render exactly this color).
MUTED_TEXT = (0xA0, 0xA4, 0xB0)

# Verbatim from `chrome.rs` NOTIFICATION_BADGE — the one fill `chrome::badge`
# paints, on both notification dots (tab pill + sidebar project row). Pinned
# to the Mac's `controlAccentColor` (#311); nothing else in the chrome wears
# it, which is what makes a bare color search a valid locator here.
NOTIFICATION_BADGE = (0x00, 0x7A, 0xFF)
NOTIFICATION_DOT_SIZE = 9.0

# Antialiasing crumbs around the badge circle that 4-connectivity could
# strand as their own 1-2px component. None appear today (the tol-2 match is
# one solid component either way), so the filter costs no signal; it is here
# so a renderer that does fray the circle's edge cannot turn one badge into
# "several blobs" and fail the count assertion for the wrong reason.
MIN_BADGE_SIDE = 3
# The badge's bounding box. `NOTIFICATION_DOT_SIZE` is 8 and the tol-2 blob
# measures exactly 8x8 in both regions on macOS/wgpu — but the fill is a
# radius-4 circle with no flattening (unlike the sidebar lifecycle dot, which
# `test_sidebar_pixels.py` gives radius 3 precisely "to retain a solid
# renderer-neutral edge"), so the corners are antialiased and this module runs
# on four lanes: macOS, Linux/X11 under two renderers, and weston. The floor is
# the same 5px saturated core `MIN_DOT_SIDE` already pins for an 8px dot
# repo-wide, measured rather than guessed on the one renderer available here.
# Coarse by intent — it fails a dot at the wrong scale (the 3px project
# stripe, or a badge grown into its pill); the color and bracket assertions
# carry the real signal.
BADGE_SIDE_RANGE = (5, 10)
# An active pill is `PILL_HEIGHT` (24) tall and always wider than its status
# dot + title; nothing else in the band is filled ACTIVE_TAB.
MIN_PILL_SIDE = 15
# Empty chrome between the badge and the next pill's fill: the badge is the
# trailing child of its pill container (`padding([0, 2])`) and the pills sit
# in a `row![].spacing(6)`, so the geometric gap is 2 + 6 = 8px — i.e.
# `pill.minx - badge.maxx == 9`, which is exactly what macOS/wgpu measures.
# The badge is a circle, so its tol-matched right edge can land a pixel or
# two inside the geometric one (widening the gap) and a different subpixel
# snap can shave one off. Bounded on BOTH sides: the ceiling fails a badge
# parked out in the inter-pill region, and the floor fails one that has
# drifted right toward the next pill — an upper bound alone would accept any
# gap from 1px up, so the drift case would pass unnoticed.
BADGE_TO_PILL_GAP_RANGE = (6, 12)
# The sidebar dot's trailing inset, derived: the project pill is inset
# `PROJECT_PILL_INSET_X` (6) from the row's right edge and reserves
# `PROJECT_DOT_INSET` (8) inside that for the dot, so the dot's right edge
# lands 14px in from the sidebar's trailing edge. Asserting this — rather
# than only "somewhere in the sidebar half" — is what makes the second blob
# provably the project row's dot and not a similarly-coloured blob elsewhere
# in the sidebar.
SIDEBAR_DOT_TRAILING_INSET = 14

# Longest legitimate single-color run that is NOT a chrome fill: glyph
# strokes and dot rows are <20px. The #281 artifacts ran 350–870px, and a
# reintroduced 2px sliver spans the scroller length (hundreds of px), so
# 100 sits an order of magnitude clear of both sides.
MAX_FOREIGN_RUN = 100

# Five ~70char titles at ~6px/char ≈ 2100px of pills — more than double
# the widest strip a 1100px window can offer, so overflow is guaranteed.
TAB_COUNT = 5
LONG_TITLE = "/Users/example/projects/roost/very/long/path/segment-{n}-0123456789abcdef"


def _wide_foreign_runs(shot, x0: int, x1: int, y1: int) -> list[tuple[int, int, int, tuple[int, int, int]]]:
    """(y, run_start_x, run_len, color) for every >MAX_FOREIGN_RUN run of a
    single exact color not in ALLOWED_WIDE, within x0..x1 and rows 0..y1."""
    width, height, bpp, px = shot
    x1 = min(x1, width)
    y1 = min(y1, height)
    bad = []
    for y in range(y1):
        base = y * width * bpp
        run_color = None
        run_start = x0
        run_len = 0
        for x in range(x0, x1 + 1):
            color = None
            if x < x1:
                o = base + x * bpp
                color = (px[o], px[o + 1], px[o + 2])
            if color == run_color:
                run_len += 1
                continue
            if run_color is not None and run_len > MAX_FOREIGN_RUN and run_color not in ALLOWED_WIDE:
                bad.append((y, run_start, run_len, run_color))
            run_color, run_start, run_len = color, x, 1
    return bad


def _sample_rows(height: int) -> list[int]:
    """Rows spanning the band, the list region and the footer band — the
    three sidebar regions the one divider fill has to cover."""
    return [1, BAND_HEIGHT // 2, BAND_HEIGHT + 8, height // 2, height - 8]


def _column_samples(shot, x: int, ys: list[int]) -> list[tuple[int, tuple[int, int, int]]]:
    width, _height, bpp, px = shot
    out = []
    for y in ys:
        o = (y * width + x) * bpp
        out.append((y, (px[o], px[o + 1], px[o + 2])))
    return out


def _fmt(samples) -> str:
    return ", ".join(f"y={y}:#{c[0]:02x}{c[1]:02x}{c[2]:02x}" for y, c in samples)


def _rightmost_title_pixel(shot, x0: int, x1: int, y1: int) -> int:
    """Rightmost MUTED_TEXT pixel in columns x0..x1, rows 0..y1, found by
    scanning each row back-to-front and stopping at its first match."""
    width, height, bpp, px = shot
    rightmost = -1
    for y in range(min(y1, height)):
        base = y * width * bpp
        for x in range(min(x1, width) - 1, x0 - 1, -1):
            o = base + x * bpp
            if (px[o], px[o + 1], px[o + 2]) == MUTED_TEXT:
                rightmost = max(rightmost, x)
                break
    return rightmost


@pytest.mark.skipif(
    not TEST_MODE,
    reason="window.resize requires ROOST_TEST_MODE=1 in the UI's launch env",
)
def test_overflowing_tab_strip_paints_no_scrollbar_band(roost, project, target, tmp_path):
    if target != "iced":
        pytest.skip("#281 is an iced scrollable artifact; mac/gtk strips are native")

    tabs = [
        roost.open_tab(
            project,
            cwd="/tmp",
            title=LONG_TITLE.format(n=n),
            argv=BARE_SHELL_ARGV,
        )
        for n in range(TAB_COUNT)
    ]
    roost.focus(tabs[0])  # make `project` active so its tabs ARE the strip
    roost.window_resize(WINDOW_W, WINDOW_H)

    artifact_dir = os.environ.get("ROOST_E2E_ARTIFACT_DIR")
    shot_path = (
        Path(artifact_dir) / "tab_strip.png"
        if artifact_dir
        else tmp_path / "tab_strip.png"
    )
    shot_path.parent.mkdir(parents=True, exist_ok=True)
    state: dict = {"shot": None, "rightmost": -1}

    def _strip_full_of_titles() -> bool:
        sidebar_w = int(roost.window_metrics()["sidebar_width"])
        if sidebar_w <= 0:
            return False
        shot = _capture(roost, shot_path)
        if shot is None:
            return False
        width = shot[0]
        # Nothing is pinned at the band's right edge anymore (bell removed,
        # `+` moved in-strip — plan 016 C4), so the probe scans the full
        # band width; no carve-out needed.
        rightmost = _rightmost_title_pixel(shot, sidebar_w + 2, width, BAND_HEIGHT)
        state["shot"], state["sidebar_w"], state["rightmost"] = shot, sidebar_w, rightmost
        # A title glyph within 150px of the right edge proves the pills run
        # to the strip's clip edge, i.e. the scrollable is overflowing and
        # the guard below is exercising the case #281 regressed on.
        return rightmost >= width - 150

    try:
        roost._wait(
            _strip_full_of_titles,
            10.0,
            "overflowing tab titles painted across the strip",
        )
    except Timeout as exc:
        raise AssertionError(
            f"tab titles never filled the strip (rightmost title pixel "
            f"x={state['rightmost']}); the overflow premise did not hold. "
            f"Last screenshot: {shot_path}"
        ) from exc

    shot, sidebar_w = state["shot"], state["sidebar_w"]
    bad = _wide_foreign_runs(shot, sidebar_w + 2, shot[0], BAND_HEIGHT)
    assert not bad, (
        "scrollbar-like band painted across the tab strip (#281): "
        + "; ".join(
            f"y={y} x={x}..{x + n - 1} len={n} color=#{c[0]:02x}{c[1]:02x}{c[2]:02x}"
            for y, x, n, c in bad[:10]
        )
        + f" (screenshot: {shot_path})"
    )


@pytest.mark.skipif(
    not TEST_MODE,
    reason="window.resize requires ROOST_TEST_MODE=1 in the UI's launch env",
)
def test_sidebar_divider_hairline_only_while_expanded(roost, target, tmp_path):
    """The divider is one 1px column at the sidebar's trailing edge, drawn
    inside `sidebar_width` (plan 016 W1.3) so the terminal grid keeps every
    pixel the sidebar leaves it — a divider that grew its own layout column
    would still look right but would steal a cell. It is absent when the
    sidebar is collapsed, like the Mac's NSSplitView divider.

    Pixel oracle by design: like the rest of this module (#281/#291), the
    subject is renderer-only chrome with no textual IPC surface — the
    narrow, documented exception to roosttest's text-not-pixels rule."""
    if target != "iced":
        pytest.skip("iced chrome hairline; mac uses NSSplitView's divider and gtk has none")

    roost.window_resize(WINDOW_W, WINDOW_H)
    _toggle_to_visible(roost)

    artifact_dir = os.environ.get("ROOST_E2E_ARTIFACT_DIR")
    base = Path(artifact_dir) if artifact_dir else tmp_path
    base.mkdir(parents=True, exist_ok=True)
    expanded_path, collapsed_path = base / "divider.png", base / "divider_collapsed.png"
    state: dict = {"samples": [], "collapsed_samples": []}

    def _divider_painted() -> bool:
        metrics = roost.window_metrics()
        sidebar_w = int(metrics["sidebar_width"])
        if metrics["sidebar_collapsed"] or sidebar_w <= 0:
            return False
        shot = _capture(roost, expanded_path)
        if shot is None:
            return False
        state["x"] = sidebar_w - 1
        state["samples"] = _column_samples(shot, sidebar_w - 1, _sample_rows(shot[1]))
        return all(color == DIVIDER for _y, color in state["samples"])

    try:
        roost._wait(_divider_painted, 10.0, "sidebar divider hairline painted")
    except Timeout as exc:
        raise AssertionError(
            f"the sidebar's trailing column (x={state.get('x')}) must render "
            f"DIVIDER #{DIVIDER[0]:02x}{DIVIDER[1]:02x}{DIVIDER[2]:02x} from the "
            f"band rows down; got {_fmt(state['samples'])} "
            f"(screenshot: {expanded_path})"
        ) from exc

    try:
        _toggle_to_collapsed(roost)

        def _collapsed_repainted() -> bool:
            shot = _capture(roost, collapsed_path)
            if shot is None:
                return False
            state["collapsed_samples"] = _column_samples(shot, 0, _sample_rows(shot[1]))
            # The leading column is the tab band once the sidebar is gone, so
            # a band-colored top row proves the relayout landed and the
            # samples below it are worth asserting on.
            return all(c == BAND for y, c in state["collapsed_samples"] if y < BAND_HEIGHT)

        try:
            roost._wait(_collapsed_repainted, 10.0, "collapsed layout repainted to the window edge")
        except Timeout as exc:
            raise AssertionError(
                f"with the sidebar collapsed the tab band must reach x=0; got "
                f"{_fmt(state['collapsed_samples'])} (screenshot: {collapsed_path})"
            ) from exc

        assert all(color != DIVIDER for _y, color in state["collapsed_samples"]), (
            f"no divider may be drawn while the sidebar is collapsed (Mac hides "
            f"its own); leading column reads {_fmt(state['collapsed_samples'])} "
            f"(screenshot: {collapsed_path})"
        )
    finally:
        _toggle_to_visible(roost)


@pytest.mark.skipif(
    not TEST_MODE,
    reason="window.resize requires ROOST_TEST_MODE=1 in the UI's launch env",
)
def test_notification_dots_paint_the_accent(roost, project, target, tmp_path):
    """Both notification dots — the tab-pill badge and the sidebar
    project-row dot — render `chrome::NOTIFICATION_BADGE` (#007aff, the
    Mac's `controlAccentColor`), and the tab badge sits inside its own
    pill, trailing the title (#311).

    Pixel oracle by design, like the rest of this module: `tab.dump` /
    `tab.list` expose the `has_notification` BIT, and nothing about the
    color or the position the bit is painted at. The regression #311
    fixed — the dots drawn in `NOTIFICATION` (#4e9af1) instead of the
    accent — is invisible to every dump op.
    """
    if target != "iced":
        pytest.skip("iced chrome fill; mac reads controlAccentColor live and gtk uses CSS")

    roost.window_resize(WINDOW_W, WINDOW_H)
    _toggle_to_visible(roost)
    metrics = roost.window_metrics()
    assert metrics["sidebar_collapsed"] is False, (
        f"the sidebar must be expanded for the project-row dot: {metrics}"
    )
    sidebar_w = int(metrics["sidebar_width"])
    assert sidebar_w > 0, f"sidebar allocated no width: {metrics}"

    # Short pinned titles so both pills fit the band with room to spare —
    # the geometry below reads the gap between them, so an overflowing
    # strip (the other test in this module) would clip the bracket.
    tab_a = roost.open_tab(project, cwd="/tmp", title="alpha", argv=BARE_SHELL_ARGV)
    tab_b = roost.open_tab(project, cwd="/tmp", title="bravo", argv=BARE_SHELL_ARGV)
    # Explicit, not inherited: the badge's render guard is
    # `has_notification && !active`, so which tab is active is a
    # precondition of the whole test, never an assumed side effect of
    # opening the second tab.
    roost.focus(tab_b)
    assert roost.identify()["active_tab_id"] == tab_b, (
        "tab B must be active so tab A's badge is the one that renders"
    )

    artifact_dir = os.environ.get("ROOST_E2E_ARTIFACT_DIR")
    base = Path(artifact_dir) if artifact_dir else tmp_path
    base.mkdir(parents=True, exist_ok=True)
    shot_path, cleared_path = base / "tab_badge.png", base / "tab_badge_cleared.png"
    state: dict = {}

    def _strip_badges(shot):
        return _blobs(
            shot, NOTIFICATION_BADGE, x0=sidebar_w, y1=BAND_HEIGHT,
            tol=COLOR_TOL, min_side=MIN_BADGE_SIDE,
        )

    def _sidebar_badges(shot):
        return _blobs(
            shot, NOTIFICATION_BADGE, x1=sidebar_w, tol=COLOR_TOL, min_side=MIN_BADGE_SIDE,
        )

    def _active_pills(shot):
        return _blobs(
            shot, ACTIVE_TAB, x0=sidebar_w, y1=BAND_HEIGHT, min_side=MIN_PILL_SIDE,
        )

    # 1. Baseline. Both pills on screen, no notification anywhere: not one
    #    pixel of the accent may be painted. This is what keeps the search
    #    below honest — it proves the blobs are notification-driven and not
    #    some permanent fixture that happens to wear the same color.
    def _both_pills_painted() -> bool:
        shot = _capture(roost, shot_path)
        if shot is None:
            return False
        state["shot"] = shot
        return len(_active_pills(shot)) == 1

    try:
        roost._wait(_both_pills_painted, 10.0, "the active tab pill painted in the strip")
    except Timeout as exc:
        raise AssertionError(
            f"the active pill never painted, so the baseline is not yet "
            f"meaningful (screenshot: {shot_path})"
        ) from exc

    stray = _blobs(state["shot"], NOTIFICATION_BADGE, tol=COLOR_TOL)
    assert not stray, (
        f"the accent #{NOTIFICATION_BADGE[0]:02x}{NOTIFICATION_BADGE[1]:02x}"
        f"{NOTIFICATION_BADGE[2]:02x} is painted with no notification pending, "
        f"so finding it later would prove nothing: {stray[:5]} "
        f"(screenshot: {shot_path})"
    )

    # 2. Notify the INACTIVE tab A and wait for the painted frame, not just
    #    the model bit — the bit lands an IPC round-trip before the redraw.
    roost.notify(tab_a, "agent needs you", "waiting on input")
    roost.wait_notification(tab_a, True)

    def _dots_painted() -> bool:
        shot = _capture(roost, shot_path)
        if shot is None:
            return False
        state["shot"] = shot
        state["strip"] = _strip_badges(shot)
        state["sidebar"] = _sidebar_badges(shot)
        return bool(state["strip"]) and bool(state["sidebar"])

    try:
        roost._wait(_dots_painted, 10.0, "both notification dots painted")
    except Timeout as exc:
        raise AssertionError(
            f"notification dots never reached the screen: tab-strip blobs "
            f"{state.get('strip')}, sidebar blobs {state.get('sidebar')} "
            f"(screenshot: {shot_path})"
        ) from exc

    shot, strip, sidebar = state["shot"], state["strip"], state["sidebar"]

    # 3/4. Exactly one dot per region, each exactly the accent at its centre.
    #      Found fuzzily (COLOR_TOL absorbs compositing rounding), asserted
    #      exactly — a one-off shade is a real regression, not rounding.
    assert len(strip) == 1, (
        f"expected exactly one tab-strip notification badge (tab A, inactive); "
        f"got {strip} (screenshot: {shot_path})"
    )
    assert len(sidebar) == 1, (
        f"expected exactly one sidebar project-row dot (one project has a "
        f"notified tab); got {sidebar} (screenshot: {shot_path})"
    )
    lo, hi = BADGE_SIDE_RANGE
    for what, (minx, miny, maxx, maxy) in (("tab-strip badge", strip[0]),
                                           ("sidebar project dot", sidebar[0])):
        got = _pixel(shot, (minx + maxx) // 2, (miny + maxy) // 2)
        assert got == NOTIFICATION_BADGE, (
            f"{what} at ({minx},{miny})-({maxx},{maxy}): centre colour "
            f"#{got[0]:02x}{got[1]:02x}{got[2]:02x} != expected "
            f"#{NOTIFICATION_BADGE[0]:02x}{NOTIFICATION_BADGE[1]:02x}"
            f"{NOTIFICATION_BADGE[2]:02x} (screenshot: {shot_path})"
        )
        w, h = maxx - minx + 1, maxy - miny + 1
        assert lo <= w <= hi and lo <= h <= hi, (
            f"{what} measures {w}x{h}px, outside the {lo}..{hi} band a "
            f"{int(NOTIFICATION_DOT_SIZE)}px dot may occupy "
            f"(screenshot: {shot_path})"
        )

    b_minx, b_miny, b_maxx, b_maxy = strip[0]

    # 4b. The sidebar blob is provably the project row's dot: it sits at the
    #     pill's reserved trailing inset. Without this the x-bounded search
    #     only proves "an accent-coloured dot somewhere in the sidebar".
    s_maxx = sidebar[0][2]
    trailing = sidebar_w - 1 - s_maxx
    assert abs(trailing - SIDEBAR_DOT_TRAILING_INSET) <= 2, (
        f"the sidebar dot's right edge is {trailing}px in from the sidebar's "
        f"trailing edge, not the {SIDEBAR_DOT_TRAILING_INSET}px "
        f"PROJECT_PILL_INSET_X + PROJECT_DOT_INSET reserve — so it is not the "
        f"project row's notification dot (blob {sidebar[0]}, sidebar_w "
        f"{sidebar_w}, screenshot: {shot_path})"
    )

    # 5. Vertically centred in the band: the pill is centred by
    #    `BAND_PILL_PADDING_Y` and the badge is `align_y(Center)` inside it.
    centre_y = (b_miny + b_maxy) / 2
    assert abs(centre_y - BAND_HEIGHT / 2) <= 1, (
        f"tab badge centre row {centre_y} is not within 1px of the band's "
        f"middle ({BAND_HEIGHT / 2}) (screenshot: {shot_path})"
    )

    # 6. Horizontally bracketed: the badge belongs to tab A's pill — right of
    #    A's title, left of the active pill's fill, and close enough to it
    #    that only the two containers' padding + the row spacing fit between.
    pills = _active_pills(shot)
    assert len(pills) == 1, (
        f"expected exactly one ACTIVE_TAB pill fill in the band; got {pills} "
        f"(screenshot: {shot_path})"
    )
    pill_minx = pills[0][0]
    assert b_maxx < pill_minx, (
        f"the badge (x {b_minx}..{b_maxx}) must sit left of the active pill "
        f"(x {pill_minx}) — it belongs to the inactive tab (screenshot: {shot_path})"
    )
    gap = pill_minx - b_maxx
    gap_lo, gap_hi = BADGE_TO_PILL_GAP_RANGE
    assert gap_lo <= gap <= gap_hi, (
        f"the badge ends {gap}px before the active pill, outside the "
        f"{gap_lo}..{gap_hi}px the pill padding + row spacing allow: it is "
        f"either floating in the inter-pill region or has drifted right out "
        f"of its own pill (screenshot: {shot_path})"
    )
    rightmost_title = _rightmost_title_pixel(shot, sidebar_w, pill_minx, BAND_HEIGHT)
    assert 0 <= rightmost_title < b_minx, (
        f"tab A's title must end before the badge starts (badge x={b_minx}, "
        f"rightmost MUTED_TEXT glyph pixel left of the active pill "
        f"x={rightmost_title}) (screenshot: {shot_path})"
    )

    # 7. Negative, isolated: clear the notification WITHOUT focusing tab A.
    #    Focusing would flip both inputs of `has_notification && !active` at
    #    once, so a blank screen afterwards would not say which one did it.
    roost.clear_notification(tab_a)
    roost.wait_notification(tab_a, False)

    def _dots_gone() -> bool:
        shot = _capture(roost, cleared_path)
        if shot is None:
            return False
        state["strip"] = _strip_badges(shot)
        state["sidebar"] = _sidebar_badges(shot)
        return not state["strip"] and not state["sidebar"]

    try:
        roost._wait(_dots_gone, 10.0, "both notification dots cleared from the screen")
    except Timeout as exc:
        raise AssertionError(
            f"clearing tab A's notification left dots on screen: tab-strip "
            f"{state['strip']}, sidebar {state['sidebar']} — tab A is still "
            f"inactive, so only the notification bit changed "
            f"(screenshot: {cleared_path})"
        ) from exc
