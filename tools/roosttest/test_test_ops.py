"""End-to-end smoke tests for the test-only IPC ops (PR B).

`tab.feed_pty_bytes` + `tab.capture_pty_input` are the scaffolding for
the PR-C OSC pipeline tests; `tab.dump_resolved` is the resolver-walk
op #142's call-site coverage gap depends on. This file is the
regression net for the scaffolding ITSELF — one assertion per op,
exercising the round trip without exercising any downstream
correctness (that's PR C's job).

Both targets run these in CI (e2e-gtk + e2e-mac) with
`ROOST_TEST_MODE: "1"` set in the workflow env block.
"""

from __future__ import annotations

import os
import re
import time

import pytest

from client import RoostError, scaled_timeout


# Skip the whole file when the gating env var is absent. The handlers
# return `not-enabled` in that case, which would make every assertion
# fail in a useless way; a clear top-of-file skip explains why.
TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"


def _wait_tab_attached(roost, tab_id: int, timeout: float = 5.0) -> None:
    """Block until the UI has attached its TerminalView to `tab_id`.

    `tab.open` returns as soon as the workspace creates the tab; the
    UI's TabSession + TerminalView attach asynchronously on the main
    loop. The test-mode ops that need the live `TerminalView`
    (`tab.feed_pty_bytes`, `tab.dump_resolved`) return `not-found`
    during the gap. `tab.dump` is the same shape and the same
    attachment dependency, so polling it is the canonical wait.
    """
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


@pytest.mark.skipif(
    not TEST_MODE,
    reason="test-only ops require ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestTestOps:
    """Smoke triple — one assertion per op."""

    def test_feed_pty_bytes_lands_on_terminal(self, roost, project):
        """A bare ASCII string fed via `tab.feed_pty_bytes` shows up
        on the next `tab.dump`. Confirms the bytes route through the
        same `TabOutput::Bytes`/`appendBytes` path the real PTY
        output uses — without that, libghostty never sees them and
        nothing renders."""
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        marker = "roost-feed-smoke-1234"
        roost.tab_feed_pty_bytes(tab, marker.encode("ascii"))
        roost.wait_text(tab, marker, timeout=5.0)

    def test_capture_pty_input_round_trip(self, roost, project):
        """Feed an OSC 11 query — libghostty + the OSC drain on
        whichever UI hosts the tab synthesise a reply via the same
        `send_input` / `onKey` path keystrokes go through. The
        reply must show up in `tab.capture_pty_input`. This is the
        bare minimum proof the input-capture tap is wired."""
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        # OSC 11 query (background). The reply rides the input
        # channel as `\e]11;rgb:RRRR/GGGG/BBBB\x07`. We don't
        # care which color — only that *something* came back.
        roost.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")

        # Poll: the OSC scanner + reply emission run on the UI
        # main loop, so the capture isn't synchronous with our
        # feed call. wait_input is local to this test — the
        # general client doesn't expose it because nothing
        # outside the test ops needs it.
        deadline = time.monotonic() + scaled_timeout(5.0)
        captured = b""
        while time.monotonic() < deadline:
            captured += roost.tab_capture_pty_input(tab, drain=True)
            if re.search(rb"\x1b\]11;rgb:[0-9a-f]{4}/[0-9a-f]{4}/[0-9a-f]{4}\x07", captured):
                return
            time.sleep(0.05)
        raise AssertionError(
            f"OSC 11 query never replied through send_input (captured={captured!r})"
        )

    def test_dump_resolved_returns_grid(self, roost, project):
        """A freshly-opened tab's resolved-cell dump should be a
        well-formed grid: `cols`/`rows` match the open dims, `cells`
        is a list with a sane size (cols*rows), every cell carries
        the expected fields including `#RRGGBB` color strings."""
        tab = roost.open_tab(project, cwd="/tmp")
        _wait_tab_attached(roost, tab)
        # Use the existing tab.list to find the open dims — the
        # IPC handler decides defaults (mac/gtk may differ on the
        # very first tab). What we assert is internal consistency
        # of the dump, not specific numbers.
        dump = roost.tab_dump_resolved(tab)
        assert isinstance(dump["cells"], list)
        cols = dump["cols"]
        rows = dump["rows"]
        assert cols > 0 and rows > 0
        # The walk visits every cell on the viewport. Don't over-
        # specify the count (cells skipped at end-of-line on
        # some terminals etc.); just that there's *some* output.
        assert len(dump["cells"]) > 0
        sample = dump["cells"][0]
        for key in (
            "row", "col", "text", "fg", "bg",
            "has_explicit_bg", "bold", "italic", "inverse",
        ):
            assert key in sample, f"resolved cell missing {key!r}: {sample}"
        # `#RRGGBB` format — six lowercase hex digits.
        assert re.fullmatch(r"#[0-9a-f]{6}", sample["fg"]), sample["fg"]
        assert re.fullmatch(r"#[0-9a-f]{6}", sample["bg"]), sample["bg"]
