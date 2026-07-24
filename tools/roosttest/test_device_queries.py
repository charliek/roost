"""End-to-end device-query reply tests (#247).

Before #247 roost never installed libghostty-vt's `write_pty` effects
callback, so every device query the engine would answer itself was
silently dropped (`stream_terminal.zig` no-ops on a null callback). Most
user-visible: the Kitty keyboard progressive-enhancement query
`ESC[?u` — crossterm's `supports_keyboard_enhancement()` blocked ~2 s
then concluded "unsupported", so Shift+Enter arrived as a bare `\r` in
query-first TUIs (strix's comment editor).

These tests drive the full production chain on a live UI: a query fed via
`tab.feed_pty_bytes` (the real PTY-output path → `vt_write`) makes the
engine emit its reply, which the write_pty drain routes onto the input
side — observable via `tab.capture_pty_input`. One case per query in the
engine-autonomous reply set (§3.5). Provider-gated queries (ENQ, XTWINOPS
14/16/18t size reports, DSR ?996) are NOT answered by `write_pty` alone
and are out of scope here (deferred to #209).

No keystroke-interleaving case: `roost.send`/`tab.write` bypasses
`send_input` on Linux, so IPC-sent keys never appear in
`capture_pty_input` — ordering is proven compositionally in the roost-vt
+ Mac unit tests instead (plan §3.5).

Both targets run these in CI (e2e-gtk + e2e-mac) with
`ROOST_TEST_MODE: "1"` set in the workflow env block.
"""

from __future__ import annotations

import os

import pytest

from util import drain_until_match, wait_tab_attached


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"


@pytest.mark.skipif(
    not TEST_MODE,
    reason="device-query tests require ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestDeviceQueries:
    """The engine-autonomous reply set, end to end."""

    def test_da1_reports_device_attributes(self, roost, project):
        """DA1 (`ESC[c`) — the sync fence crossterm blocks on. The
        engine's default attribute set at the pinned SHA is
        `ESC[?62;22c` (VT220 + ANSI color)."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b[c")
        captured = drain_until_match(roost, tab, rb"\x1b\[\?62;22c")
        assert b"\x1b[?62;22c" in captured, captured

    def test_dsr5n_reports_ok_status(self, roost, project):
        """DSR 5n (operating status) → `ESC[0n` ("OK")."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b[5n")
        captured = drain_until_match(roost, tab, rb"\x1b\[0n")
        assert b"\x1b[0n" in captured, captured

    def test_dsr6n_reports_cursor_position(self, roost, project):
        """DSR 6n (cursor-position report) → `ESC[<row>;<col>R`. The
        exact position depends on where the shell prompt left the
        cursor, so match the shape, not a literal."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b[6n")
        captured = drain_until_match(roost, tab, rb"\x1b\[[0-9]+;[0-9]+R")
        assert captured

    def test_decrqm_reports_cursor_visibility_mode(self, roost, project):
        """DECRQM for mode 25 (DECTCEM cursor visibility) →
        `ESC[?25;<state>$y`, state 1 (set) or 2 (reset) depending on
        whether the shell has hidden the cursor."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b[?25$p")
        captured = drain_until_match(roost, tab, rb"\x1b\[\?25;[12]\$y")
        assert captured

    def test_xtversion_reports_libghostty(self, roost, project):
        """XTVERSION (`ESC[>q`) → the default `DCS >|libghostty ST`
        (roost ships no override; advertising policy is #209)."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b[>q")
        captured = drain_until_match(roost, tab, rb"libghostty")
        assert b"libghostty" in captured, captured

    def test_kitty_keyboard_query_reports_flags(self, roost, project):
        """Kitty keyboard progressive-enhancement query (`ESC[?u`) —
        the #247 headliner. A bare shell has pushed no flags →
        `ESC[?0u`. This is the reply crossterm waits on before it
        pushes Kitty flags (fixing Shift+Enter in strix)."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b[?u")
        captured = drain_until_match(roost, tab, rb"\x1b\[\?0u")
        assert b"\x1b[?0u" in captured, captured

    def test_mode2031_theme_switch_emits_color_scheme_report(self, roost, project):
        """DEC 2031 proactive notification (C3): once a tab enables mode
        2031 (`CSI ? 2031 h`), a runtime theme switch must emit
        `CSI ? 997 ; Ps n` onto its PTY input (Ps=1 dark, 2 light) so
        2031-aware TUIs re-theme live. All bundled themes are dark, so any
        switch reports dark (`ESC[?997;1n`).

        Drives the real theme-switch path: filtering the themes sub-frame
        moves the live-preview highlight to a different theme, which runs
        the same `set_theme`/`setTheme` broadcast a user gets while
        arrowing. Preview (not commit) is used deliberately — commit
        persists to `ROOST_CONFIG`, and dismiss reverts cleanly — so the
        seed config fixture is left untouched."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        # Opt the tab's terminal into color-scheme reporting.
        roost.tab_feed_pty_bytes(tab, b"\x1b[?2031h")

        # Drill into the themes frame, then filter to a theme other than
        # the active one (its row is the current selection). Setting the
        # query moves the highlight to the top match → live preview fires
        # `set_theme` on every view, emitting the report for our 2031 tab.
        roost.palette_open("commands")
        themes = roost.palette_activate("select_theme")
        items = themes.get("items", [])
        assert items, f"no themes in palette: {themes}"
        selection = themes.get("selection", 0)
        target_title = items[(selection + 1) % len(items)]["title"]
        try:
            roost.palette_query(target_title)
            captured = drain_until_match(roost, tab, rb"\x1b\[\?997;1n")
            assert b"\x1b[?997;1n" in captured, captured
        finally:
            # Revert the preview without persisting (Esc-equivalent).
            roost.palette_dismiss()
