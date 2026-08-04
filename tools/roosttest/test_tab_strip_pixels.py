"""Tab-strip band pixel guard — issue #281, iced only.

With enough wide tabs the strip's horizontal scrollable overflows, and
iced's stock scrollbar painted a 10px filled rail + scroller straight
across the 24px pills — a gray band across the tab row. The fix is a
zero-width scrollbar (any visible indicator overlays the pills), which
`tab.dump`/`tab.list` cannot see; only pixels can.

*Asserted* — with the strip overflowing, no row of the tab band contains
a long horizontal run of a single color other than the band's own chrome
fills (`SURFACE_DARK` band background, `ACTIVE_TAB` pill fill — our own
constants in `chrome.rs`). The stock rail (~870px), its scroller
(~350px), and even the interim 2px hover sliver all form such runs; tab
title glyphs, status dots, and antialiasing never do. Color-agnostic on
purpose: a scrollbar reintroduced in ANY theme color is caught, not just
the iced-Dark grays observed in #281.

*Not asserted* — text position or metrics (font-dependent), the clipped
right-most pill's exact edge, and hover states (headless capture has no
pointer inside the window; the zero-width scrollbar draws nothing in any
state anyway).

Iced-only: the mac/gtk strips are native scroll views with overlay
scrollers and have never shown this artifact; their band colors differ.

`window.resize` is test-mode-gated, so the file skips without
`ROOST_TEST_MODE=1` — same convention as `test_sidebar_pixels.py`.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from client import Timeout
from test_sidebar_pixels import _capture
from util import BARE_SHELL_ARGV

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "screenshot"))
import pngtool  # noqa: E402  — pure stdlib PNG decoder, imported not shelled out

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

WINDOW_W, WINDOW_H = 1100.0, 700.0

# Verbatim from `crates/roost-iced/src/chrome.rs`: BAND_HEIGHT bounds the
# scan; the two fills are the only colors allowed to run wide in the band.
BAND_HEIGHT = 34
SURFACE_DARK = (0x21, 0x21, 0x21)
ACTIVE_TAB = (0x24, 0x37, 0x51)
ALLOWED_WIDE = {SURFACE_DARK, ACTIVE_TAB}

# The tab title color, verbatim from `chrome.rs` MUTED_TEXT — used only as
# the readiness/overflow probe (glyph pixels render exactly this color).
MUTED_TEXT = (0xA0, 0xA4, 0xB0)

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
        # The +/notification buttons occupy the last ~70px of the band, and
        # the empty-inbox glyph is itself MUTED_TEXT (codex review finding) —
        # exclude that cluster so only tab-title glyphs can satisfy the probe.
        rightmost = _rightmost_title_pixel(shot, sidebar_w + 2, width - 70, BAND_HEIGHT)
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
