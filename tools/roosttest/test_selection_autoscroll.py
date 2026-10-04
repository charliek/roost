"""Selection auto-scroll (#342), through `tab.dispatch_mouse_event`.

A selection drag held past the grid's top or bottom edge keeps scrolling
the viewport, and the selection keeps growing, while the pointer holds
still: the UI's own 50 ms tick does it, with no pointer events arriving.
The op's `overshoot` puts the synthetic pointer that many rows past the
grid, which is what the terminal widget reports for a real drag there,
so these cases run the model and the tick a real drag does. The real
pointer's half is `tools/input/linux/iced_clipboard_check.py` (X11) and
`iced_wayland_clipboard_check.py` (cage).

Assertions read the selection over `selection.dump`, never a clipboard,
so the module runs on every lane. History is seeded with
`tab.feed_pty_bytes`, never a shell, so its numbering is exact.

Skipped under `--roost-target mac`: the auto-scroll is iced's, and the
Swift app is frozen.
"""

from __future__ import annotations

import os
import warnings

import pytest

from client import Timeout
from test_tab_dump_scrollback import line, numbered
from util import wait_tab_attached, wait_tab_quiet

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="tab.dispatch_mouse_event and tab.feed_pty_bytes require ROOST_TEST_MODE=1",
)

# Many screens of history on any lane's window, and well inside the
# 2000 rows a tab keeps.
SEEDED_LINES = 600

# How far the downward case pages up before its drag: far enough that a
# screen's worth of auto-scroll, plus the ticks that land before the
# release does, stays clear of the live bottom.
PAGES_UP = 4

# How far past the grid the held pointer is: three rows a tick.
OVERSHOOT = 3

# How many gestures a case may make when the OS takes focus from the window
# mid-gesture (a macOS runner's activation churn does). A focus loss disarms
# the auto-scroll for good, and only a native loss earns another attempt, so
# a drag that stalls in a focused window still fails on its first.
FOCUS_ATTEMPTS = 3


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("the selection auto-scroll is iced's; the Swift app is frozen")


LAST_COL = len(line(0)) - 1


def _seed(roost, project) -> int:
    """A new tab on screen, in a focused window, at the live bottom of a
    numbered history: its last row sits just above the empty cursor row.

    The tick scrolls only the tab on screen in a focused window, and a
    headless lane's compositor may never have focused it.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    wait_tab_quiet(roost, tab)
    roost.focus(tab)
    roost._wait(lambda: roost.identify()["active_tab_id"] == tab, 10.0, f"tab {tab} on screen")
    roost.app_set_window_focus(focus=True)

    body = "\x1b[2J\x1b[H" + "".join(f"{line(i)}\r\n" for i in range(SEEDED_LINES))
    roost.tab_feed_pty_bytes(tab, body.encode())
    roost.wait_text(tab, line(SEEDED_LINES - 1), timeout=30.0)
    return tab


def _native_focus(roost) -> tuple[bool, int]:
    metrics = roost.window_metrics()
    return metrics["window_focused"], metrics["native_focus_losses"]


class _NativeFocusLost(Exception):
    """A held drag timed out after the OS took focus from the window."""


def _on_a_seeded_tab(roost, project, case) -> None:
    """Run `case` on a freshly seeded tab, and again on another one when
    its held drag lost to the OS taking focus, at most `FOCUS_ATTEMPTS`
    times in all."""
    for attempt in range(1, FOCUS_ATTEMPTS + 1):
        tab = _seed(roost, project)
        try:
            case(tab)
            return
        except _NativeFocusLost as lost:
            if attempt == FOCUS_ATTEMPTS:
                raise AssertionError(f"{lost}, on every one of {FOCUS_ATTEMPTS} attempts") from lost
            warnings.warn(f"attempt {attempt}: {lost}; retrying on a new tab", stacklevel=2)


def _hold(
    roost, tab: int, press: tuple[int, int], held: tuple[int, int], overshoot: int, reached, what: str
) -> None:
    """Press at `press`, hold `overshoot` rows past the grid from `held`
    until `reached`, then release there. A timeout with a native focus loss
    between the press and the timeout raises `_NativeFocusLost`. Any other
    timeout fails the case and names the focus state."""
    _, before = _native_focus(roost)
    top_at_press = roost.dump(tab)["rows_text"][0]
    _drag(roost, tab, "press", press)
    try:
        _drag(roost, tab, "motion", held, overshoot=overshoot)
        try:
            roost._wait(reached, 10.0, what)
        except Timeout as timeout:
            focused, after = _native_focus(roost)
            state = _hold_state(roost, tab, top_at_press)
            if after > before:
                raise _NativeFocusLost(
                    f"{timeout}: the OS took focus from the window {after - before}x during the hold"
                    f" ({state})"
                ) from timeout
            raise AssertionError(
                f"{timeout} (window_focused={focused}, no native focus loss during the hold; {state})"
            ) from timeout
    finally:
        _drag(roost, tab, "release", held, overshoot=overshoot)


def _hold_state(roost, tab: int, top_at_press: str) -> str:
    """Where a stalled hold got to, read raw so a partial row shows as is:
    whether the view scrolled, and the selection's ends, if it has any.
    Best effort: a failed read is reported, never raised over the timeout."""
    try:
        top = roost.dump(tab)["rows_text"][0]
        rows = (roost.selection_dump(tab).get("text") or "").split("\n")
    except Exception as error:
        return f"the hold's state could not be read: {error!r}"
    selection = f"selection {rows[0]!r}..{rows[-1]!r}" if rows != [""] else "no selection"
    return f"view top {top!r}, {top_at_press!r} at the press; {selection}"


def _selected_rows(roost, tab: int) -> list[int]:
    """The seeded index of every selected row, top to bottom."""
    text = roost.selection_dump(tab).get("text") or ""
    return [numbered(row) for row in text.split("\n")] if text else []


def _assert_one_run(rows: list[int], first: int, last: int) -> None:
    assert rows == list(range(first, last + 1)), (
        f"want row-{first:04d}..row-{last:04d} whole and in order, got {rows}"
    )


def _drag(roost, tab: int, kind: str, cell: tuple[int, int], overshoot: int = 0) -> None:
    if kind == "press":
        # A focus loss cancels the drag and the tick scrolls only a focused
        # window. On a CI runner the app's own activation churn (an earlier
        # module activating it) can unfocus the window after the fixture
        # focused it, so focus it again right before the gesture starts.
        roost.app_set_window_focus(focus=True)
    roost.tab_dispatch_mouse_event(
        tab, kind=kind, button="left", cell_x=cell[0], cell_y=cell[1], overshoot=overshoot
    )


def test_a_drag_held_above_the_grid_selects_into_history(roost, project):
    """Upward from the live bottom: the selection reaches a screen's
    worth of history above the viewport the drag started in, and
    releasing keeps exactly the run the scroll brought on screen."""
    _on_a_seeded_tab(roost, project, lambda tab: _held_above(roost, tab))


def _held_above(roost, tab: int) -> None:
    dumped = roost.dump(tab)
    rows = dumped["rows"]
    top = numbered(dumped["rows_text"][0])
    anchor_row = dumped["rows_text"].index(line(SEEDED_LINES - 1))

    _hold(
        roost,
        tab,
        (LAST_COL, anchor_row),
        (0, 0),
        -OVERSHOOT,
        lambda: (_selected_rows(roost, tab) or [top])[0] <= top - rows,
        f"the held drag to select a screen above row-{top:04d}",
    )

    selected = _selected_rows(roost, tab)
    assert selected, "the release dropped the selection"
    _assert_one_run(selected, selected[0], SEEDED_LINES - 1)
    assert selected[0] <= top - rows, selected[:3]
    assert numbered(roost.dump(tab)["rows_text"][0]) == selected[0], (
        "the selection starts on the row the scroll brought to the top"
    )


def test_a_drag_held_below_a_scrolled_up_grid_selects_toward_the_bottom(roost, project):
    """Downward from a viewport already paged up: the selection reaches a
    screen's worth below the viewport the drag started in."""
    _on_a_seeded_tab(roost, project, lambda tab: _held_below(roost, tab))


def _held_below(roost, tab: int) -> None:
    dumped = roost.dump(tab)
    rows = dumped["rows"]
    live_top = numbered(dumped["rows_text"][0])
    for _ in range(PAGES_UP):
        roost.key_event("PageUp")
    top = live_top - PAGES_UP * rows
    roost._wait(
        lambda: numbered(roost.dump(tab)["rows_text"][0]) == top,
        10.0,
        f"{PAGES_UP} pages up to row-{top:04d}",
    )
    bottom = top + rows - 1

    _hold(
        roost,
        tab,
        (0, 0),
        (LAST_COL, rows - 1),
        OVERSHOOT,
        lambda: (_selected_rows(roost, tab) or [bottom])[-1] >= bottom + rows,
        f"the held drag to select a screen below row-{bottom:04d}",
    )

    selected = _selected_rows(roost, tab)
    assert selected, "the release dropped the selection"
    _assert_one_run(selected, top, selected[-1])
    assert selected[-1] >= bottom + rows, selected[-3:]
    assert numbered(roost.dump(tab)["rows_text"][rows - 1]) == selected[-1], (
        "the selection ends on the row the scroll brought to the bottom"
    )
