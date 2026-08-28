"""End-to-end OSC pipeline tests.

Closes both coverage gaps that #142 and #145 left open:

* **#142** (`bold-color`): the existing resolver tests pass
  `boldColor` explicitly into `resolve_cell_colors` /
  `resolveCellColors`. They don't exercise the production
  call sites (`crates/roost-iced/src/terminal_widget.rs::draw` /
  `mac/Sources/Roost/TerminalView.swift::draw`). A revert to
  `None`/`nil` would still pass the unit tests. The
  `tab.dump_resolved` IPC op walks the SAME `resolve_cell_colors`
  call with the live `theme.bold_color`, so asserting on its
  output pins the call site.
* **#145** (`OSC 10/11/12` dynamic replies): the color-query
  replies come from libghostty's `write_pty` effect, which each UI
  drains onto the tab's PTY input after `vt_write` (see
  `docs/reference/terminal-queries.md`). The unit tests cover the
  pieces; only this suite drives PTY bytes → the UI's drain → reply
  bytes on PTY stdin, on BOTH UIs. `tab.feed_pty_bytes` +
  `tab.capture_pty_input` make the full chain testable.

  Every color-query case here also asserts **exactly one** reply.
  Roost used to synthesize these replies from its own OSC scanner;
  once the pinned libghostty started answering them, that put two
  answers on the wire for every query. The counts are the regression
  guard.

Plus parity coverage for the other OSC-routed behaviors (title /
cwd / notification) that are currently unit-tested only.

Every UI target runs these in CI (e2e-mac and the iced lanes) with
`ROOST_TEST_MODE: "1"` set in the workflow env block.
"""

from __future__ import annotations

import os
import time

import pytest

from client import scaled_timeout
from util import drain, drain_until_match, wait_tab_quiet


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
        # Quiet, not just attached: `feed_pty_bytes` applies immediately
        # and does not serialize with PTY output still in flight, so a
        # seed sent at attach can land ahead of the prompt.
        wait_tab_quiet(roost, tab)
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
        wait_tab_quiet(roost, tab)
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
        """A mid-session `OSC 11;rgb:00/11/22` set must be reflected in
        the next `OSC 11;?` reply: libghostty answers from
        `override orelse default`, and the set moved the override. SET
        in one feed, QUERY in a second."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_quiet(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]11;rgb:00/11/22\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")
        # The 16-bit-per-channel form spells `0000/1111/2222`.
        captured = _drain_settled(roost, tab, rb"0000/1111/2222")
        _expect_one_reply(captured, b"\x1b]11;rgb:", b"0000/1111/2222")
        # The stale theme bg must NOT be in the reply — for roost-dark
        # that's `1e1e/1e1e/1e1e` (no escape characters needed; the
        # color string is sufficient).
        assert b"1e1e/1e1e/1e1e" not in captured, captured

    def test_osc10_set_then_query_replies_with_new_fg(self, roost, project):
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_quiet(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]10;rgb:aa/bb/cc\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]10;?\x07")
        captured = _drain_settled(roost, tab, rb"aaaa/bbbb/cccc")
        _expect_one_reply(captured, b"\x1b]10;rgb:", b"aaaa/bbbb/cccc")
        # Stale theme fg (roost-dark): `ffff/ffff/ffff`.
        assert b"ffff/ffff/ffff" not in captured, captured

    def test_osc12_set_then_query_replies_with_new_cursor(self, roost, project):
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_quiet(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]12;rgb:de/ad/be\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]12;?\x07")
        captured = _drain_settled(roost, tab, rb"dede/adad/bebe")
        _expect_one_reply(captured, b"\x1b]12;rgb:", b"dede/adad/bebe")
        # Stale theme cursor (the default cmux/roost cursor):
        # `9898/9898/9d9d`.
        assert b"9898/9898/9d9d" not in captured, captured

    # ----- OSC 4 palette-query coverage (opencode/opentui gate) ----------

    def test_osc4_query_replies_to_gate_probe(self, roost, project):
        """opencode/opentui gate ALL terminal color detection on a reply
        to `OSC 4;0;?` (a 300ms-timeout probe). Pre-fix roost ignored
        OSC 4, so the probe timed out and opencode fell back to an
        unreadable gray theme. We don't pin the exact color (palette[0]
        is theme-dependent) — only that exactly one well-formed OSC 4
        reply for index 0 comes back, which is what unblocks opencode."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_quiet(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]4;0;?\x07")
        captured = _drain_settled(
            roost, tab, rb"\x1b\]4;0;rgb:[0-9a-f]{4}/[0-9a-f]{4}/[0-9a-f]{4}"
        )
        assert captured.count(b"\x1b]4;0;rgb:") == 1, captured

    def test_osc4_set_then_query_replies_with_new_palette(self, roost, project):
        """OSC 4 analogue: a mid-session `OSC 4;5;rgb:de/ad/be` set must
        be reflected in the next `OSC 4;5;?` reply. SET in one feed,
        QUERY in a second."""
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_quiet(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]4;5;rgb:de/ad/be\x07")
        roost.tab_feed_pty_bytes(tab, b"\x1b]4;5;?\x07")
        captured = _drain_settled(roost, tab, rb"\x1b\]4;5;rgb:dede/adad/bebe")
        _expect_one_reply(captured, b"\x1b]4;5;rgb:", b"dede/adad/bebe")

    def test_osc11_same_chunk_set_query_replies_sequentially(self, roost, project):
        """SET + QUERY in ONE chunk answers with the JUST-SET color, and
        answers exactly once — pinned behavior on both UIs.

        This case has moved twice. It started as a skipped "known #145
        limitation" slot; plan 026's D10 moved iced's scan onto the PTY
        drain and pinned *pre-chunk* semantics (the SET was applied to
        a chunk-start snapshot, so a QUERY beside it saw the old color).
        The libghostty pin `f2d5758f6` made the terminal answer color
        queries itself, sequentially and in wire order — so Roost's own
        reply became a duplicate carrying a different (pre-chunk) color,
        and was removed. What the terminal reports is the just-set
        value, once.
        """
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_quiet(roost, tab)
        roost.tab_feed_pty_bytes(
            tab,
            b"\x1b]11;rgb:00/11/22\x07\x1b]11;?\x07",
        )
        captured = _drain_settled(roost, tab, rb"\x1b\]11;rgb:")
        _expect_one_reply(captured, b"\x1b]11;rgb:", b"0000/1111/2222")

    def test_osc11_query_reply_needs_no_dump_or_refresh(self, roost, project):
        """A color-query reply must not depend on a UI-side round-trip.

        Every other case here polls `tab.capture_pty_input` (via
        `drain_until_match`), which is already refresh-free — this test
        exists to say so out loud, and to fail if a future change makes
        the reply wait on a dump, a refresh, or any other round-trip
        beyond the `vt_write` + `write_pty` drain the UI already does
        per chunk. `prox`/termenv exits within a frame of its probe;
        anything slower leaks the answer into the shell prompt.
        """
        tab = roost.open_tab(project, cwd="/tmp")
        wait_tab_quiet(roost, tab)
        roost.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")
        captured = drain_until_match(
            roost, tab, rb"\x1b\]11;rgb:[0-9a-f]{4}/[0-9a-f]{4}/[0-9a-f]{4}"
        )
        assert b"\x1b]11;rgb:" in captured, captured

    # ----- parity coverage for OSC routing (title / cwd / notif) --------

    def test_osc7_cwd_updates_tab_metadata(self, roost, project):
        """OSC 7 (current working directory). The scanner parses
        `file:///path` → `/path` and the workspace records it as
        `tab.cwd`. Existing test_terminal.py covers this via a real
        shell `cd`; this test pins the wire path independently so a
        regression in the OSC dispatch surfaces without depending on
        shell integration."""
        tab = roost.open_tab(project, cwd="/tmp")
        # Quiet first: an integrated shell emits its own OSC 7 with the
        # real cwd at the prompt, and `feed_pty_bytes` doesn't serialize
        # behind it — seeding at attach can be overwritten a tick later.
        wait_tab_quiet(roost, tab)
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
        # Quiet first: the shell re-emits its title on every prompt, so a
        # seed racing the first prompt can be clobbered before we poll.
        wait_tab_quiet(roost, tab)
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
        notification path — same surface a Claude Code hook drives.

        The second tab steals active so the tab under test is a
        background one: notification policy B (plan 002 §3.5) drops a
        notification for the active tab of an active window, which would
        otherwise make this test depend on whether the runner's window
        happens to hold focus."""
        tab = roost.open_tab(project, cwd="/tmp")
        roost.open_tab(project, cwd="/tmp")  # steals active
        wait_tab_quiet(roost, tab)
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


def _drain_settled(roost, tab_id: int, pattern: bytes, settle: float = 0.5) -> bytes:
    """`drain_until_match`, then keep draining for `settle` seconds.

    A duplicate reply arrives *after* the one the match found — the two
    answers come from different points in the pipeline — so a bare
    `drain_until_match` would return before the second one landed and
    the count assertions would never see it.
    """
    captured = drain_until_match(roost, tab_id, pattern)
    deadline = time.monotonic() + scaled_timeout(settle)
    while time.monotonic() < deadline:
        captured += drain(roost, tab_id)
        time.sleep(0.05)
    return captured


def _expect_one_reply(captured: bytes, prefix: bytes, color: bytes) -> None:
    """Exactly one reply with `prefix`, and it carries `color`."""
    assert captured.count(prefix) == 1, (
        f"expected exactly one {prefix!r} reply, got {captured.count(prefix)}: "
        f"{captured!r}"
    )
    assert prefix + color in captured, captured


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
