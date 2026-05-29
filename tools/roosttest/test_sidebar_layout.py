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

import pytest

from client import Roost


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"
WIDTH_TOLERANCE_PT = 1.0
# Bare xvfb (CI) has no window manager, so GTK4's `set_default_size`
# doesn't actually resize a mapped toplevel — the `window.resize` IPC
# op returns immediately but the window allocation stays put. Locally,
# a real WM (Mutter / Quartz on a developer Mac) honors the resize and
# the test runs end-to-end. Skip on GTK in CI; Mac runs always exercise
# the regression. GTK's structural correctness (gtk4::Paned with
# resize_start_child(false)) makes the parity-lock less critical here.
SKIP_GTK_IN_CI = os.environ.get("CI") == "true"


def _wait_window_width(roost, target_width: float, timeout: float = 2.0) -> dict:
    """Block until the UI reports the requested window width, then return
    the full metrics. GTK is asynchronous about applying the resize (the
    request hops to the main context and re-allocates on the next idle).

    Routes through `Roost._wait` so `ROOST_TEST_TIMEOUT_SCALE` scales
    this wait alongside every other wait helper.
    """
    Roost._wait(
        lambda: abs(roost.window_metrics()["window_width"] - target_width)
        <= WIDTH_TOLERANCE_PT,
        timeout=timeout,
        what=f"window width to reach {target_width}",
    )
    return roost.window_metrics()


@pytest.fixture(autouse=True)
def _skip_gtk_in_ci(target):
    if SKIP_GTK_IN_CI and target == "gtk":
        pytest.skip(
            "GTK target on CI uses bare xvfb (no WM), so window.resize is "
            "a no-op there; runs locally on developer GTK and on Mac CI"
        )


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
