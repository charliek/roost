"""256-color palette rendering tests (both UIs).

Regression: roost left the xterm 256-color cube (16-231) + grayscale
ramp (232-255) as a flat placeholder, so every `SGR 48;5;N` cell
rendered the same wrong color. opencode over SSH (256-color, because
COLORTERM is unset in a shed) backgrounds with `48;5;232` (#080808) and
rendered an unreadable gray. Feed known indices and assert the
production color resolver (`tab.dump_resolved`) returns the correct RGB.

Runs on both targets in CI (e2e-gtk + e2e-mac).
"""

from __future__ import annotations

import os
import time

import pytest

from client import scaled_timeout
from util import wait_tab_attached

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# 256-color index -> expected #RRGGBB (xterm/libghostty standard).
CASES = [
    (232, "#080808"),  # grayscale ramp start — opencode's background
    (255, "#eeeeee"),  # grayscale ramp end
    (240, "#585858"),  # grayscale ramp middle
    (16, "#000000"),   # cube black corner
    (231, "#ffffff"),  # cube white corner
    (21, "#0000ff"),   # cube pure blue
    (196, "#ff0000"),  # cube pure red
    (46, "#00ff00"),   # cube pure green
]
CHARS = "ABCDEFGH"


@pytest.mark.skipif(
    not TEST_MODE,
    reason="256-color palette test needs ROOST_TEST_MODE=1 (tab.feed_pty_bytes)",
)
class TestPalette256:
    def test_cube_and_grayscale_resolve_to_correct_rgb(self, roost, project):
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        # Clear + home to row 10, then one marker char per index, each
        # with that 256-color background.
        seq = b"\x1b[2J\x1b[10;1H"
        for (idx, _), ch in zip(CASES, CHARS):
            seq += b"\x1b[48;5;%dm%s\x1b[0m" % (idx, ch.encode())
        roost.tab_feed_pty_bytes(tab, seq)
        # Poll the resolved dump until every marker shows up (row 9 =
        # 0-based for the `\e[10;1H` 1-based home).
        deadline = time.monotonic() + scaled_timeout(5.0)
        got: dict[str, str] = {}
        while time.monotonic() < deadline:
            dump = roost.tab_dump_resolved(tab)
            got = {
                c["text"]: c["bg"]
                for c in dump["cells"]
                if c["row"] == 9 and c["text"] in CHARS
            }
            if len(got) >= len(CASES):
                break
            time.sleep(0.05)
        for (idx, exp), ch in zip(CASES, CHARS):
            assert got.get(ch) == exp, (
                f"256-color index {idx} ({ch}): expected {exp}, got {got.get(ch)!r} "
                f"(a flat placeholder here is the pre-fix bug)"
            )
