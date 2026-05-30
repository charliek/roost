"""End-to-end mouse-tracking + focus + OSC 22 pipeline tests.

Closes the cross-platform behavioral-parity gate for the four DEC
modes mouse-aware TUIs care about, plus OSC 22 cursor shape:

* **Mode 1000 / 1002** — button-event reporting. Strix uses these
  for `Down(Left)` / `Up(Left)` / `Drag(Left)` on its file pane and
  divider; htop / less / mc all variations of the same surface.
* **Mode 1003** — any-event motion. Strix uses this for hover
  detection on its split bar. Throttled to 60 Hz at the UI seam.
* **Mode 1004** — focus tracking. vim, less, btop and friends use
  `\\x1b[I` / `\\x1b[O` to redraw on focus state changes.
* **OSC 22** — pointer shape via W3C CSS cursor names. Strix sends
  `pointer` on divider hover and `default` to reset.

All tests use the `tab.feed_pty_bytes` (to enable a mode) +
`tab.dispatch_mouse_event` / `app.set_window_focus` /
`app.cursor_shape` IPC ops; capture via `tab.capture_pty_input`.

Cross-platform behavioral-parity gate: every case runs against
`--roost-target mac` (PR A wiring) and `--roost-target gtk`
(PR B wiring). A regression on either side fails the matching
job in CI.
"""

from __future__ import annotations

import os
import time

import pytest

from client import scaled_timeout
from util import drain, drain_until_match, wait_tab_attached


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"


# The whole module is gated on test mode. Both platforms run it now
# (PR B dropped the PR-A skip-on-gtk markers); it's the cross-
# platform behavioral-parity gate the plan called for.
pytestmark = [
    pytest.mark.skipif(
        not TEST_MODE,
        reason="mouse-tracking tests require ROOST_TEST_MODE=1 in the UI's launch env",
    ),
]


def test_button_press_release_emits_sgr_when_tracking_enabled(roost, project, target):
    """Mode 1000 + 1006 (button-event + SGR). A press LEFT at cell
    (5, 3) → `\\x1b[<0;6;4M`; release → `\\x1b[<0;6;4m`. SGR
    encoding uses 1-indexed cells; bit 0 of the button byte is left
    (0); the trailing `M` is press, `m` is release.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    # Enable 1000 (button-event) + 1006 (SGR). Clear the input drain
    # so a stray shell-prompt write doesn't pollute the assertion.
    roost.tab_feed_pty_bytes(tab, b"\x1b[?1000h\x1b[?1006h")
    drain(roost, tab)

    roost.tab_dispatch_mouse_event(
        tab, kind="press", button="left", cell_x=5, cell_y=3
    )
    captured = drain_until_match(roost, tab, rb"\x1b\[<0;6;4M", timeout=2.0)
    assert b"\x1b[<0;6;4M" in captured, captured

    roost.tab_dispatch_mouse_event(
        tab, kind="release", button="left", cell_x=5, cell_y=3
    )
    captured = drain_until_match(roost, tab, rb"\x1b\[<0;6;4m", timeout=2.0)
    assert b"\x1b[<0;6;4m" in captured, captured


def test_button_no_emit_when_tracking_off(roost, project, target):
    """No `\\x1b[?1000h` enable bytes → the encoder declines and the
    UI routes the press to the selection layer instead. Capture must
    be empty (no SGR sequence in the input channel)."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    drain(roost, tab)  # discard the shell's prompt bytes

    roost.tab_dispatch_mouse_event(
        tab, kind="press", button="left", cell_x=2, cell_y=2
    )
    # Allow a small settle window in case the UI ever buffers the
    # decision; capture must NOT contain any SGR mouse report.
    time.sleep(scaled_timeout(0.2))
    captured = drain(roost, tab)
    assert b"\x1b[<" not in captured, captured


def test_drag_emits_motion_with_button_in_mode_1002(roost, project, target):
    """Mode 1002 (button-event-drag) → a press → motion → release
    sequence produces three SGR reports: press(LEFT), motion with
    drag bit set, release. The drag bit is `+ 32` on the button
    byte in libghostty's SGR encoding."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    roost.tab_feed_pty_bytes(tab, b"\x1b[?1002h\x1b[?1006h")
    drain(roost, tab)

    roost.tab_dispatch_mouse_event(
        tab, kind="press", button="left", cell_x=5, cell_y=3
    )
    roost.tab_dispatch_mouse_event(
        tab, kind="motion", button="left", cell_x=7, cell_y=3
    )
    roost.tab_dispatch_mouse_event(
        tab, kind="release", button="left", cell_x=7, cell_y=3
    )
    captured = drain_until_match(
        roost, tab, rb"\x1b\[<0;6;4M.*\x1b\[<32;8;4M.*\x1b\[<0;8;4m", timeout=2.0
    )
    # Confirm the report shapes individually so a regression in any
    # one byte fails loudly with the offender named.
    assert b"\x1b[<0;6;4M" in captured, ("press missing", captured)
    assert b"\x1b[<32;8;4M" in captured, ("drag-motion missing", captured)
    assert b"\x1b[<0;8;4m" in captured, ("release missing", captured)


def test_motion_no_button_emits_only_in_mode_1003(roost, project, target):
    """Mode 1000 alone: motion-no-button is suppressed (no report).
    Mode 1003 enabled: same motion emits an SGR motion report with
    button=35 (no-button + motion bit 32, total 35)."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    # Mode 1000 only — no any-event motion gate. The UI's mouseMoved
    # gate short-circuits motion-no-button before reaching the
    # encoder.
    roost.tab_feed_pty_bytes(tab, b"\x1b[?1000h\x1b[?1006h")
    drain(roost, tab)
    roost.tab_dispatch_mouse_event(
        tab, kind="motion", button="none", cell_x=4, cell_y=4
    )
    time.sleep(scaled_timeout(0.2))
    captured_off = drain(roost, tab)
    assert b"\x1b[<" not in captured_off, captured_off

    # Now enable 1003. Same motion → SGR report.
    roost.tab_feed_pty_bytes(tab, b"\x1b[?1003h")
    drain(roost, tab)
    roost.tab_dispatch_mouse_event(
        tab, kind="motion", button="none", cell_x=4, cell_y=4
    )
    captured_on = drain_until_match(roost, tab, rb"\x1b\[<35;5;5M", timeout=2.0)
    assert b"\x1b[<35;5;5M" in captured_on, captured_on


def test_motion_throttle_dedups_same_cell(roost, project, target):
    """100 motion events at the same cell collapse to one report
    (per-cell dedup). The 60 Hz cap is covered by the Swift
    `MotionThrottleTests`; here we lock in that the
    `tab.dispatch_mouse_event` path actually hits the throttle."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    roost.tab_feed_pty_bytes(tab, b"\x1b[?1003h\x1b[?1006h")
    drain(roost, tab)
    for _ in range(100):
        roost.tab_dispatch_mouse_event(
            tab, kind="motion", button="none", cell_x=10, cell_y=5
        )
    captured = drain_until_match(roost, tab, rb"\x1b\[<35;11;6M", timeout=2.0)
    # Exactly one report — every subsequent same-cell motion is
    # suppressed by the throttle's `lastCell` check.
    reports = captured.count(b"\x1b[<35;11;6M")
    assert reports == 1, f"expected 1 throttled report, got {reports}: {captured!r}"


def test_focus_event_emitted_when_mode_1004_enabled(roost, project, target):
    """Mode 1004 on → focus-out emits `\\x1b[O`; focus-in emits
    `\\x1b[I`. The bytes are the canonical xterm focus sequences and
    the order matters (TUIs interpret O as "lost"; I as "gained")."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    roost.tab_feed_pty_bytes(tab, b"\x1b[?1004h")
    drain(roost, tab)

    roost.app_set_window_focus(focus=False)
    captured = drain_until_match(roost, tab, rb"\x1b\[O", timeout=2.0)
    assert b"\x1b[O" in captured, captured

    roost.app_set_window_focus(focus=True)
    captured = drain_until_match(roost, tab, rb"\x1b\[I", timeout=2.0)
    assert b"\x1b[I" in captured, captured


def test_focus_event_silent_when_mode_1004_disabled(roost, project, target):
    """Default mode (no 1004 enable) → focus toggles emit nothing.
    A regression that always emitted would dump junk into the user's
    shell prompt on every Cmd-Tab."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    drain(roost, tab)
    roost.app_set_window_focus(focus=False)
    roost.app_set_window_focus(focus=True)
    time.sleep(scaled_timeout(0.2))
    captured = drain(roost, tab)
    assert b"\x1b[O" not in captured, captured
    assert b"\x1b[I" not in captured, captured


def _wait_cursor_shape(roost, expected: str, timeout: float = 2.0) -> None:
    """Poll `app.cursor_shape` until it equals `expected` or the
    deadline expires. Each call gets its own scaled deadline so
    sequential phases inside one test don't share a budget."""
    deadline = time.monotonic() + scaled_timeout(timeout)
    while time.monotonic() < deadline:
        if roost.app_cursor_shape() == expected:
            return
        time.sleep(0.05)
    raise AssertionError(
        f"cursor never became {expected!r} (got {roost.app_cursor_shape()!r})"
    )


def test_osc_22_pointer_changes_cursor(roost, project, target):
    """Each W3C name produced by strix and friends round-trips
    through the OSC scanner → cursor mapper → `app.cursor_shape`.
    Empty body and unknown names both canonicalise to `"default"`."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)

    # `pointer` — the strix divider grab cursor.
    roost.tab_feed_pty_bytes(tab, b"\x1b]22;pointer\x1b\\")
    _wait_cursor_shape(roost, "pointer")

    # `default` — the strix reset form.
    roost.tab_feed_pty_bytes(tab, b"\x1b]22;default\x1b\\")
    _wait_cursor_shape(roost, "default")

    # `text` — BEL terminator. Same dispatch path as the ST form.
    roost.tab_feed_pty_bytes(tab, b"\x1b]22;text\x07")
    _wait_cursor_shape(roost, "text")

    # Empty reset form. Canonicalises to "default" on the wire so
    # tests can always assert against a non-empty name.
    roost.tab_feed_pty_bytes(tab, b"\x1b]22;\x1b\\")
    _wait_cursor_shape(roost, "default")


def test_right_click_emits_button_2_when_tracking_on(roost, project, target):
    """Mode 1000 + 1006: right-button press at (3, 3) → SGR
    `\\x1b[<2;4;4M`. Button code `2` is right-button in SGR
    encoding; the cells are 1-indexed."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    roost.tab_feed_pty_bytes(tab, b"\x1b[?1000h\x1b[?1006h")
    drain(roost, tab)
    roost.tab_dispatch_mouse_event(
        tab, kind="press", button="right", cell_x=3, cell_y=3
    )
    captured = drain_until_match(roost, tab, rb"\x1b\[<2;4;4M", timeout=2.0)
    assert b"\x1b[<2;4;4M" in captured, captured


def test_left_click_when_tracking_off_does_not_emit_sgr(roost, project, target):
    """Belt-and-braces of `test_button_no_emit_when_tracking_off`:
    confirm that with NO mode bytes the SGR encoder is genuinely
    silent. (Earlier test asserts absence; this one also exercises
    the URL precedence in the same gesture — a no-URL no-tracking
    click anchors a selection and stays silent.)"""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    drain(roost, tab)

    # Press + release with NO mode bytes fed. The capture must stay
    # SGR-free — the legacy selection path runs but it doesn't write
    # to the input channel.
    roost.tab_dispatch_mouse_event(
        tab, kind="press", button="left", cell_x=2, cell_y=2
    )
    roost.tab_dispatch_mouse_event(
        tab, kind="release", button="left", cell_x=2, cell_y=2
    )
    time.sleep(scaled_timeout(0.2))
    captured = drain(roost, tab)
    assert b"\x1b[<" not in captured, captured
