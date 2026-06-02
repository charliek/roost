"""End-to-end OSC pipeline tests.

Closes both coverage gaps that #142 and #145 left open:

* **#142** (`bold-color`): the existing resolver tests pass
  `boldColor` explicitly into `resolve_cell_colors` /
  `resolveCellColors`. They don't exercise the production
  call sites (`crates/roost-linux/src/terminal_view.rs::paint` /
  `mac/Sources/Roost/TerminalView.swift::draw`). A revert to
  `None`/`nil` would still pass the unit tests. The
  `tab.dump_resolved` IPC op walks the SAME `resolve_cell_colors`
  call with the live `theme.bold_color`, so asserting on its
  output pins the call site.
* **#145** (`OSC 10/11/12` dynamic replies): the existing tests
  cover `Terminal::live_colors`, `TerminalView.liveColor(forQuery:)`,
  and `format_color_query_response` in isolation. They don't
  drive PTY bytes → OscScanner.feed → ColorQuery event → reply
  written to PTY stdin. A regression in `app.rs`'s drain loop
  (Linux) or Mac's `appendBytes` event-loop would slip through.
  `tab.feed_pty_bytes` + `tab.capture_pty_input` (PR B) make the
  full chain testable.

Plus parity coverage for the other OSC-routed behaviors (title /
cwd / notification) that are currently unit-tested only.

Both targets run these in CI (e2e-gtk + e2e-mac) with
`ROOST_TEST_MODE: "1"` set in the workflow env block.
"""

from __future__ import annotations

import os
import time

import pytest

from client import scaled_timeout
from util import drain_until_match, wait_tab_attached


TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"


@pytest.mark.skipif(
    not TEST_MODE,
    reason="OSC pipeline tests require ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestOscPipeline:
    """The 8 cases tracked in the PR plan."""

    # ----- #142 call-site coverage --------------------------------------

    def test_bold_resolver_call_site_walks_style_bits(self, roost, project):
        """The production resolver call site (`paint` / `draw`) walks
        every cell through `resolve_cell_colors(&cell, default_fg,
        default_bg, theme.bold_color)`. `tab.dump_resolved` walks the
        SAME path, so asserting that a `\\e[1m`-marked cell surfaces
        `bold: true` (and the following non-bold cell surfaces
        `bold: false`) pins the call site reads `cell.style.bold`.

        Without the call site running through the resolver, the dump
        would still produce cells, but `bold` would be wrong (or
        absent). With it, the bold bit round-trips through libghostty
        + the resolver + the wire — exactly the chain #142 fixed.

        Note: The bundled `roost-dark` has `bold-color = foreground`,
        so the resolved fg is `#ffffff` in both arms — we can't
        differentiate bold-color vs default-fg by the color value
        alone. That's covered separately by the Mac/Rust unit tests
        in `ThemeBoldColorTests.swift` /
        `terminal_view.rs::tests::bold_default_fg_through_libghostty_uses_theme_bold_color`.
        Closing the call-site gap end-to-end via the bold bit is the
        strongest signal we can get from the bundled-theme set.
        """
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        # Clear + home + bold "B" + reset + non-bold "N", on a row
        # the shell startup won't touch.
        roost.tab_feed_pty_bytes(
            tab,
            b"\x1b[2J\x1b[10;1H\x1b[1mB\x1b[0mN",
        )
        # Settle: dump goes through the same render-state cycle the
        # production paint uses, so polling tab.dump_resolved until
        # the marker shows up doubles as the "libghostty has parsed
        # the input" wait.
        bold_cell, non_bold_cell = _find_bn_cells(roost, tab)
        assert bold_cell["text"] == "B", bold_cell
        assert non_bold_cell["text"] == "N", non_bold_cell
        assert bold_cell["bold"] is True, bold_cell
        assert non_bold_cell["bold"] is False, non_bold_cell

    def test_inverse_resolver_call_site_swaps_fg_bg(self, roost, project):
        """The resolver's `\\e[7m` (SGR inverse) branch swaps fg/bg
        and sets `has_explicit_bg: true`. Pinning this through the
        production call site proves the resolver actually ran — a
        regression that returned raw libghostty cell data without
        going through `resolve_cell_colors` would skip the swap and
        leave `has_explicit_bg: false`.
        """
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(
            tab,
            b"\x1b[2J\x1b[10;1H\x1b[7mX",
        )
        # Find the inverse cell. Poll because libghostty parses + the
        # walk is read on the next dump call.
        deadline = time.monotonic() + scaled_timeout(5.0)
        x_cell = None
        while time.monotonic() < deadline:
            dump = roost.tab_dump_resolved(tab)
            x_cell = next(
                (c for c in dump["cells"] if c["row"] == 9 and c["col"] == 0 and c["text"] == "X"),
                None,
            )
            if x_cell is not None:
                break
            time.sleep(0.05)
        assert x_cell is not None, "X cell never appeared in resolved dump"
        assert x_cell["inverse"] is True, x_cell
        assert x_cell["has_explicit_bg"] is True, x_cell
        # Discover the canvas defaults from a non-inverse cell on the
        # same row (e.g., the blank space at col 5) — instead of
        # hard-coding roost-dark's `#ffffff` / `#1e1e1e`, which would
        # silently rot if the harness ever ran against a different
        # default theme. Inverse must SWAP those two colors exactly:
        # asserting only fg != bg is too lax (any random swap would
        # pass); asserting on the literal post-swap values catches a
        # regression where, say, the resolver swapped to a third
        # color or only swapped fg.
        baseline = next(
            c for c in dump["cells"]
            if c["row"] == 9 and c["col"] == 5 and not c["inverse"]
        )
        canvas_fg = baseline["fg"]
        canvas_bg = baseline["bg"]
        assert canvas_fg != canvas_bg, (
            f"baseline fg == bg ({canvas_fg!r}) — can't validate inverse swap "
            f"on a single-color theme"
        )
        assert x_cell["fg"] == canvas_bg, (
            f"inverse fg ({x_cell['fg']!r}) must == canvas bg ({canvas_bg!r})"
        )
        assert x_cell["bg"] == canvas_fg, (
            f"inverse bg ({x_cell['bg']!r}) must == canvas fg ({canvas_fg!r})"
        )

    # ----- #145 drain-wiring coverage -----------------------------------

    def test_osc11_set_then_query_replies_with_new_bg(self, roost, project):
        """Pre-fix the OSC drain read the static theme bg, so a
        mid-session `OSC 11;rgb:00/11/22` set would NOT be reflected
        in the next `OSC 11;?` query reply. Post-fix (#145) it reads
        libghostty's live colors. SET in one feed, QUERY in a second
        — libghostty processes SET via vt_write before the QUERY's
        scanner.feed runs, so the reply uses the post-set bg."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]11;rgb:00/11/22\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")
        # The 16-bit-per-channel form spells `0000/1111/2222`.
        captured = drain_until_match(roost, tab, rb"0000/1111/2222")
        # The stale theme bg must NOT be in the reply — for roost-dark
        # that's `1e1e/1e1e/1e1e` (no escape characters needed; the
        # color string is sufficient).
        assert b"1e1e/1e1e/1e1e" not in captured, captured

    def test_osc10_set_then_query_replies_with_new_fg(self, roost, project):
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]10;rgb:aa/bb/cc\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]10;?\x07")
        captured = drain_until_match(roost, tab, rb"aaaa/bbbb/cccc")
        # Stale theme fg (roost-dark): `ffff/ffff/ffff`.
        assert b"ffff/ffff/ffff" not in captured, captured

    def test_osc12_set_then_query_replies_with_new_cursor(self, roost, project):
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]12;rgb:de/ad/be\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]12;?\x07")
        captured = drain_until_match(roost, tab, rb"dede/adad/bebe")
        # Stale theme cursor (the default cmux/roost cursor):
        # `9898/9898/9d9d`.
        assert b"9898/9898/9d9d" not in captured, captured

    # ----- OSC 4 palette-query coverage (opencode/opentui gate) ----------

    def test_osc4_query_replies_to_gate_probe(self, roost, project):
        """opencode/opentui gate ALL terminal color detection on a reply
        to `OSC 4;0;?` (a 300ms-timeout probe). Pre-fix roost ignored
        OSC 4, so the probe timed out and opencode fell back to an
        unreadable gray theme. Post-fix the drain answers each index from
        the live palette. We don't pin the exact color (palette[0] is
        theme-dependent) — only that a well-formed OSC 4 reply for
        index 0 comes back, which is what unblocks opencode."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]4;0;?\x07")
        captured = drain_until_match(
            roost, tab, rb"\x1b\]4;0;rgb:[0-9a-f]{4}/[0-9a-f]{4}/[0-9a-f]{4}"
        )
        assert b"\x1b]4;0;rgb:" in captured, captured

    def test_osc4_set_then_query_replies_with_new_palette(self, roost, project):
        """OSC 4 analogue of #145: a mid-session `OSC 4;5;rgb:de/ad/be`
        set must be reflected in the next `OSC 4;5;?` reply, read from
        libghostty's live palette. SET in one feed, QUERY in a second so
        libghostty's vt_write applies the set before the query's
        scanner.feed runs (the same ordering the OSC 11 test relies on)."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]4;5;rgb:de/ad/be\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]4;5;?\x07")
        captured = drain_until_match(roost, tab, rb"\x1b\]4;5;rgb:dede/adad/bebe")
        assert b"\x1b]4;5;rgb:dede/adad/bebe" in captured, captured

    @pytest.mark.skip(
        reason=(
            "known #145 limitation: vt_write happens AFTER scanner.feed in the "
            "drain, so SET in the same chunk as QUERY isn't visible to the "
            "color-query reply yet. PR slot for the eventual fix — when the "
            "drain reorders, removing this skip makes the assertion pass."
        )
    )
    def test_osc11_same_chunk_set_query_known_stale(self, roost, project):
        """Regression slot: feed SET + QUERY as one chunk. The reply
        currently encodes the STALE theme bg because the OSC scanner
        runs before libghostty's vt_write. Documented in #145's PR
        body as out-of-scope; this test stays here so a future
        reordering surfaces by unskipping."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(
            tab,
            b"\x1b]11;rgb:00/11/22\x07\x1b]11;?\x07",
        )
        captured = drain_until_match(roost, tab, rb"\x1b\]11;rgb:")
        # When this is FIXED, the assertion should flip to check
        # that the post-set color (0000/1111/2222) appears AND
        # the stale theme color (1e1e/1e1e/1e1e) does not.
        assert b"0000/1111/2222" in captured, captured
        assert b"1e1e/1e1e/1e1e" not in captured, captured

    # ----- parity coverage for OSC routing (title / cwd / notif) --------

    def test_osc7_cwd_updates_tab_metadata(self, roost, project):
        """OSC 7 (current working directory). The scanner parses
        `file:///path` → `/path` and the workspace records it as
        `tab.cwd`. Existing test_terminal.py covers this via a real
        shell `cd`; this test pins the wire path independently so a
        regression in the OSC dispatch surfaces without depending on
        shell integration."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]7;file:///usr\x07")
        # The dispatch fires asynchronously on the UI loop; poll
        # tab.list until cwd reflects.
        deadline = time.monotonic() + scaled_timeout(5.0)
        while time.monotonic() < deadline:
            if (roost.tab(tab) or {}).get("cwd") == "/usr":
                return
            time.sleep(0.05)
        raise AssertionError(
            f"tab cwd never updated to /usr after OSC 7 feed "
            f"(got {(roost.tab(tab) or {}).get('cwd')!r})"
        )

    def test_osc0_title_routes_to_tab(self, roost, project):
        """OSC 0 (icon name + window title) updates the tab's title
        until the user explicitly renames (then `user_titled=true`
        locks it). Pins the OSC dispatch end-to-end."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        marker = "roost-osc0-title-test"
        roost.tab_feed_pty_bytes(tab, b"\x1b]0;" + marker.encode("ascii") + b"\x07")
        deadline = time.monotonic() + scaled_timeout(5.0)
        while time.monotonic() < deadline:
            title = (roost.tab(tab) or {}).get("title", "")
            if marker in title:
                return
            time.sleep(0.05)
        raise AssertionError(
            f"tab title never picked up OSC 0 marker (last={title!r})"
        )

    def test_osc9_notification_lands_on_tab(self, roost, project):
        """OSC 9 (iTerm2 notification, title-only) flips
        `tab.has_notification = true` via the workspace's
        notification path — same surface a Claude Code hook drives."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_attached(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]9;build complete\x07")
        deadline = time.monotonic() + scaled_timeout(5.0)
        while time.monotonic() < deadline:
            if (roost.tab(tab) or {}).get("has_notification") is True:
                return
            time.sleep(0.05)
        raise AssertionError(
            "tab.has_notification never flipped to True after OSC 9 feed"
        )


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _find_bn_cells(roost, tab_id: int, timeout: float = 5.0):
    """Poll `tab.dump_resolved` until both the bold 'B' cell at row
    9 col 0 AND the non-bold 'N' cell at row 9 col 1 are present.
    Returns the two cells (bold first). Raises AssertionError on
    timeout. Used by `test_bold_resolver_call_site_walks_style_bits`.
    """
    deadline = time.monotonic() + scaled_timeout(timeout)
    last = None
    while time.monotonic() < deadline:
        dump = roost.tab_dump_resolved(tab_id)
        last = dump
        cells_by_pos = {(c["row"], c["col"]): c for c in dump["cells"]}
        bold = cells_by_pos.get((9, 0))
        non_bold = cells_by_pos.get((9, 1))
        if (
            bold is not None
            and non_bold is not None
            and bold.get("text") == "B"
            and non_bold.get("text") == "N"
        ):
            return bold, non_bold
        time.sleep(0.05)
    raise AssertionError(
        f"B/N cells never appeared at row 9 col 0/1 (last dump cells head={(last or {}).get('cells', [])[:5]})"
    )
