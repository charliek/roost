"""Sidebar collapsed state must survive a quit + relaunch cycle.

Bug (pre-existing on main, surfaced while spot-checking PR #180):
the Mac launch path correctly applied the saved `RoostSidebarVisible=
false` and collapsed the sidebar, but `bootstrapWorkspace` then called
`selectProject` → `ensureSidebarVisible` → `toggleSidebar`, which
restored the sidebar AND rewrote `RoostSidebarVisible=true` to
UserDefaults — silently erasing the user's preference on every launch.

Fix: `selectProject(id:revealSidebar:)` parameter; `bootstrapWorkspace`
passes `revealSidebar: false`.

Skip on GTK in CI for the same reason `test_sidebar_layout.py` does —
bare xvfb has no window manager and the quit/relaunch lifecycle is
unreliable there. Mac CI exercises this on a real GUI runner; GTK
locally (under a real WM) also passes and acts as the parity lock.
"""

from __future__ import annotations

import os

import pytest

import ui
from client import Roost


SKIP_GTK_IN_CI = os.environ.get("CI") == "true"


@pytest.fixture(autouse=True)
def _skip_gtk_in_ci(target):
    if SKIP_GTK_IN_CI and target == "gtk":
        pytest.skip(
            "GTK target on CI uses bare xvfb (no WM); the quit + relaunch "
            "lifecycle in this test is unreliable there. Runs locally on a "
            "developer GTK build, and on Mac CI."
        )


def _toggle_to_collapsed(roost: Roost) -> None:
    """Drive the palette to collapse the sidebar. No-op if already collapsed."""
    metrics = roost.window_metrics()
    if metrics["sidebar_collapsed"]:
        return
    roost.palette_open()
    roost.palette_query("toggle sidebar")
    roost.palette_activate("toggle_sidebar")


def _toggle_to_visible(roost: Roost) -> None:
    """Drive the palette to restore the sidebar. No-op if already visible."""
    metrics = roost.window_metrics()
    if not metrics["sidebar_collapsed"]:
        return
    roost.palette_open()
    roost.palette_query("toggle sidebar")
    roost.palette_activate("toggle_sidebar")


def test_sidebar_collapsed_state_survives_relaunch(roost, target):
    """The full bug repro + lock-in.

    1. Baseline: ensure sidebar is visible.
    2. Collapse via the palette (writes RoostSidebarVisible=false).
    3. Quit the UI; the harness's idempotent quit path waits for the
       socket to disappear.
    4. Relaunch the UI; wait until its IPC socket answers `identify`.
    5. Assert the relaunched UI reports `sidebar_collapsed=true`. Pre-fix,
       bootstrapWorkspace's `selectProject` would have re-revealed it.
    6. Restore visible state so the rest of the test session is clean.
    """
    # 1. Baseline: visible.
    _toggle_to_visible(roost)
    assert not roost.window_metrics()["sidebar_collapsed"]

    # 2. Collapse via the palette.
    _toggle_to_collapsed(roost)
    assert roost.window_metrics()["sidebar_collapsed"], (
        "collapse-via-palette must take effect before the relaunch step"
    )

    # 3. Quit the UI cleanly. `ui.quit` blocks until the socket dies.
    roost.close()
    ui.quit(target)

    # 4. Relaunch + wait for the new socket to answer.
    ui.launch(target)

    # 5. The regression-locking assertion. The relaunched UI must report
    #    the sidebar still collapsed.
    fresh = Roost(ui.socket_path(target))
    try:
        after = fresh.window_metrics()
        assert after["sidebar_collapsed"], (
            f"sidebar must stay collapsed across relaunch, got {after}. "
            "Pre-fix, bootstrapWorkspace's selectProject auto-uncollapsed it."
        )
        # 6. Restore visible state for any later tests in the same session.
        _toggle_to_visible(fresh)
        assert not fresh.window_metrics()["sidebar_collapsed"]
    finally:
        fresh.close()
