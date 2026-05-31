"""Sidebar layout regression — sidebar holds its width across window resizes.

The bug (#TBD, sibling to #159): on macOS, widening the window grew the
project sidebar proportionally instead of letting the terminal pane
absorb the delta. GTK was correct via `gtk4::Paned` with
`resize_start_child(false) + shrink_start_child(false)`. This file
locks in the parity assertion on both UIs.

`window.resize` is test-mode-gated, so the whole file is skipped when
`ROOST_TEST_MODE` is unset — the bare resize attempt would fail with
`not-enabled` and produce noisy "wrong reason" failures.

Capability gate (instead of a blanket CI skip): a resize only lands if
the environment honors it up to the available screen. GTK CI runs xvfb
with a large virtual screen (`-screen 0 2560x1440x24`) so the toplevel
resizes the full amount and these run there; but a tiling/constraining
compositor (some local setups) or a small screen may grant only a
partial — or no — resize. So we *measure the achieved window delta* and:
  - assert the sidebar-hold invariant when the delta is large enough to
    be meaningful (>= USABLE_DELTA_PT — the original bug grew the sidebar
    ~20% of the window delta, far above the 1pt tolerance);
  - emit a registered skip when the environment granted too little resize.
We never fail via a resize timeout.

Tolerance: 1pt. Divider thickness and HiDPI rounding can shift the
measured pane width sub-point; 1pt is well under the ~140pt regression.
"""

from __future__ import annotations

import os

import pytest

from client import Roost, Timeout


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"
WIDTH_TOLERANCE_PT = 1.0
# Achieved window-width delta we need before the hold-invariant assertion
# is meaningful (the bug would grow the sidebar well beyond 1pt over this).
USABLE_DELTA_PT = 200.0


def _wait_window_width(roost, target_width: float, timeout: float = 2.0) -> dict:
    """Block until the UI reports the requested window width, then return
    the full metrics. Raises `Timeout` if the width never gets there.
    Routes through `Roost._wait` so `ROOST_TEST_TIMEOUT_SCALE` scales it."""
    Roost._wait(
        lambda: abs(roost.window_metrics()["window_width"] - target_width)
        <= WIDTH_TOLERANCE_PT,
        timeout=timeout,
        what=f"window width to reach {target_width}",
    )
    return roost.window_metrics()


def _resize_settle(roost, target_width: float) -> dict:
    """Request `target_width` and return the settled metrics. Does NOT fail
    if the WM refuses or only partially grants the resize — the caller
    gates on the achieved delta. (GTK applies resizes asynchronously, so
    we still wait for the width to reach the target when the WM allows it.)
    """
    roost.window_resize(target_width, 700)
    try:
        return _wait_window_width(roost, target_width)
    except Timeout:
        return roost.window_metrics()  # WM granted less than requested


@pytest.mark.skipif(
    not TEST_MODE,
    reason="window.resize requires ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestSidebarLayout:
    def test_sidebar_holds_width_on_window_resize(self, roost):
        """Widening the window MUST NOT widen the sidebar.

        Pre-refactor on Mac (raw NSSplitView + tied-priority constraints):
        sidebar grows by ~20% of the window delta. Post-refactor
        (raw NSSplitView + custom splitView(_:resizeSubviewsWithOldSize:)):
        sidebar holds. GTK has always held; this test pins both behaviors.
        """
        # Seed a known geometry, then widen. The capability gate runs
        # BEFORE any geometry assertion: under a constraining WM (xvfb,
        # tiling compositors) the window size is unreliable, so a baseline
        # assertion would trip spuriously — skip first if the WM won't
        # grant a usable resize.
        before = _resize_settle(roost, 1100)
        after = _resize_settle(roost, 1800)
        achieved = abs(after["window_width"] - before["window_width"])
        if achieved < USABLE_DELTA_PT:
            pytest.skip(
                f"WM granted only a {achieved:.0f}pt window delta "
                f"({before['window_width']:.0f}→{after['window_width']:.0f}); "
                "need ≥200pt to exercise the sidebar-hold invariant (no WM?)"
            )
        # WM cooperated → the geometry is trustworthy. Assert invariants.
        baseline_sidebar = before["sidebar_width"]
        assert not before["sidebar_collapsed"], "sidebar must be visible for the test"
        assert 160 <= baseline_sidebar <= 400, (
            f"sidebar starting width {baseline_sidebar} out of [160, 400]"
        )
        # The bug grew the sidebar ~140pt for a 700pt resize; 1pt clears it.
        assert abs(after["sidebar_width"] - baseline_sidebar) <= WIDTH_TOLERANCE_PT, (
            f"sidebar grew on window resize: before={baseline_sidebar} "
            f"after={after['sidebar_width']} (delta "
            f"{after['sidebar_width'] - baseline_sidebar:+.1f}pt) over a "
            f"{achieved:.0f}pt window widen."
        )

    def test_sidebar_holds_width_on_window_shrink(self, roost):
        """Narrowing the window also MUST NOT shrink the sidebar
        (until it would push past the 160pt minimum). The symmetric
        case of the grow-on-widen bug: both share the holding-priority
        dance, so the regression test pins both directions.
        """
        before = _resize_settle(roost, 1800)
        baseline_sidebar = before["sidebar_width"]

        after = _resize_settle(roost, 1100)
        achieved = abs(after["window_width"] - before["window_width"])
        if achieved < USABLE_DELTA_PT:
            pytest.skip(
                f"WM granted only a {achieved:.0f}pt window delta "
                f"({before['window_width']:.0f}→{after['window_width']:.0f}); "
                "need ≥200pt to exercise the sidebar-hold invariant (no WM?)"
            )
        # The shrink target (1100) is still wide enough that the
        # sidebar shouldn't be compressed (sidebar + min terminal width).
        assert abs(after["sidebar_width"] - baseline_sidebar) <= WIDTH_TOLERANCE_PT, (
            f"sidebar shrank on window narrow: before={baseline_sidebar} "
            f"after={after['sidebar_width']} over a {achieved:.0f}pt window narrow."
        )
