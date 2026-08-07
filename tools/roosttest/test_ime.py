"""IME e2e — plan 021 (iced adapter's `tab.feed_ime` test-mode op).

Covers the production IME path (`ime_preedit` / `ime_commit` /
`ime_session_boundary` in `crates/roost-iced/src/app.rs`) end-to-end,
against the same `tab.feed_ime` IPC op a real input method drives
through winit. Every case runs against a single active tab — no test
here needs more than one.

* **Byte-exactness (commit encoding):** `encode_ime_commit`
  (`crates/roost-iced/src/input.rs`) drives libghostty's key encoder
  with an UNIDENTIFIED key + the commit text as a raw UTF-8 payload.
  The unit test `ime_commits_encode_their_exact_utf8` pins that this
  returns exactly `text.as_bytes()` — no CSI-u / kitty-protocol
  wrapping — so the end-to-end PTY capture is asserted byte-for-byte
  equal here, not merely containing the payload.
* **Preedit never reaches the PTY:** `set_preedit` only mutates
  `TerminalTab::preedit` (the overlay the renderer draws at the
  cursor); nothing in that path touches the session's input channel.
  Only `commit_ime` calls `encode_ime_commit` + `session.send_input`.
* **The one-shot discard latch (`ImeDiscard`):** a route change (e.g.
  opening the palette) cancels any live preedit — `open_palette` calls
  `cancel_ime_composition()`, which arms the latch whenever it actually
  cleared a composition. The OS may still re-offer that composition's
  commit after focus returns to the terminal; the latch drops exactly
  that one commit and is then consumed (a fresh preedit disarms it
  first via the non-empty-text check in `set_preedit_in`).

Iced-only: `tab.feed_ime` raises `RoostError('not-implemented')` on the
GTK target (see `client.py`'s docstring); Mac has no IME test-mode op.
"""

from __future__ import annotations

import os
import re
import time
from pathlib import Path

import pytest

from client import RoostError, Roost, scaled_timeout
from util import BARE_SHELL_ARGV, drain, drain_until_match, wait_tab_quiet

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# "é" precomposed (U+00E9) vs "e" + combining acute (U+0065 U+0301) —
# same rendered glyph, different UTF-8 bytes; both must round-trip
# byte-exact. Mirrors `ime_commits_encode_their_exact_utf8`'s case set.
IME_COMMIT_CASES = ["é", "é", "你好", "👍"]


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("tab.feed_ime is iced-only for now (plan 021)")


def _ime_tab(roost, project) -> int:
    tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_quiet(roost, tab)
    return tab


def _capture_png(roost, path: Path) -> bytes | None:
    """One `app.screenshot` (scale 1) capture as raw PNG bytes, or None
    while the window is transiently mid-relayout — the same
    empty-snapshot transient `test_sidebar_pixels._capture` polls
    through."""
    try:
        png, _w, _h = roost.screenshot()
    except RoostError as e:
        if e.code == "internal" and "empty snapshot" in e.message:
            return None
        raise
    path.write_bytes(png)
    return png


def _settled_png(roost, path: Path) -> bytes:
    result: dict[str, bytes] = {}

    def captured() -> bool:
        shot = _capture_png(roost, path)
        if shot is None:
            return False
        result["png"] = shot
        return True

    Roost._wait(captured, 10.0, f"screenshot captured ({path.name})")
    return result["png"]


def _after_redraw(roost, op) -> None:
    """Run `op`, then wait for a fresh paint (the `app.render_stats`
    draw-call counter ticking) before the caller screenshots — mirrors
    `test_sprite_pixels`'s D4 wait, so the capture reflects the op's
    state change and not a stale cached frame. `feed_ime` (and
    `palette.dismiss`) are synchronous over IPC — the state mutation
    has already landed by the time `op()` returns — so this only waits
    for the NEXT paint to pick it up."""
    roost.call("app.render_stats", {"reset": True})
    op()
    Roost._wait(
        lambda: int(roost.call("app.render_stats", {})["draw_calls"]) > 0,
        5.0,
        "redraw after IME op",
    )


@pytest.mark.skipif(
    not TEST_MODE,
    reason="IME e2e requires ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestIme:
    def test_ime_commit_reaches_pty_byte_exact(self, roost, project):
        tab = _ime_tab(roost, project)
        for text in IME_COMMIT_CASES:
            drain(roost, tab)
            roost.tab_feed_ime(tab, "commit", text)
            want = text.encode("utf-8")
            captured = drain_until_match(roost, tab, re.escape(want))
            assert captured == want, (text, captured)

    def test_ime_preedit_renders_and_clears_at_the_cursor(self, roost, project, tmp_path):
        tab = _ime_tab(roost, project)
        artifact_dir = Path(os.environ.get("ROOST_E2E_ARTIFACT_DIR", tmp_path))
        artifact_dir.mkdir(parents=True, exist_ok=True)

        baseline = _settled_png(roost, artifact_dir / "ime-baseline.png")

        _after_redraw(
            roost, lambda: roost.tab_feed_ime(tab, "preedit", "你好", cursor=(0, 6))
        )
        during = _settled_png(roost, artifact_dir / "ime-preedit.png")
        assert during != baseline, "preedit text never reached the screen"

        _after_redraw(roost, lambda: roost.tab_feed_ime(tab, "clear"))
        cleared = _settled_png(roost, artifact_dir / "ime-cleared.png")
        assert cleared == baseline, "preedit overlay left a residue after clear"

    def test_ime_preedit_then_commit_emits_once(self, roost, project):
        tab = _ime_tab(roost, project)
        drain(roost, tab)
        roost.tab_feed_ime(tab, "preedit", "ni", cursor=(0, 2))
        roost.tab_feed_ime(tab, "preedit", "你", cursor=(0, 3))
        roost.tab_feed_ime(tab, "commit", "你")
        captured = drain_until_match(roost, tab, re.escape("你".encode()))
        assert captured.count("你".encode()) == 1, captured
        assert b"ni" not in captured, captured

    def test_ime_clear_on_route_change(self, roost, project, tmp_path):
        tab = _ime_tab(roost, project)
        artifact_dir = Path(os.environ.get("ROOST_E2E_ARTIFACT_DIR", tmp_path))
        artifact_dir.mkdir(parents=True, exist_ok=True)

        baseline = _settled_png(roost, artifact_dir / "ime-route-baseline.png")

        _after_redraw(
            roost, lambda: roost.tab_feed_ime(tab, "preedit", "你", cursor=(0, 3))
        )
        during = _settled_png(roost, artifact_dir / "ime-route-preedit.png")
        assert during != baseline, "preedit text never reached the screen"

        roost.palette_open()
        # While the palette owns the keyboard route, ANY feed_ime call
        # is rejected — the route is no longer `Terminal(tab)` (see
        # `servicing.rs`'s `TabFeedIme` arm, mapped by
        # `map_test_op_err`'s "is not the active terminal" branch).
        with pytest.raises(RoostError) as ei:
            roost.tab_feed_ime(tab, "commit", "你")
        assert ei.value.code == "invalid-param", ei.value

        _after_redraw(roost, roost.palette_dismiss)
        cleared = _settled_png(roost, artifact_dir / "ime-route-cleared.png")
        assert cleared == baseline, "opening the palette must cancel the live preedit"

        # The discard latch: `open_palette` cancelled the live preedit
        # above (arming the one-shot latch), and nothing has disarmed
        # it since — only a fresh, non-empty preedit does. The route is
        # `Terminal(tab)` again post-dismiss, so this commit call
        # succeeds (no RoostError) — but its bytes are dropped.
        drain(roost, tab)
        roost.tab_feed_ime(tab, "commit", "你")
        dropped = drain(roost, tab)
        assert dropped == b"", dropped

        # A fresh preedit disarms the latch; the following commit lands.
        roost.tab_feed_ime(tab, "preedit", "你", cursor=(0, 3))
        roost.tab_feed_ime(tab, "commit", "你")
        captured = drain_until_match(roost, tab, re.escape("你".encode()))
        assert captured == "你".encode(), captured

    def test_ime_commit_empty_text_is_a_no_op(self, roost, project):
        tab = _ime_tab(roost, project)
        drain(roost, tab)
        roost.tab_feed_ime(tab, "commit", "")
        # No IPC signal for "nothing happened" — a bounded settle
        # window, same shape as `test_button_no_emit_when_tracking_off`.
        time.sleep(scaled_timeout(0.2))
        assert drain(roost, tab) == b""
