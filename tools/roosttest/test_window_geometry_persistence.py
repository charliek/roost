"""The window frame survives quit + relaunch on macOS (plan 074 §D5b, #595).

The macOS iced UI keeps its window's frame — the content size and the
outer top-left, in iced logical points — in its own `state.json`
(`SnapshotFile.window`). It writes it on a 500 ms trailing debounce after
a resize or move, and once more on quit before the final flush, and
creates the window at it on the next launch, so restored tabs spawn at
that grid. `crates/roost-iced/src/app/window_frame.rs` holds the rules
and their unit tests.

This module pins the size half end to end, three relaunches against the
same throwaway state dir: each round resizes, waits until `state.json`
carries the new size with the window still open (the debounce, not the
quit), quits, relaunches, and reads the size back through
`app.window_metrics`. The position and an off-screen saved frame need a
screen reader; the real-input module's window scenario covers them.

macOS iced only: Linux never remembers the frame (Ghostty's GTK build
doesn't, and Wayland can't place a window), and Roost.app answers
neither this file's ops nor its format. Skipped on CI like
`test_sidebar_collapse_persistence.py` — the slow macOS LaunchServices
respawn pushes a mid-test relaunch past `wait_alive`'s budget there — so
it is a required mac-mini step instead.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

import ui
from client import Roost
from util import skip_on_ci

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

#: Each round's content size in logical points: distinct from each other
#: and from the 1100×720 default, and small enough for a 1024×768 display
#: with its menu bar and title bar, so the post-open screen check never
#: has to shrink one.
ROUNDS = ((880.0, 560.0), (960.0, 600.0), (820.0, 520.0))

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="window.resize requires ROOST_TEST_MODE=1",
)


@pytest.fixture(autouse=True)
def _mac_iced_only(target):
    if sys.platform != "darwin" or target != "iced":
        pytest.skip("only the macOS iced UI remembers its window frame")
    skip_on_ci(
        "quit + relaunch is unreliable on CI: the slow macOS LaunchServices "
        "respawn pushes wait_alive past its 90s budget",
        alt_coverage="Rust app::window_frame tests + "
        "workspace window_frame_persists_across_reopen",
    )


def _size(roost: Roost) -> tuple[float, float]:
    metrics = roost.window_metrics()
    return (metrics["window_width"], metrics["window_height"])


def _saved_size(state_dir: Path) -> tuple[float, float] | None:
    """The content size `state.json` remembers, or None. The UI writes
    the file with tmp + rename, so a read never sees half of one."""
    try:
        state = json.loads((state_dir / "state.json").read_text())
    except FileNotFoundError:
        return None
    window = state.get("window")
    if window is None:
        return None
    return (window["content_width"], window["content_height"])


def _resize_and_wait_saved(
    roost: Roost, state_dir: Path, size: tuple[float, float]
) -> None:
    width, height = size
    roost.window_resize(width, height)
    Roost._wait(
        lambda: _size(roost) == size,
        timeout=5.0,
        what=f"the window to report {width}x{height}",
    )
    Roost._wait(
        lambda: _saved_size(state_dir) == size,
        timeout=5.0,
        what=f"state.json to remember {width}x{height} while the window is open",
    )


def _restore_and_wait_saved(roost: Roost, state_dir: Path, size: tuple[float, float]) -> None:
    """Put the window back and wait for `state.json` to hold the size it
    settles at. That can be smaller than `size`: the session's first window
    is never fitted, but a resize that lands before the relaunch's screen
    check is, and on a screen too small for it (the 1024×768 mini and CI
    runners) the fit shrinks it, as it should."""
    roost.window_resize(*size)
    Roost._wait(
        lambda: _saved_size(state_dir) == _size(roost),
        timeout=5.0,
        what="state.json to remember the size the restored window settled at",
    )


def test_window_size_survives_three_relaunches(roost, target):
    # Ownership gate, before anything is mutated: quit + relaunch against
    # a developer's own UI would close their session.
    state_dir = ui.session_state_dir()
    if state_dir is None or ui.owned_session_config_path() is None:
        pytest.skip(
            "quit + relaunch would close a developer's own UI; requires a "
            "harness-owned instance (--roost-fresh / ROOST_TEST_FRESH=1, or no "
            "UI already running)"
        )
    original = _size(roost)
    client: Roost | None = roost
    try:
        for size in ROUNDS:
            _resize_and_wait_saved(client, state_dir, size)
            client.close()
            client = None
            ui.quit(target)
            ui.launch(target)
            client = Roost(ui.socket_path(target))
            assert _size(client) == size, (
                f"the relaunched window must open at the {size} it was quit at, "
                f"got {_size(client)}"
            )
    finally:
        # A later relaunch in this session would otherwise inherit the
        # last round's size (plan 074 §9 R7).
        if client is not None:
            _restore_and_wait_saved(client, state_dir, original)
            if client is not roost:
                client.close()
