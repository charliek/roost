"""Sidebar layout regression — sidebar holds its width across window resizes.

The bug (#TBD, sibling to #159): on macOS, widening the window grew the
project sidebar proportionally instead of letting the terminal pane
absorb the delta. GTK was correct via `gtk4::Paned` with
`resize_start_child(false) + shrink_start_child(false)`. This file
locks in the parity assertion on both UIs.

`window.resize` is test-mode-gated, so the whole file is skipped when
`ROOST_TEST_MODE` is unset — the bare resize attempt would fail with
`not-enabled` and produce noisy "wrong reason" failures.

Tolerance: 1pt. Divider thickness and HiDPI rounding can shift the
measured pane width by a sub-point in either direction; 1pt is well
under the regression we're catching (the original bug grew the sidebar
by ~140pt over a 700pt window resize).
"""

from __future__ import annotations

import os
import time

import pytest

from client import scaled_timeout


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"
WIDTH_TOLERANCE_PT = 1.0


def _wait_window_width(roost, target_width: float, timeout: float = 2.0) -> dict:
    """Block until the UI reports the requested window width, then return
    the full metrics. GTK is asynchronous about applying the resize (the
    request hops to the main context and re-allocates on the next idle);
    polling avoids a fragile time.sleep.
    """
    deadline = time.monotonic() + scaled_timeout(timeout)
    last = roost.window_metrics()
    while abs(last["window_width"] - target_width) > WIDTH_TOLERANCE_PT:
        if time.monotonic() >= deadline:
            raise TimeoutError(
                f"window never reached width {target_width} "
                f"(last seen {last['window_width']})"
            )
        time.sleep(0.05)
        last = roost.window_metrics()
    return last


@pytest.mark.skipif(
    not TEST_MODE,
    reason="window.resize requires ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestSidebarLayout:
    def test_sidebar_holds_width_on_window_resize(self, roost):
        """Widening the window MUST NOT widen the sidebar.

        Pre-refactor on Mac (raw NSSplitView + tied-priority constraints):
        sidebar grows by ~20% of the window delta. Post-refactor
        (NSSplitViewController + NSSplitViewItem): sidebar holds.
        GTK has always held; this test pins both behaviors.
        """
        # Seed a known geometry. Use values comfortably inside the
        # sidebar's [160, 400] clamp so a tolerance miss doesn't get
        # swallowed by a constraint clip.
        roost.window_resize(1100, 700)
        before = _wait_window_width(roost, 1100)
        baseline_sidebar = before["sidebar_width"]
        assert not before["sidebar_collapsed"], "sidebar must be visible for the test"
        assert 160 <= baseline_sidebar <= 400, (
            f"sidebar starting width {baseline_sidebar} out of [160, 400]"
        )

        # The actual assertion. The bug grew the sidebar by ~140pt for
        # this resize; a 1pt tolerance is well clear of that.
        roost.window_resize(1800, 700)
        after = _wait_window_width(roost, 1800)
        assert abs(after["sidebar_width"] - baseline_sidebar) <= WIDTH_TOLERANCE_PT, (
            f"sidebar grew on window resize: before={baseline_sidebar} "
            f"after={after['sidebar_width']} (delta "
            f"{after['sidebar_width'] - baseline_sidebar:+.1f}pt). "
            f"Window: {before['window_width']} → {after['window_width']}."
        )

    def test_sidebar_holds_width_on_window_shrink(self, roost):
        """Narrowing the window also MUST NOT shrink the sidebar
        (until it would push past the 160pt minimum). The symmetric
        case of the grow-on-widen bug: both share the holding-priority
        dance, so the regression test pins both directions.
        """
        roost.window_resize(1800, 700)
        before = _wait_window_width(roost, 1800)
        baseline_sidebar = before["sidebar_width"]

        roost.window_resize(1100, 700)
        after = _wait_window_width(roost, 1100)
        # The shrink target (1100) is still wide enough that the
        # sidebar shouldn't be compressed (sidebar + min terminal width).
        assert abs(after["sidebar_width"] - baseline_sidebar) <= WIDTH_TOLERANCE_PT, (
            f"sidebar shrank on window narrow: before={baseline_sidebar} "
            f"after={after['sidebar_width']}"
        )
