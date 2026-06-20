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

from client import Roost
from util import wait_tab_attached


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
