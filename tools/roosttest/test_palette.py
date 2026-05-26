"""Command-palette E2E — drives the palette overlay over IPC (open,
introspect, filter, activate a row, dismiss) against either UI via
`--roost-target`.

Activating a palette row dispatches the *same* command its keybind
would (a command row's id IS the KeybindAction id), so these also
exercise command dispatch end-to-end — the north star. Assertions use
only the command ids both UIs expose; the Mac-only `jump_to_unread` and
the `close_project`/`delete_project` split are a known parity gap (see
the harness README), deliberately not asserted here.
"""

from __future__ import annotations

import uuid

import pytest
from client import RoostError

# The shared `palette` fixture (drive from closed, leave closed) lives in
# conftest.py so the notification + launcher suites reuse it.

# Curated command rows present in BOTH UIs with the same wire id. The
# two UIs are kept at parity, so this is the full command-palette set
# (minus the dynamic notification rows) — `close_project` + `jump_to_unread`
# were unified/ported in the P8 parity pass.
COMMON_COMMAND_IDS = (
    "new_tab",
    "close_tab",
    "rename_tab",
    "cycle_tab_next",
    "cycle_tab_prev",
    "new_project",
    "rename_project",
    "close_project",
    "toggle_sidebar",
    "jump_to_unread",
    "font_increase",
    "font_decrease",
    "font_reset",
    "select_theme",
)


def test_open_lists_common_commands(palette):
    st = palette.palette_open()
    assert st["open"] is True
    assert st["frame"] == "commands"
    ids = palette.palette_item_ids(st)
    missing = [c for c in COMMON_COMMAND_IDS if c not in ids]
    assert not missing, f"command rows missing {missing}; got {ids}"


def test_state_reflects_open_then_closed(palette):
    assert palette.palette_state()["open"] is False
    palette.palette_open()
    assert palette.palette_state()["open"] is True
    st = palette.palette_dismiss()
    assert st["open"] is False
    assert palette.palette_item_ids(st) == []


def test_query_filters_rows(palette):
    palette.palette_open()
    st = palette.palette_query("theme")
    assert st["query"] == "theme"
    ids = palette.palette_item_ids(st)
    assert "select_theme" in ids, ids
    # The filter narrows the list — an unrelated command drops out.
    assert "new_tab" not in ids, ids
    # Selection resets to the top match (a valid row).
    assert 0 <= st["selection"] < len(st["items"])


def test_query_no_match_yields_empty(palette):
    palette.palette_open()
    st = palette.palette_query(uuid.uuid4().hex)  # matches nothing
    assert st["open"] is True
    assert palette.palette_item_ids(st) == []


def test_activate_select_theme_pushes_subframe(palette):
    palette.palette_open()
    st = palette.palette_activate("select_theme")
    # Drilling into the theme list: a new frame, palette still open.
    assert st["open"] is True
    assert st["frame"] == "themes"
    assert len(st["items"]) > 0, "theme list should not be empty"


def test_activate_unknown_id_is_not_found(palette):
    palette.palette_open()
    with pytest.raises(RoostError) as ei:
        palette.palette_activate("no_such_command_" + uuid.uuid4().hex[:6])
    assert ei.value.code == "not-found"
    # A failed activate leaves the palette open (nothing was confirmed).
    assert palette.palette_state()["open"] is True


def test_activate_when_closed_is_not_found(palette):
    # No palette open → activating any id is not-found.
    with pytest.raises(RoostError) as ei:
        palette.palette_activate("new_tab")
    assert ei.value.code == "not-found"


def test_activate_new_tab_dispatches_command(roost, project, palette):
    """Activating `new_tab` runs the command (closes the palette) and a
    tab actually appears — proving the palette routes to the same
    dispatch as the hotkey, not just a UI poke."""
    seed = roost.open_tab(project, cwd="/tmp")
    roost.focus(seed)  # make `project` active so the new tab lands here
    before = len(roost.tabs())
    palette.palette_open()
    st = palette.palette_activate("new_tab")
    assert st["open"] is False  # new_tab confirms + closes the palette
    roost._wait(
        lambda: len(roost.tabs()) == before + 1,
        5.0,
        "palette new_tab dispatch adds a tab",
    )


def test_open_launcher_frame(palette):
    st = palette.palette_open(kind="launcher")
    assert st["open"] is True
    assert st["frame"] == "launcher"
