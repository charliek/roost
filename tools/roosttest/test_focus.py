"""End-to-end terminal keyboard-focus tests for project/tab navigation.

Asserts the UI keeps the active tab's terminal as the window's *logical*
keyboard-focus widget as the user navigates — so typing lands in the
terminal without an extra click (the Swift `makeFirstResponder` policy).

Companion to `test_mouse_tracking.py`, which owns the mode-1004 focus-
*event* + cursor-shape surface; this module owns the *who-holds-focus*
surface via the `app.active_terminal_focused` op. That op reads GTK
logical focus (`window.focus_widget() == terminal`), which `grab_focus()`
sets regardless of whether the toplevel has the compositor's input focus —
so it is observable under the WM-less Xvfb e2e runner, unlike the global
`:has-focus` property.
"""

from __future__ import annotations

import time

import pytest

from client import Roost, RoostError, scaled_timeout
from util import wait_tab_attached


@pytest.fixture(autouse=True)
def _gtk_only(target):
    """These tests read the `app.active_terminal_focused` op, which only
    the GTK UI implements. The Mac UI already has the focus behavior
    (it's the reference); exposing the op + running these on Mac for
    full parity is a follow-up."""
    if target != "gtk":
        pytest.skip("app.active_terminal_focused is implemented by the GTK UI only")


def _wait_terminal_focused(roost, what: str, timeout: float = 2.0) -> None:
    """Poll until the active terminal holds logical focus. Focus can
    settle a main-loop tick after a navigation (the project-switch grab
    is idle-deferred), so every assertion is a condition-wait, never a
    single read."""
    Roost._wait(
        lambda: roost.app_active_terminal_focused(),
        timeout=timeout,
        what=what,
    )


def test_new_tab_focuses_terminal(roost, project):
    """Opening a tab lands logical keyboard focus on its terminal — the
    baseline the navigation tests build on, and the headless-observability
    check for `app.active_terminal_focused` (logical focus is set by
    `grab_focus()` even with no window manager)."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    _wait_terminal_focused(roost, what="new tab's terminal to hold focus")


def test_palette_steals_then_restores_terminal_focus(palette, project):
    """The op must track real focus transitions, not return a constant:
    opening the palette moves logical focus to its entry (terminal → not
    focused); dismissing it returns focus to the terminal. Guards against
    an always-true implementation (and exercises both the `false` and the
    refocus paths)."""
    roost = palette  # the `palette` fixture is the roost client with
    # open/closed hygiene around the test.
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    _wait_terminal_focused(roost, what="terminal focused after new tab")

    roost.palette_open(kind="commands")
    Roost._wait(
        lambda: not roost.app_active_terminal_focused(),
        timeout=2.0,
        what="terminal to lose focus while the palette is open",
    )

    roost.palette_dismiss()
    _wait_terminal_focused(roost, what="terminal refocused after palette dismiss")


def test_project_switch_focuses_terminal(roost):
    """Switching the active project lands keyboard focus on the new
    project's active terminal — the Swift `selectProject` ->
    `makeFirstResponder` parity.

    Forward sentinel for the IPC/keyboard switch path, NOT the F2
    regression: switching via `tab.focus` doesn't focus a sidebar row,
    so it never reproduced the strand bug (focus left on the clicked
    GtkListBoxRow, cursor hollow). The real-click F2 regression lives in
    tools/input/linux/click_to_focus_check.py."""
    a = b = None
    try:
        a = roost.create_project(name="focus-a", cwd="/tmp")
        b = roost.create_project(name="focus-b", cwd="/tmp")
        ta = roost.open_tab(a, cwd="/tmp")
        wait_tab_attached(roost, ta)
        tb = roost.open_tab(b, cwd="/tmp")
        wait_tab_attached(roost, tb)

        roost.focus(ta)
        assert roost.identify()["active_project_id"] == a
        _wait_terminal_focused(roost, what="terminal focused after switch to project A")

        roost.focus(tb)
        assert roost.identify()["active_project_id"] == b
        _wait_terminal_focused(roost, what="terminal focused after switch to project B")
    finally:
        for pid in (a, b):
            if pid is None:
                continue
            try:
                roost.delete_project(pid)
            except RoostError:
                pass  # already cascade-closed; real errors still propagate


def test_tab_switch_keeps_focus(roost, project):
    """Switching tabs within a project keeps focus on the terminal."""
    a = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, a)
    b = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, b)
    roost.focus(a)
    _wait_terminal_focused(roost, what="terminal focused after switching to tab A")
    roost.focus(b)
    _wait_terminal_focused(roost, what="terminal focused after switching to tab B")


def test_close_tab_focuses_survivor(roost, project):
    """Closing the active tab lands focus on the surviving tab's
    terminal (the AdwTabView auto-selects a neighbor)."""
    a = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, a)
    b = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, b)
    roost.focus(b)
    _wait_terminal_focused(roost, what="terminal focused on active tab before close")
    roost.close_tab(b)
    Roost._wait(lambda: roost.tab(b) is None, timeout=5.0, what="closed tab to drop")
    _wait_terminal_focused(roost, what="surviving tab's terminal focused after close")


def test_core_tracks_displayed_tab(roost):
    """The workspace core (`identify().active_tab_id`) and the on-screen
    AdwTabView selection (`app.selected_tab_id`) must agree through every
    programmatic path — open, focus, project switch, close-active-tab
    survivor. This pins the `selected-page` core-sync guard added for
    #228/#229: it must suppress the echo on our own programmatic selection
    changes (so no path desyncs core from UI) while still syncing genuine
    gestures (covered by the real-input harness). Uses 3 tabs so a
    survivor-policy mismatch (daemon HashMap order vs AdwTabView visual
    order) on close would surface."""
    a = b = None
    try:
        a = roost.create_project(name="coreui-a", cwd="/tmp")
        b = roost.create_project(name="coreui-b", cwd="/tmp")
        a1 = roost.open_tab(a, cwd="/tmp")
        wait_tab_attached(roost, a1)
        a2 = roost.open_tab(a, cwd="/tmp")
        wait_tab_attached(roost, a2)
        a3 = roost.open_tab(a, cwd="/tmp")
        wait_tab_attached(roost, a3)
        b1 = roost.open_tab(b, cwd="/tmp")
        wait_tab_attached(roost, b1)

        def assert_core_eq_ui(what: str) -> None:
            Roost._wait(
                lambda: roost.identify()["active_tab_id"] == roost.app_selected_tab_id(),
                timeout=4.0, what=f"core active == displayed tab after {what}")

        assert_core_eq_ui("opening tabs")

        roost.focus(a2)
        Roost._wait(lambda: roost.identify()["active_tab_id"] == a2,
                    timeout=4.0, what="a2 active")
        assert_core_eq_ui("focus a2")

        roost.focus(b1)
        Roost._wait(lambda: roost.identify()["active_project_id"] == b,
                    timeout=4.0, what="project b active")
        assert_core_eq_ui("switch to project b")

        # Close the active tab in A: the survivor must be tracked by BOTH
        # the core and the displayed selection, with no echo/divergence.
        roost.focus(a2)
        Roost._wait(lambda: roost.identify()["active_tab_id"] == a2,
                    timeout=4.0, what="a2 active again")
        roost.close_tab(a2)
        Roost._wait(lambda: roost.tab(a2) is None, timeout=5.0, what="a2 closed")
        assert_core_eq_ui("closing active tab a2")
        assert roost.identify()["active_tab_id"] in (a1, a3), \
            "survivor must be a real surviving tab in project A"
    finally:
        for pid in (a, b):
            if pid is None:
                continue
            try:
                roost.delete_project(pid)
            except RoostError:
                pass


def test_crossproject_focus_not_overwritten(roost):
    """Core-driven `tab.focus` to a tab in another project must leave the
    core active on that exact tab. Guards the re-entrancy hazard: the
    project-switch core-sync must live in the UI-action paths, NOT inside
    the `ActiveChanged` reaction (`set_active_project`) — otherwise the
    reaction would echo `focus_tab(project's *previously*-selected tab)`
    and clobber the active tab the caller actually asked for."""
    a = b = None
    try:
        a = roost.create_project(name="xover-a", cwd="/tmp")
        b = roost.create_project(name="xover-b", cwd="/tmp")
        a1 = roost.open_tab(a, cwd="/tmp")
        wait_tab_attached(roost, a1)
        a2 = roost.open_tab(a, cwd="/tmp")  # a2 becomes A's on-screen selected tab
        wait_tab_attached(roost, a2)
        b1 = roost.open_tab(b, cwd="/tmp")
        wait_tab_attached(roost, b1)

        roost.focus(b1)
        Roost._wait(lambda: roost.identify()["active_tab_id"] == b1,
                    timeout=4.0, what="project B active")
        # Focus a1 — a DIFFERENT tab than A's on-screen selected tab (a2).
        roost.focus(a1)
        Roost._wait(lambda: roost.identify()["active_tab_id"] == a1,
                    timeout=4.0, what="core active tab to reach a1")
        # The overwrite is in the ASYNC GTK ActiveChanged reaction that runs
        # after focus(a1) returns — it would echo focus_tab(a2) over a1. So
        # require the core to *stay* on a1 across the reaction window, not
        # just read a1 once (which the immediate focus_tab result satisfies
        # even with the bug).
        deadline = time.monotonic() + scaled_timeout(1.5)
        while time.monotonic() < deadline:
            assert roost.identify()["active_tab_id"] == a1, \
                "core active tab overwritten to A's previously-selected tab"
            time.sleep(0.1)
        assert roost.identify()["active_project_id"] == a
    finally:
        for pid in (a, b):
            if pid is None:
                continue
            try:
                roost.delete_project(pid)
            except RoostError:
                pass
