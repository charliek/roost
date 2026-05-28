"""End-to-end tests for the `selection.*` and `clipboard.*` IPC ops.

These exercise the selection-coordinate plumbing landed in PR #146 and
the copy-on-select / middle-click work from PR #147, *without* needing
a real mouse — `selection.set` drives the same flow `mouseDown` /
`drag_begin` would. The clipboard ops let tests assert on the host
pasteboard, which roosttest previously had no way to read.

Run against either UI:

    pytest -q tools/roosttest/test_selection.py --roost-target mac
    pytest -q tools/roosttest/test_selection.py --roost-target gtk
"""

from __future__ import annotations

import uuid


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


def test_selection_set_dump_round_trip(roost, project):
    """Anchor a selection on a known row + col, then dump it. The
    returned text should be the substring of that row between the
    anchor + cursor cols."""
    tab = roost.open_tab(project, cwd="/tmp")
    marker = _seed_lines(roost, tab, n=5)
    dump = roost.dump(tab)
    # Find the viewport row holding row-03.
    target = f"{marker}-row03"
    rows_text = dump["rows_text"]
    row_idx = next(i for i, line in enumerate(rows_text) if target in line)
    col_start = rows_text[row_idx].index(target)
    col_end = col_start + len(target)
    roost.selection_set(
        tab,
        anchor=(col_start, row_idx),
        cursor=(col_end - 1, row_idx),
    )
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
    _seed_lines(roost, tab, n=3)
    roost.selection_set(tab, anchor=(0, 0), cursor=(3, 0))
    assert roost.selection_dump(tab).get("text") is not None
    roost.selection_clear(tab)
    sel = roost.selection_dump(tab)
    assert sel.get("text") is None
    assert sel["anchor_visible"] is False
    assert sel["cursor_visible"] is False


def test_clipboard_write_dump_round_trip(roost, project):
    """`clipboard.write` + `clipboard.dump` round-trip via the host
    pasteboard. Sanity check for the test ops themselves — they're
    needed by the OSC 52 PR's E2E test."""
    # Use a unique payload so a leaked prior clipboard value doesn't
    # produce a false pass.
    payload = f"roost-clip-{uuid.uuid4().hex[:8]}"
    roost.clipboard_write("system", payload)
    assert roost.clipboard_dump("system") == payload


def test_selection_survives_scroll(roost, project):
    """Regression for the scroll-drift bug fixed in PR #146.

    Selection is anchored on a row, then enough output is generated to
    scroll the original viewport position off-screen. The selection
    should track the row (screen-y stable), not the viewport position.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    marker = _seed_lines(roost, tab, n=5)
    dump = roost.dump(tab)
    rows_text = dump["rows_text"]
    target = f"{marker}-row03"
    row_idx = next(i for i, line in enumerate(rows_text) if target in line)
    col_start = rows_text[row_idx].index(target)
    col_end = col_start + len(target)
    roost.selection_set(
        tab,
        anchor=(col_start, row_idx),
        cursor=(col_end - 1, row_idx),
    )
    # Generate enough new output to push the original row off-screen.
    # The default 24-row viewport needs roughly that many extra lines.
    pad = uuid.uuid4().hex[:6]
    roost.run(tab, f"for i in $(seq 1 30); do printf '{pad}-pad%02d\\n' $i; done")
    roost.wait_text(tab, f"{pad}-pad30", timeout=8)
    # The originally-selected row may have scrolled off the visible
    # viewport entirely. In that case copy returns None (partial-copy
    # limitation documented for v1) and `anchor_visible` is false.
    # Either way the selection didn't silently start pointing at the
    # wrong text — that's the regression we're checking.
    sel = roost.selection_dump(tab)
    text = sel.get("text")
    if text is not None:
        # If still partially visible, the text must match the original
        # row content; it must NOT be one of the new pad rows.
        assert target in text or text == target, (
            f"selection drifted to wrong content: {text!r}"
        )
        assert pad not in text, (
            f"selection picked up unrelated newer content: {text!r}"
        )
