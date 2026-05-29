"""End-to-end coverage for double-/triple-click word + line selection
(#161).

Drives the production word/line dispatch through the test-mode
`tab.expand_selection_at` IPC op on both UIs — same path the real
GestureClick (Linux) / `mouseDown` `clickCount` branch (Mac) hands a
click event to. We cover the golden cases the unit tests already pin
plus a few cross-port parity checks; the unit + fixture corpus is the
exhaustive layer.

Both targets run these in CI (e2e-gtk + e2e-mac) with
`ROOST_TEST_MODE: "1"` set in the workflow env block.
"""

from __future__ import annotations

import os
import time

import pytest

from client import RoostError, scaled_timeout


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"


def _wait_tab_attached(roost, tab_id: int, timeout: float = 5.0) -> None:
    """Poll `tab.dump` until the UI's TerminalView is live for
    `tab_id`. Same shape as `test_test_ops._wait_tab_attached`."""
    deadline = time.monotonic() + scaled_timeout(timeout)
    while True:
        try:
            roost.dump_text(tab_id)
            return
        except RoostError as e:
            if e.code != "not-found":
                raise
        if time.monotonic() >= deadline:
            raise TimeoutError(f"tab {tab_id} never attached a TerminalView")
        time.sleep(0.05)


def _seed_row(roost, tab_id: int, text: str, row: int = 10) -> None:
    """Feed `text` onto a viewport row clear of the shell's startup
    noise. Same prefix trick as `test_feed_pty_bytes_lands_on_terminal`
    so a slow shell on CI can't race with our marker."""
    payload = b"\x1b[2J\x1b[" + str(row + 1).encode() + b";1H" + text.encode("utf-8")
    roost.tab_feed_pty_bytes(tab_id, payload)
    roost.wait_text(tab_id, text, timeout=5.0)


@pytest.mark.skipif(
    not TEST_MODE,
    reason="tab.expand_selection_at requires ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestWordSelection:
    """Behavioural triple — word, line, path. Each runs against both
    targets via the `--roost-target` fixture parametrisation."""

    def test_double_click_selects_word(self, roost, project):
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        _seed_row(roost, tab, "hello world", row=10)
        # Col 2 ("l" in "hello"), n_press=2 → expand to "hello".
        result = roost.tab_expand_selection_at(tab, col=2, row=10, click_count=2)
        assert result["col0"] == 0
        assert result["col1"] == 4
        assert result["text"] == "hello"

    def test_triple_click_selects_line(self, roost, project):
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        _seed_row(roost, tab, "hello world", row=10)
        result = roost.tab_expand_selection_at(tab, col=0, row=10, click_count=3)
        assert result["col0"] == 0
        assert result["col1"] == 10
        assert result["text"] == "hello world"

    def test_double_click_path_selects_whole_path(self, roost, project):
        """Default `word-break-chars` keeps `/` and `.` inside the
        word so file paths select as one unit on double-click. This
        is the headline UX promise of the feature."""
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        _seed_row(roost, tab, "see /tmp/foo.txt here", row=10)
        # Col 8 is "p" inside "/tmp" — span should cover the whole
        # path, not just the "tmp" segment.
        result = roost.tab_expand_selection_at(tab, col=8, row=10, click_count=2)
        assert result["col0"] == 4
        assert result["col1"] == 15
        assert result["text"] == "/tmp/foo.txt"

    def test_double_click_whitespace_is_not_found(self, roost, project):
        """A whitespace double-click falls through to single-cell
        selection. The IPC op surfaces that as `not-found` (mirroring
        `tab.dump_resolved`'s 'no live span' arm)."""
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        _seed_row(roost, tab, "hello world", row=10)
        with pytest.raises(RoostError) as exc:
            roost.tab_expand_selection_at(tab, col=5, row=10, click_count=2)
        assert exc.value.code == "not-found"

    def test_click_count_below_two_rejected(self, roost, project):
        """The op refuses click_count < 2 with `invalid-param` so a
        caller can't accidentally drive single-click selection
        through this surface."""
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        with pytest.raises(RoostError) as exc:
            roost.tab_expand_selection_at(tab, col=0, row=10, click_count=1)
        assert exc.value.code == "invalid-param"
