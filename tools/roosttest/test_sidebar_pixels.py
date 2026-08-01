"""Sidebar agent-row pixel guards — plan 007, all three targets
(`--roost-target mac|gtk|iced`). The only two things about the rendered rows
that `app.sidebar_dump` cannot see: the lifecycle DOT COLOUR and the
column the dots line up in.

Low-flake by construction, so the scope is deliberately narrow:

*Asserted* — the four lifecycle colours (`#5fa3f0` working, `#f0a040`
waiting, `#7a7a7a` finished, `#e05252` failed), which are our own
constants spelled in `resources/style.css` and `ProjectRollup.swift`,
and the dots' shared LEFT EDGE, which comes from layout constraints
(Mac: `dot.leadingAnchor + 8` inside the row's leading inset) and CSS
padding (GTK: `.roost-sidebar-agent { padding-left: 10px }` inside a
6px margin). Neither depends on font metrics or on the theme.

*Not asserted*, on purpose — whole-image golden diffs (antialiasing and
font rasterization differ per machine), anything derived from text
position or size, row heights derived from text, and the Mac project
row's selection pill (it uses the system `controlAccentColor`, which is
a per-user preference).

The dots are found by colour, then reduced to *solid blobs* (a
connected component at least `MIN_DOT_SIDE` px on both axes). That
filter is what makes the reads unambiguous: the project row's 3px
lifecycle stripe wears the same four colours but is too narrow, and
antialiased grey text pixels near `#7a7a7a` are too small. The scan is
also clipped to the sidebar's own width, so the tab pill's status dot
(same palette, different chrome) can't be mistaken for a sidebar dot.

`window.resize` is test-mode-gated, so the file skips without
`ROOST_TEST_MODE=1` — same convention as `test_sidebar_layout.py`.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from client import RoostError, Timeout
from test_agent_palette import _seed
from test_sidebar_agents import _agent_row, _set_agents_visible
from test_sidebar_collapse_persistence import _toggle_to_visible
from util import BARE_SHELL_ARGV

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "screenshot"))
import pngtool  # noqa: E402  — pure stdlib PNG decoder, imported not shelled out

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# Pinned geometry: the dots are left-anchored so the window size doesn't
# move them, but a fixed size keeps the rows on screen and the capture
# small.
WINDOW_W, WINDOW_H = 1100.0, 700.0

# The four lifecycle colours, verbatim from `resources/style.css`
# (`.roost-sidebar-agent-dot.agent-*`) and `rollupColor` in
# `ProjectRollup.swift`. `inactive` is deliberately absent: it renders at
# 50% alpha over the sidebar background, so it has no fixed RGB.
LIFECYCLE_COLORS: dict[str, tuple[int, int, int]] = {
    "working": (0x5F, 0xA3, 0xF0),
    "waiting": (0xF0, 0xA0, 0x40),
    "finished": (0x7A, 0x7A, 0x7A),
    "failed": (0xE0, 0x52, 0x52),
}

# Left edge (x, in the screenshot's pixels — `app.screenshot` at scale 1
# renders one pixel per logical point on both UIs) shared by every agent
# dot: the centre of the band it must fall in, per target.
#
# The dot sits a fixed distance inside its ROW (our own margin+padding),
# but the row's own origin is set by the toolkit's padding, and that is
# not the same everywhere: the gtk target measures x=25 against macOS
# Homebrew GTK and x=29 against libadwaita on the Ubuntu CI runner. The
# gtk band is therefore centred between them and widened to cover both;
# AppKit does not vary that way, so mac keeps a tight band.
#
# This is deliberately a coarse guard. The precise, platform-independent
# invariant — every dot on one edge — is asserted exactly below; this
# band only catches gross indentation, which is the regression it exists
# for (the dot was 10px past the project name and still is caught).
DOT_LEFT_X = {"gtk": 27, "iced": 21, "mac": 25}
DOT_LEFT_TOLERANCE = {"gtk": 6, "iced": 2, "mac": 3}

# All three UIs draw an 8x8 rounded dot (AppKit/GTK use a 4px radius; Iced
# uses 3px to retain a solid renderer-neutral edge). The saturated core is
# at least 5x5, while the project lifecycle stripe is only 3px wide.
MIN_DOT_SIDE = 5
# Colour match tolerance for *finding* a dot. 0 and 2 select identical
# pixels on both targets today; 2 leaves room for compositing rounding
# without reaching a neighbouring shade. The colour itself is then
# asserted exactly, at the blob's centre.
COLOR_TOL = 2


def _components(points: set[tuple[int, int]]) -> list[tuple[int, int, int, int]]:
    """4-connected components of `points`, each as (minx, miny, maxx, maxy)."""
    seen: set[tuple[int, int]] = set()
    out = []
    for start in points:
        if start in seen:
            continue
        seen.add(start)
        stack = [start]
        comp = []
        while stack:
            x, y = stack.pop()
            comp.append((x, y))
            for n in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)):
                if n in points and n not in seen:
                    seen.add(n)
                    stack.append(n)
        xs = [c[0] for c in comp]
        ys = [c[1] for c in comp]
        out.append((min(xs), min(ys), max(xs), max(ys)))
    return sorted(out)


def _dot_blobs(shot, max_x: int) -> dict[str, list[tuple[int, int, int, int]]]:
    """Solid, dot-sized blobs of each lifecycle colour within `max_x`."""
    width, height, bpp, px = shot
    max_x = max(0, min(max_x, width))
    matches: dict[str, set[tuple[int, int]]] = {name: set() for name in LIFECYCLE_COLORS}
    for y in range(height):
        base = y * width * bpp
        for x in range(max_x):
            o = base + x * bpp
            r, g, b = px[o], px[o + 1], px[o + 2]
            for name, (tr, tg, tb) in LIFECYCLE_COLORS.items():
                if abs(r - tr) <= COLOR_TOL and abs(g - tg) <= COLOR_TOL and abs(b - tb) <= COLOR_TOL:
                    matches[name].add((x, y))
                    break
    return {
        name: [
            c
            for c in _components(pts)
            if (c[2] - c[0] + 1) >= MIN_DOT_SIDE and (c[3] - c[1] + 1) >= MIN_DOT_SIDE
        ]
        for name, pts in matches.items()
    }


def _pixel(shot, x: int, y: int) -> tuple[int, int, int]:
    width, _height, bpp, px = shot
    o = (y * width + x) * bpp
    return (px[o], px[o + 1], px[o + 2])


def _capture(roost, path: Path):
    """Decode one `app.screenshot` (scale 1) capture, or None while the
    window is mid-relayout. GTK renders the capture off a live widget
    snapshot, which comes back empty (`internal: empty snapshot`) for a
    frame or two right after a resize — a transient to poll through, not
    a failure."""
    try:
        png, _w, _h = roost.screenshot()
    except RoostError as e:
        if e.code == "internal" and "empty snapshot" in e.message:
            return None
        raise
    path.write_bytes(png)
    return pngtool.load(str(path))


def _seed_one_per_lifecycle(roost, project) -> dict[str, int]:
    tabs = {}
    for lifecycle in LIFECYCLE_COLORS:
        tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        _seed(roost, tab, lifecycle=lifecycle, name=f"px-{lifecycle}")
        tabs[lifecycle] = tab
    # `_seed` guarantees the lifecycle stuck (bare shell, no late marks,
    # plus its own tripwire), so all that is left is waiting for the row
    # to reach the sidebar's rendered cache.
    for lifecycle, tab in tabs.items():
        roost._wait(
            lambda t=tab: _agent_row(roost.sidebar_dump(), project, t) is not None,
            10.0,
            f"{lifecycle} agent row for tab {tab} reaches the sidebar dump",
        )
    return tabs


@pytest.mark.skipif(
    not TEST_MODE,
    reason="window.resize requires ROOST_TEST_MODE=1 in the UI's launch env",
)
def test_lifecycle_dot_colors_and_shared_left_edge(roost, project, target, tmp_path):
    """One agent per lifecycle, so all four dot colours are on screen in
    a single capture: each colour must be present as a dot-sized blob
    whose centre is EXACTLY the constant, and every dot in the sidebar
    must start at the same x — the per-target expected column."""
    _set_agents_visible(roost, True)
    _toggle_to_visible(roost)
    _seed_one_per_lifecycle(roost, project)

    # Pin the window (best-effort: a constraining WM may grant less, and
    # the dots are left-anchored either way — see test_sidebar_layout.py
    # for why a refused resize must never fail a test).
    roost.window_resize(WINDOW_W, WINDOW_H)

    artifact_dir = os.environ.get("ROOST_E2E_ARTIFACT_DIR")
    shot_path = (
        Path(artifact_dir) / "sidebar.png"
        if artifact_dir
        else tmp_path / "sidebar.png"
    )
    shot_path.parent.mkdir(parents=True, exist_ok=True)
    state: dict = {"shot": None, "blobs": {}}

    def _all_four_painted() -> bool:
        sidebar_w = int(roost.window_metrics()["sidebar_width"])
        if sidebar_w <= 0:
            return False
        shot = _capture(roost, shot_path)
        if shot is None:
            return False
        blobs = _dot_blobs(shot, sidebar_w)
        state["shot"], state["blobs"] = shot, blobs
        return all(blobs[name] for name in LIFECYCLE_COLORS)

    try:
        roost._wait(_all_four_painted, 10.0, "all four lifecycle dots painted in the sidebar")
    except Timeout as exc:
        found = {k: len(v) for k, v in state["blobs"].items()}
        raise AssertionError(
            f"not every lifecycle dot reached the sidebar: blob counts {found} "
            f"(0 means that colour was never found as a >={MIN_DOT_SIDE}px blob). "
            f"Last screenshot: {shot_path}"
        ) from exc

    shot, blobs = state["shot"], state["blobs"]
    expected_x = DOT_LEFT_X[target]

    for name, want in LIFECYCLE_COLORS.items():
        for minx, miny, maxx, maxy in blobs[name]:
            got = _pixel(shot, (minx + maxx) // 2, (miny + maxy) // 2)
            assert got == want, (
                f"{name} dot at ({minx},{miny})-({maxx},{maxy}): centre colour "
                f"#{got[0]:02x}{got[1]:02x}{got[2]:02x} != expected "
                f"#{want[0]:02x}{want[1]:02x}{want[2]:02x} (screenshot: {shot_path})"
            )

    edges = {
        name: sorted({b[0] for b in found}) for name, found in blobs.items() if found
    }

    # Every dot shares one edge — exact, and toolkit-independent, because
    # they are all positioned by the same rule.
    observed = sorted({x for xs in edges.values() for x in xs})
    assert len(observed) == 1, (
        f"agent dots must all share one left edge on {target}; got {edges} "
        f"(screenshot: {shot_path})"
    )

    # That shared edge sits where the project label starts. Compared with a
    # tolerance rather than exactly: the dot's offset is our own CSS/constraint
    # inset nested inside the toolkit's own row padding, so a different
    # libadwaita minor on a Linux runner can shift it a pixel or two. The
    # regression this guards — the dot indented past the project name — was
    # 10px, so the band still catches it. Asserting the label's own edge
    # instead would mean measuring antialiased text, which is genuinely flaky.
    tolerance = DOT_LEFT_TOLERANCE[target]
    assert abs(observed[0] - expected_x) <= tolerance, (
        f"agent dots start at x={observed[0]} on {target}, expected "
        f"{expected_x}±{tolerance} (screenshot: {shot_path})"
    )
