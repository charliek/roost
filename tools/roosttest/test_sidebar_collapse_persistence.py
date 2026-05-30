"""Sidebar collapsed state must survive programmatic active-project changes.

Two regressions in the same class, locked in here:

1. `test_sidebar_collapsed_state_survives_relaunch` — the launch path.
   Bug (pre-existing on main, surfaced while spot-checking PR #180):
   the Mac launch path correctly applied the saved `RoostSidebarVisible=
   false` and collapsed the sidebar, but `bootstrapWorkspace` then called
   `selectProject` → `ensureSidebarVisible` → `toggleSidebar`, which
   restored the sidebar AND rewrote `RoostSidebarVisible=true` to
   UserDefaults — silently erasing the user's preference on every launch.
   First fix (PR #181): `selectProject(id:revealSidebar:)` parameter;
   `bootstrapWorkspace` passes `revealSidebar: false`.

2. `test_collapsed_sidebar_survives_project_delete` — the event-reconcile
   path. PR #181's tactical fix left three more programmatic callers
   (the two event-reconcile arms and `focusTab → selectProject`) still
   defaulting to `revealSidebar: true`. Deleting the active project
   fires `.projectDeleted` → next-pick `selectProject(id: next.id)`,
   silently uncollapsing the sidebar by the same path.
   Fix: refactor `selectProject` to pure data mutation (drop the
   parameter entirely) and move `ensureSidebarVisible()` to the
   user-action call sites. Matches GTK's `set_active_project` and the
   vision.md DL-11 principle.

Both UIs persist the collapse choice: the Mac UI in UserDefaults
(`RoostSidebarVisible`), the GTK/Linux UI in `state.json`
(`SnapshotFile.sidebar_collapsed`, restored synchronously in `App::new`
and written through on `toggle_sidebar`). So this runs and passes on a
developer GTK build (Linux or the macOS GTK dev profile) too, not just Mac.

Skip on CI entirely:
- GTK CI runs under bare xvfb (no WM); the quit/relaunch lifecycle is
  unreliable there.
- Mac CI sets `ROOST_TEST_RESET_STATE=1` so `ui._mac_cleanup` deletes
  `state.json` between launches, plus the GUI runner is slow enough
  that the mid-test `ui.quit + ui.launch` cycle frequently exceeds
  the 90s `wait_alive` budget even after the harness's retry.

Runs locally on a developer Mac (and on a developer GTK build under a
real window manager) — where the fix is actually iterated, the test
reliably catches the bug.
"""

from __future__ import annotations

import os

import pytest

import ui
from client import Roost


SKIP_ON_CI = os.environ.get("CI") == "true"


@pytest.fixture(autouse=True)
def _skip_on_ci():
    if SKIP_ON_CI:
        pytest.skip(
            "quit + relaunch is unreliable on CI: GTK xvfb has no WM, and the "
            "Mac runner's ROOST_TEST_RESET_STATE=1 nukes state.json + slow "
            "LaunchServices respawn pushes wait_alive past its 90s budget. "
            "Runs locally where the fix is actually iterated."
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


def test_collapsed_sidebar_survives_project_delete(roost, target):
    """Deleting the active project must not auto-reveal the sidebar.

    The `.projectDeleted` event arm at App.swift:1500 runs a next-pick
    `selectProject(id: next.id)`. Before this refactor, that call took
    the default `revealSidebar: true` (the parameter PR #181 added was
    only threaded into `bootstrapWorkspace`), which silently
    uncollapsed a sidebar the user had hidden via ⌘B and rewrote
    `RoostSidebarVisible = true` to UserDefaults — the same class of
    bug as the launch-time path #181 fixed, on a different code path.

    After the refactor, `selectProject` is pure data mutation; only
    user-action call sites call `ensureSidebarVisible()`. The
    next-pick path preserves the user's collapse intent.
    """
    # 1. Baseline: sidebar visible.
    _toggle_to_visible(roost)
    assert not roost.window_metrics()["sidebar_collapsed"]

    # 2. Ensure there are at least 2 projects so deleting the active one
    #    leaves a `next` for the `.projectDeleted` arm to pick. The IPC
    #    `project.create` does NOT auto-switch active on the Mac side
    #    (its `.projectCreated` arm only calls `insertProjectLocally`),
    #    so the freshly-created sibling stays non-active and the
    #    existing active project remains the deletion target.
    sibling_id = roost.create_project(name="", cwd="/tmp")
    assert len(roost.list()) >= 2

    ident = roost.identify()
    active_id = ident["active_project_id"]
    assert active_id != 0, "must have an active project before deletion"
    assert active_id != sibling_id, (
        "fresh sibling must not be the active one — `.projectCreated` "
        "inserts locally without switching active selection"
    )

    # 3. Collapse the sidebar (writes RoostSidebarVisible=false).
    _toggle_to_collapsed(roost)
    assert roost.window_metrics()["sidebar_collapsed"]

    # 4. Delete the active project. The `.projectDeleted` event fires
    #    on the Mac side; the next-pick path at App.swift:1500 calls
    #    `selectProject(id: next.id)`.
    roost.delete_project(active_id)

    # 5. Wait for the active to switch off the deleted id, then assert
    #    the sidebar is still collapsed. Pre-fix, the next-pick
    #    `selectProject` would have uncollapsed it.
    Roost._wait(
        lambda: roost.identify()["active_project_id"] != active_id,
        timeout=5.0,
        what="next project to become active after delete",
    )
    after = roost.window_metrics()
    assert after["sidebar_collapsed"], (
        f"sidebar must stay collapsed after deleting the active project, "
        f"got {after}. Pre-fix, the `.projectDeleted` arm's next-pick "
        "selectProject silently auto-revealed it."
    )

    # 6. Restore visible state for any later tests in the same session.
    _toggle_to_visible(roost)
    assert not roost.window_metrics()["sidebar_collapsed"]
