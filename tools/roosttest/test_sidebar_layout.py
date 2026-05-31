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


def _window_and_sidebar_settled(roost, target_width: float) -> bool:
    """Predicate: window width is within tolerance AND, when the sidebar
    is visible, its width is non-zero.

    GTK's `set_visible(true)` flips `is_visible()` synchronously but
    queues the actual layout pass on the idle cycle, so a freshly-
    uncollapsed sidebar reports `collapsed=False` with `width=0` until
    that pass runs. Waiting on window-width alone would race that
    interval; the next test then reads `sidebar_width=0` and trips its
    bounds assertion (root cause of the local-env flake fixed in this
    file).

    When the sidebar is genuinely collapsed (`collapsed=True`), the
    second clause short-circuits — `sidebar_width=0` is the correct
    settled state. Backward-compatible with future collapsed-state
    tests.
    """
    m = roost.window_metrics()
    if abs(m["window_width"] - target_width) > WIDTH_TOLERANCE_PT:
        return False
    if not m["sidebar_collapsed"] and m["sidebar_width"] <= 0:
        return False
    return True


def _resize_settle(roost, target_width: float) -> dict:
    """Request `target_width` and return the settled metrics. Does NOT fail
    if the WM refuses, only partially grants, or stalls the resize — the
    caller gates on the achieved delta, and the bounds assertion catches
    the layout-stall case with a metric snapshot so the two failure modes
    don't read identically.
    """
    roost.window_resize(target_width, 700)
    try:
        Roost._wait(
            lambda: _window_and_sidebar_settled(roost, target_width),
            timeout=2.0,
            what=f"window+sidebar settle to {target_width}",
        )
    except Timeout:
        pass  # WM refused, OR sidebar layout stalled — disambiguated downstream
    return roost.window_metrics()


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
            f"sidebar starting width {baseline_sidebar} out of [160, 400] "
            f"(collapsed={before['sidebar_collapsed']}, "
            f"window_width={before['window_width']}). "
            "0.0 with collapsed=False indicates a layout-stall — the GTK "
            "set_visible(true) idle relayout didn't run within the "
            "_resize_settle budget. Check whether a preceding test "
            "toggled the sidebar without waiting for the layout pass "
            "(see test_sidebar_collapse_persistence::_toggle_to_visible)."
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
        # Parity with the grow test: assert visible + bounded baseline.
        # Without these, a `visible=True, sidebar_width=0` transient
        # (GTK layout-stall after a preceding `set_visible(true)`) would
        # produce baseline=0 → after=0 → |0-0|<=1pt false-green, defeating
        # the regression check.
        assert not before["sidebar_collapsed"], "sidebar must be visible for the test"
        assert 160 <= baseline_sidebar <= 400, (
            f"sidebar starting width {baseline_sidebar} out of [160, 400] "
            f"(collapsed={before['sidebar_collapsed']}, "
            f"window_width={before['window_width']}). "
            "0.0 with collapsed=False indicates a layout-stall — see "
            "test_sidebar_holds_width_on_window_resize for the diagnostic."
        )
        # The shrink target (1100) is still wide enough that the
        # sidebar shouldn't be compressed (sidebar + min terminal width).
        assert abs(after["sidebar_width"] - baseline_sidebar) <= WIDTH_TOLERANCE_PT, (
            f"sidebar shrank on window narrow: before={baseline_sidebar} "
            f"after={after['sidebar_width']} over a {achieved:.0f}pt window narrow."
        )
