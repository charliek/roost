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
import ui
from client import RoostError
from util import BARE_SHELL_ARGV, wait_tab_attached

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
    "select_font",
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


def test_activate_select_font_pushes_subframe(palette):
    palette.palette_open()
    st = palette.palette_activate("select_font")
    # Drilling into the font list: a new frame, palette still open.
    assert st["open"] is True
    assert st["frame"] == "fonts"
    assert len(st["items"]) > 0, "font list should not be empty"


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


def test_theme_frame_keeps_selection_in_view(target, palette):
    """The theme list opens pre-positioned on the active theme; the palette
    must scroll that row into the viewport so its highlight is visible.

    Regression for a GTK bug where the pre-selected row rode off the bottom
    (invisible highlight, though arrowing still changed the theme) because
    nothing scrolled the list to the selection. The bundled theme set
    overflows the palette viewport and a fresh instance's default active
    theme (`roost-dark`) sorts last, so the pre-selected row starts
    off-screen — exercising the scroll. GTK and Iced report measured geometry;
    the Mac UI scrolls correctly but does not expose the field."""
    if target == "mac":
        pytest.skip("selected_in_view geometry is not exposed by the Mac UI")
    palette.palette_open()
    st = palette.palette_activate("select_theme")
    assert st["frame"] == "themes"
    assert len(st["items"]) > 1, "theme list should have multiple rows"
    # The scroll lands on a frame tick after layout settles, so poll until
    # the highlighted row is fully within the viewport (rather than reading
    # a single pre-layout snapshot).
    palette._wait(
        lambda: palette.palette_state().get("selected_in_view") is True,
        10.0,
        "theme frame scrolls the pre-selected row into view",
    )

    # A viewport change invalidates the old geometry. The selected row must be
    # remeasured and, if the smaller viewport clips it, minimally revealed
    # again instead of retaining a stale `true` result.
    palette.window_resize(800, 360)
    palette._wait(
        lambda: palette.palette_state().get("selected_in_view") is True,
        10.0,
        "theme selection remains visible after window resize",
    )


def _default_background(roost, tab_id: int) -> str:
    cells = roost.tab_dump_resolved(tab_id)["cells"]
    assert cells, f"tab {tab_id} has no resolved cells"
    return cells[0]["bg"]


def _theme_lines(path) -> list[str]:
    return [
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.partition("=")[0].strip() == "theme"
    ]


def test_theme_preview_reverts_and_confirm_persists_for_all_tabs(
    roost, project, palette
):
    """Theme preview is live-only; confirmation is live + persistent.

    Refuse to perform even the first mutating UI action when this session does
    not belong to the harness. That keeps a reused developer instance and its
    real config completely out of this write-back test.
    """
    config_path = ui.owned_session_config_path()
    if config_path is None:
        pytest.skip("theme persistence requires a harness-owned config copy")
    assert config_path.parent == ui._SESSION_STATE_DIR.resolve()
    assert config_path != ui.SEED_CONFIG.resolve()

    seed_before = ui.SEED_CONFIG.read_bytes()
    config_before = config_path.read_bytes()
    first = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    second = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, first)
    wait_tab_attached(roost, second)
    original_backgrounds = {
        first: _default_background(roost, first),
        second: _default_background(roost, second),
    }

    palette.palette_open()
    themes = palette.palette_activate("select_theme")
    original = themes["items"][themes["selection"]]
    preferred = "Oxocarbon" if original["id"] != "Oxocarbon" else "roost-dark"
    target = next(item for item in themes["items"] if item["id"] == preferred)
    third = None
    try:
        preview = palette.palette_query(target["title"])
        assert preview["items"][preview["selection"]]["id"] == target["id"]
        roost._wait(
            lambda: all(
                _default_background(roost, tab_id) != original_backgrounds[tab_id]
                for tab_id in (first, second)
            ),
            5.0,
            "theme preview reaches both existing tabs",
        )
        preview_background = _default_background(roost, first)
        assert _default_background(roost, second) == preview_background
        assert config_path.read_bytes() == config_before

        dismissed = palette.palette_dismiss()
        assert dismissed["open"] is False
        roost._wait(
            lambda: all(
                _default_background(roost, tab_id) == original_backgrounds[tab_id]
                for tab_id in (first, second)
            ),
            5.0,
            "dismissed theme preview reverts both existing tabs",
        )
        assert config_path.read_bytes() == config_before

        palette.palette_open()
        palette.palette_activate("select_theme")
        confirmed = palette.palette_activate(target["id"])
        assert confirmed["open"] is False
        roost._wait(
            lambda: all(
                _default_background(roost, tab_id) == preview_background
                for tab_id in (first, second)
            ),
            5.0,
            "confirmed theme reaches both existing tabs",
        )
        assert _theme_lines(config_path) == [f"theme = {target['id']}"]
        assert ui.SEED_CONFIG.read_bytes() == seed_before

        third = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        wait_tab_attached(roost, third)
        assert _default_background(roost, third) == preview_background
    finally:
        palette.palette_dismiss()
        palette.palette_open()
        palette.palette_activate("select_theme")
        restored = palette.palette_activate(original["id"])
        assert restored["open"] is False
        expected_tabs = [first, second] + ([third] if third is not None else [])
        roost._wait(
            lambda: all(
                _default_background(roost, tab_id) == original_backgrounds[first]
                for tab_id in expected_tabs
            ),
            5.0,
            "theme cleanup restores every fixture tab",
        )
        assert _theme_lines(config_path) == [f"theme = {original['id']}"]
        assert ui.SEED_CONFIG.read_bytes() == seed_before
