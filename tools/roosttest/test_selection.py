"""End-to-end tests for the `selection.*` IPC ops.

These exercise the selection-coordinate plumbing landed in PR #146 and
the copy-completeness fix from #249, *without* needing a real mouse —
`selection.set` drives the same flow `mouseDown` / `drag_begin` would,
and `selection.dump` reads the copied text back over IPC.

Nothing here touches the host pasteboard, which is why this file runs
in every lane including headless Wayland. The clipboard round-trip
lives in `test_osc52.py` alongside the OSC 52 tests that need it.

Run against any UI:

    pytest -q tools/roosttest/test_selection.py --roost-target mac
    pytest -q tools/roosttest/test_selection.py --roost-target iced
"""

from __future__ import annotations

import os
import uuid

import pytest

from util import wait_tab_quiet

# `tab.feed_pty_bytes` is gated the same way it is in `test_test_ops.py`
# — without it the handler returns `not-enabled` and every assertion
# fails for an unrelated reason.
TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"


def _seed_lines(roost, tab, n: int = 10) -> str:
    """Print a deterministic block of lines into a tab + wait for the
    last one to appear. Returns the marker prefix so the test can pick
    out a specific row. The marker is unique per call so re-runs against
    a long-lived tab don't collide with prior output."""
    marker = uuid.uuid4().hex[:6]
    # `seq` is identical on every Mac + Linux. Format each row with the
    # marker so we can reason about which row holds which content.
    roost.run(tab, f"for i in $(seq 1 {n}); do printf '{marker}-row%02d\\n' $i; done")
    roost.wait_text(tab, f"{marker}-row{n:02d}", timeout=8)
    return marker


def _row_span(dump: dict, needle: str) -> tuple[int, int, int]:
    """(viewport row, first col, last col) of `needle` in a dump.

    Cols are cell columns, which equal character indices only because
    every marker row here is pure ASCII — the wide-glyph test below
    places its content at known cells instead.
    """
    rows_text = dump["rows_text"]
    row = next(i for i, line in enumerate(rows_text) if needle in line)
    col0 = rows_text[row].index(needle)
    return row, col0, col0 + len(needle) - 1


def _scroll_out_of_view(roost, tab) -> None:
    """Push enough output to move the whole viewport into scrollback.

    Sized off the tab's own row count rather than a hardcoded 24: the
    UI sizes the grid to the window, so a taller window would leave the
    selection on screen and quietly turn the scrollback assertions back
    into viewport ones.
    """
    rows = roost.dump(tab)["rows"]
    pad = uuid.uuid4().hex[:6]
    count = rows + 5
    roost.run(tab, f"for i in $(seq 1 {count}); do printf '{pad}-pad%03d\\n' $i; done")
    roost.wait_text(tab, f"{pad}-pad{count:03d}", timeout=10)


def test_selection_set_dump_round_trip(roost, project):
    """Anchor a selection on a known row + col, then dump it. The
    returned text should be the substring of that row between the
    anchor + cursor cols."""
    tab = roost.open_tab(project, cwd="/tmp")
    marker = _seed_lines(roost, tab, n=5)
    target = f"{marker}-row03"
    row, col0, col1 = _row_span(roost.dump(tab), target)
    roost.selection_set(tab, anchor=(col0, row), cursor=(col1, row))
    sel = roost.selection_dump(tab)
    assert sel["anchor_visible"] is True
    assert sel["cursor_visible"] is True
    assert sel["text"] == target, (
        f"expected exact substring {target!r}, got {sel['text']!r}"
    )


def test_selection_clear(roost, project):
    """`selection.clear` drops the selection; `selection.dump` then
    returns the default-empty result (`text` absent / `None`, both
    visibility flags `false`). The wire schema omits `text` when
    `None` (`#[serde(skip_serializing_if = "Option::is_none")]`), so
    use `.get()` rather than subscript."""
    tab = roost.open_tab(project, cwd="/tmp")
    marker = _seed_lines(roost, tab, n=3)
    target = f"{marker}-row01"
    row, col0, col1 = _row_span(roost.dump(tab), target)
    roost.selection_set(tab, anchor=(col0, row), cursor=(col1, row))
    assert roost.selection_dump(tab).get("text") == target
    roost.selection_clear(tab)
    sel = roost.selection_dump(tab)
    assert sel.get("text") is None
    assert sel["anchor_visible"] is False
    assert sel["cursor_visible"] is False


def test_selection_survives_scroll(roost, project):
    """Regression for the scroll-drift bug (#146) and the copy
    clipping fixed in #249.

    A selection is anchored on a row, then enough output is generated
    to push that row out of the viewport and into scrollback. The
    selection still refers to the same row (screen-y stable, not
    viewport-relative) and copying it returns that row's text IN FULL
    — the endpoint being off screen is not a reason to return partial
    text or nothing at all.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    marker = _seed_lines(roost, tab, n=5)
    target = f"{marker}-row03"
    row, col0, col1 = _row_span(roost.dump(tab), target)
    roost.selection_set(tab, anchor=(col0, row), cursor=(col1, row))
    assert roost.selection_dump(tab).get("text") == target

    _scroll_out_of_view(roost, tab)

    sel = roost.selection_dump(tab)
    # `anchor_visible` / `cursor_visible` stay viewport-truthful by
    # design (D4.5) — they answer "is this endpoint on screen", which
    # is now independent of what `text` contains.
    assert sel["anchor_visible"] is False
    assert sel["cursor_visible"] is False
    assert sel.get("text") == target, (
        f"scrolled-off selection copied {sel.get('text')!r}, want {target!r}"
    )


def test_selection_spanning_scrollback_copies_every_row(roost, project):
    """A multi-row selection copies every row, in order, whether the
    rows are on screen or in scrollback.

    The scrollback text is asserted against the *same* selection's
    on-screen text rather than a second literal, so the two copy paths
    (the viewport walk and libghostty's selection formatter) cannot
    disagree without failing here.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    n = 8
    marker = _seed_lines(roost, tab, n=n)
    dump = roost.dump(tab)
    first_row, first_col, _ = _row_span(dump, f"{marker}-row01")
    last_row, _, last_col = _row_span(dump, f"{marker}-row{n:02d}")
    roost.selection_set(
        tab, anchor=(first_col, first_row), cursor=(last_col, last_row)
    )
    expected = "\n".join(f"{marker}-row{i:02d}" for i in range(1, n + 1))
    assert roost.selection_dump(tab).get("text") == expected

    _scroll_out_of_view(roost, tab)

    sel = roost.selection_dump(tab)
    assert sel["anchor_visible"] is False
    assert sel["cursor_visible"] is False
    assert sel.get("text") == expected, (
        f"scrollback selection copied {sel.get('text')!r}, want {expected!r}"
    )


def test_reversed_drag_copies_in_document_order(roost, project):
    """Dragging upward selects the same text as dragging downward.

    The endpoints are handed to the copy path raw (the formatter orders
    them itself), so a reversed drag is the case where an ordering slip
    would surface — on screen and, after scrolling, through the
    formatter path too.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    n = 6
    marker = _seed_lines(roost, tab, n=n)
    dump = roost.dump(tab)
    first_row, first_col, _ = _row_span(dump, f"{marker}-row01")
    last_row, _, last_col = _row_span(dump, f"{marker}-row{n:02d}")
    expected = "\n".join(f"{marker}-row{i:02d}" for i in range(1, n + 1))

    # Anchor on the LAST row, drag up to the first.
    roost.selection_set(
        tab, anchor=(last_col, last_row), cursor=(first_col, first_row)
    )
    assert roost.selection_dump(tab).get("text") == expected

    _scroll_out_of_view(roost, tab)

    assert roost.selection_dump(tab).get("text") == expected


@pytest.mark.skipif(
    not TEST_MODE,
    reason="seeding exact cells needs tab.feed_pty_bytes (ROOST_TEST_MODE=1)",
)
def test_wide_glyphs_copy_without_phantom_space(roost, project):
    """A CJK run copies as its graphemes, with no spacer cells leaking
    in as spaces (`你好`, not `你 好` — #249).

    The content is placed with an explicit erase + cursor-home so the
    wide graphemes sit on known CELLS: column indices and character
    indices diverge for wide glyphs, so deriving them from
    `rows_text` would be assuming the very mapping under test.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_quiet(roost, tab)
    text = "你好"
    roost.tab_feed_pty_bytes(tab, b"\x1b[2J\x1b[H" + text.encode())
    # Wait on the leading grapheme alone: `tab.dump`'s `rows_text` is one
    # character per CELL, so a wide glyph reads back as the grapheme
    # followed by a space for its spacer — the very artifact this test
    # says must not reach the copied text.
    roost.wait_text(tab, text[0], timeout=5)
    # 你 occupies cells 0-1, 好 cells 2-3. The cursor endpoint is the
    # trailing spacer on purpose: it must contribute nothing.
    roost.selection_set(tab, anchor=(0, 0), cursor=(3, 0))
    sel = roost.selection_dump(tab)
    assert sel.get("text") == text, (
        f"wide-glyph selection copied {sel.get('text')!r}, want {text!r}"
    )
