"""Native macOS menu bar E2E — plan 028 C3 (`app.menu_dump` /
`app.menu_activate`).

darwin+iced only: see `_mac_iced_only`. Covers the static menu shape and
key-equivalent policy C1/C2 built (`crates/roost-iced/src/macos/menu.rs`,
the parity port of `mac/Sources/Roost/App.swift`'s `installMainMenu()`),
plus dispatch, the dynamic Window menu, and the gating matrix.

Deviations from the plan's full AC4 gating matrix, and why:

* **User-override key equivalent**: skipped. This module drives a
  single shared session (`ICED_E2E_TESTS`); a config-override case needs
  its own launch with a different config file, which doesn't fit the
  shared-session harness here. `keybind.rs`'s unit tests
  (`row_accels_come_from_the_table_and_stop_after_nine`,
  `the_shared_inversion_feeds_the_appkit_spelling`) already pin that the
  inversion rule reads the table, not a hardcoded default — a rebind
  reaching the menu is a property of the table lookup, not of AppKit
  wiring this module would add coverage for.
* **Rename-editor / confirm-delete gating**: NOT driven here, even
  though `app.menu_activate` can technically OPEN either (`File → Rename
  Tab…` / `File → Close Project`). Neither has an IPC-reversible exit:
  there is no key-injection primitive, no cancel/complete op, and once
  either is open `text_capture` gating disables every custom menu item
  — including the Window rows a menu-driven escape would need. Opening
  one here would strand every test that runs after it in this shared
  session. Palette-open and IME-composing gating below cover the same
  `MenuGating` mechanism (`sync_gating` in `macos/menu.rs`) and are both
  fully reversible over IPC (`palette.dismiss`, `tab.feed_ime` action
  `"clear"`), so they're what this module exercises; real-key coverage
  for the editor/confirm routes is a morning real-input checklist item.
"""

from __future__ import annotations

import os
import re
import sys
import uuid

import pytest

from client import RoostError
from util import BARE_SHELL_ARGV

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# `title_fallback(BundleProfileKind::Iced)` (crates/roost-iced/src/app.rs) —
# the App menu's title is set to this at install time, so no separate
# CFBundleName-substitution case exists to test.
APP = "Roost-Iced"


@pytest.fixture(autouse=True)
def _mac_iced_only(target):
    """Skip unless BOTH halves hold — same reasoning as
    `test_dock_badge.py`'s `_mac_iced_only`: `make e2e-mac` runs this
    whole directory on macOS too (Swift has no case for these ops, so
    it would answer `unknown-op`), and the iced UI also builds for
    Linux, which has no native menu bar at all.
    """
    if sys.platform != "darwin" or target != "iced":
        pytest.skip(
            "app.menu_dump/app.menu_activate are macOS-iced-only (plan 028 § 3.12)"
        )


pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="app.menu_dump/app.menu_activate require ROOST_TEST_MODE=1 in the UI's launch env",
)

_TAB_ROW = re.compile(r"^Tab \d+$")
_WINDOW_STATIC_TAIL_TITLES = {"Minimize", "Zoom"}

# ---------------------------------------------------------------------
# Pinned static inventory — one row per non-separator item, in NSMenu
# order. `None` marks a separator. Rows are
# (title, key_equivalent, modifiers, enabled, action).
#
# Key equivalents follow the deterministic inversion rule
# (`menu_accel_for_action`, roost-ui-model/src/keybind.rs § "prefer
# SUPER, tie-break by default_bindings() declaration order") against
# macOS's default bindings (`primary`/`project_mod`/`clipboard_mod` all
# = "super" on macOS). These are the DEFAULT bindings only — see the
# module docstring for why a user-override case isn't covered here.
# ---------------------------------------------------------------------

STATIC_MENUS: dict[str, list[tuple | None]] = {
    APP: [
        (f"About {APP}", "", [], True, "appkit:orderFrontStandardAboutPanel:"),
        ("Check for Updates…", "", [], False, None),
        None,
        (f"Hide {APP}", "h", ["super"], True, "appkit:hide:"),
        ("Hide Others", "h", ["alt", "super"], True, "appkit:hideOtherApplications:"),
        ("Show All", "", [], True, "appkit:unhideAllApplications:"),
        None,
        (f"Quit {APP}", "q", ["super"], True, "quit"),
    ],
    "File": [
        ("New Project", "n", ["super"], True, "new_project"),
        ("New Tab", "t", ["super"], True, "new_tab"),
        ("Close Tab", "w", ["super"], True, "close_tab"),
        None,
        ("Rename Tab…", "r", ["super"], True, "rename_tab"),
        ("Rename Project…", "r", ["shift", "super"], True, "rename_project"),
        ("Close Project", "w", ["shift", "super"], True, "close_project"),
        None,
        ("Previous Tab", "[", ["shift", "super"], True, "cycle_tab_prev"),
        ("Next Tab", "]", ["shift", "super"], True, "cycle_tab_next"),
    ],
    "View": [
        ("Command Palette…", "p", ["shift", "super"], True, "command_palette"),
        ("Command Launcher…", "t", ["shift", "super"], True, "command_launcher"),
        ("Agent Palette…", "o", ["shift", "super"], True, "agent_palette"),
        ("Custom Commands…", "e", ["shift", "super"], True, "custom_palette"),
        None,
        ("Zoom In", "+", ["super"], True, "font_increase"),
        ("Zoom Out", "-", ["super"], True, "font_decrease"),
        ("Actual Size", "0", ["super"], True, "font_reset"),
        None,
        ("Toggle Sidebar", "b", ["super"], True, "toggle_sidebar"),
        ("Toggle Sidebar Agents", "a", ["shift", "super"], True, "toggle_sidebar_agents"),
        None,
        ("Jump to Unread", "u", ["shift", "super"], True, "jump_to_unread"),
    ],
    "Edit": [
        ("Cut", "", [], False, None),
        ("Copy", "c", ["super"], True, "copy"),
        ("Paste", "v", ["super"], True, "paste"),
        ("Select All", "", [], False, None),
    ],
}

# The Window menu's static tail, after whatever dynamic project/tab rows
# `WindowRows::derive` produced.
WINDOW_TAIL = [
    ("Minimize", "m", ["super"], True, "appkit:performMiniaturize:"),
    ("Zoom", "", [], True, "appkit:performZoom:"),
]

PALETTE_TOGGLE_TITLES = (
    "Command Palette…",
    "Command Launcher…",
    "Agent Palette…",
    "Custom Commands…",
)


def _menus_by_title(dump: list[dict]) -> dict[str, dict]:
    return {menu["title"]: menu for menu in dump}


def _row(item: dict) -> tuple | None:
    if item["separator"]:
        return None
    return (
        item["title"],
        item["key_equivalent"],
        item["modifiers"],
        item["enabled"],
        item["action"],
    )


def _rows(menu: dict) -> list[tuple | None]:
    return [_row(item) for item in menu["items"]]


def _items_by_title(menu: dict) -> dict[str, dict]:
    return {item["title"]: item for item in menu["items"] if not item["separator"]}


def _window_items(roost) -> list[dict]:
    menus = _menus_by_title(roost.app_menu_dump())
    return [i for i in menus["Window"]["items"] if not i["separator"]]


def _window_tab_titles(roost) -> list[str]:
    return [i["title"] for i in _window_items(roost) if _TAB_ROW.match(i["title"])]


def _window_tab_states(roost) -> list[str]:
    return [i["state"] for i in _window_items(roost) if _TAB_ROW.match(i["title"])]


def _window_project_titles(roost) -> list[str]:
    return [
        i["title"]
        for i in _window_items(roost)
        if i["title"] not in _WINDOW_STATIC_TAIL_TITLES and not _TAB_ROW.match(i["title"])
    ]


def _file_new_tab_enabled(roost) -> bool:
    menus = _menus_by_title(roost.app_menu_dump())
    return _items_by_title(menus["File"])["New Tab"]["enabled"]


class TestMenuShape:
    """§ AC1: the dump matches the pinned inventory C1/C2 built."""

    def test_menu_titles(self, roost):
        titles = [menu["title"] for menu in roost.app_menu_dump()]
        assert titles == [APP, "File", "View", "Edit", "Window"]

    def test_static_items_match_the_pinned_inventory(self, roost):
        menus = _menus_by_title(roost.app_menu_dump())
        for title, expected in STATIC_MENUS.items():
            assert _rows(menus[title]) == expected, title

    def test_window_menu_ends_with_minimize_and_zoom(self, roost):
        menus = _menus_by_title(roost.app_menu_dump())
        rows = _rows(menus["Window"])
        assert rows[-2:] == WINDOW_TAIL

    def test_static_actionable_item_count_is_28_plus_minimize_zoom(self, roost):
        menus = _menus_by_title(roost.app_menu_dump())
        static_count = sum(
            len([i for i in menus[t]["items"] if not i["separator"]])
            for t in (APP, "File", "View", "Edit")
        )
        assert static_count == 28
        assert _rows(menus["Window"])[-2:] == WINDOW_TAIL

    def test_separator_counts_match_swift(self, roost):
        menus = _menus_by_title(roost.app_menu_dump())
        counts = {
            t: sum(1 for i in menus[t]["items"] if i["separator"])
            for t in (APP, "File", "View", "Edit")
        }
        assert counts == {APP: 2, "File": 2, "View": 3, "Edit": 0}

    def test_cut_and_select_all_are_disabled_with_no_key_equivalent(self, roost):
        edit = _items_by_title(_menus_by_title(roost.app_menu_dump())["Edit"])
        for title in ("Cut", "Select All"):
            assert edit[title]["enabled"] is False
            assert edit[title]["key_equivalent"] == ""

    def test_check_for_updates_is_disabled(self, roost):
        app_menu = _items_by_title(_menus_by_title(roost.app_menu_dump())[APP])
        assert app_menu["Check for Updates…"]["enabled"] is False


class TestKeyEquivalents:
    """§ AC1: key equivalents against the deterministic accel-inversion
    rule, named examples from the plan plus the full table above."""

    @pytest.mark.parametrize(
        "menu_title,item_title,key,modifiers",
        [
            ("File", "New Tab", "t", ["super"]),
            ("Edit", "Copy", "c", ["super"]),
            ("View", "Command Palette…", "p", ["shift", "super"]),
            ("File", "Previous Tab", "[", ["shift", "super"]),
        ],
    )
    def test_pinned_examples(self, roost, menu_title, item_title, key, modifiers):
        menus = _menus_by_title(roost.app_menu_dump())
        item = _items_by_title(menus[menu_title])[item_title]
        assert item["key_equivalent"] == key
        assert item["modifiers"] == modifiers


class TestDispatch:
    """§ AC2: menu activation reaches the workspace op set through the
    real AppKit -> channel -> update-loop path."""

    def test_new_tab_grows_tab_list(self, roost, project):
        # "File -> New Tab" acts on the ACTIVE project (menu-driven, same
        # as a real keystroke) — a project the `project` fixture merely
        # created over IPC is not active until something focuses one of
        # its tabs (`workspace.rs`: only opening/focusing a tab moves
        # `active_project_id`). Seed one throwaway tab first so `project`
        # is the one "New Tab" lands in.
        roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        before = len(roost.project_tab_ids(project))
        roost.app_menu_activate(["File", "New Tab"])
        roost._wait(
            lambda: len(roost.project_tab_ids(project)) == before + 1,
            5.0,
            "File -> New Tab opens a tab in the active project",
        )

    def test_window_project_row_switches_project(self, roost):
        name_a = f"pytest-menu-{uuid.uuid4().hex[:6]}"
        name_b = f"pytest-menu-{uuid.uuid4().hex[:6]}"
        pid_a = roost.create_project(name=name_a, cwd="/tmp")
        pid_b = roost.create_project(name=name_b, cwd="/tmp")
        try:
            # `select_project` (the Window row's dispatch target) focuses
            # `workspace.preferred_tab(project_id)` — a tab-less project
            # has no preferred tab, so the selection would silently
            # no-op. Seed one throwaway tab in each first.
            roost.open_tab(pid_a, cwd="/tmp", argv=BARE_SHELL_ARGV)
            roost.open_tab(pid_b, cwd="/tmp", argv=BARE_SHELL_ARGV)
            roost._wait(
                lambda: {name_a, name_b} <= set(_window_project_titles(roost)),
                5.0,
                "both new projects appear as Window rows",
            )
            roost.app_menu_activate(["Window", name_a])
            roost._wait(
                lambda: roost.identify()["active_project_id"] == pid_a,
                5.0,
                "Window row selects project A",
            )
            roost.app_menu_activate(["Window", name_b])
            roost._wait(
                lambda: roost.identify()["active_project_id"] == pid_b,
                5.0,
                "Window row selects project B",
            )
        finally:
            roost.delete_project(pid_a)
            roost.delete_project(pid_b)

    def test_window_tab_row_switches_tab(self, roost, project):
        tab_a = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        tab_b = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        roost.focus(tab_a)
        roost._wait(
            lambda: _window_tab_titles(roost) == ["Tab 1", "Tab 2"],
            5.0,
            "both tabs of the active project show as rows",
        )
        roost.app_menu_activate(["Window", "Tab 2"])
        roost._wait(
            lambda: roost.identify()["active_tab_id"] == tab_b,
            5.0,
            "Window row 'Tab 2' selects the second tab",
        )
        roost.app_menu_activate(["Window", "Tab 1"])
        roost._wait(
            lambda: roost.identify()["active_tab_id"] == tab_a,
            5.0,
            "Window row 'Tab 1' selects the first tab",
        )


class TestDynamicRows:
    """§ AC3: Window-menu rows track project/tab open/close/select,
    with active-state checkmarks."""

    def test_tab_rows_track_open_close_and_selection(self, roost, project):
        tab_a = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        roost.focus(tab_a)
        roost._wait(
            lambda: _window_tab_titles(roost) == ["Tab 1"],
            5.0,
            "one open tab shows as one Window row",
        )
        assert _window_tab_states(roost) == ["on"]

        tab_b = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        roost.focus(tab_b)
        roost._wait(
            lambda: _window_tab_titles(roost) == ["Tab 1", "Tab 2"],
            5.0,
            "a second open tab adds a Window row",
        )
        roost._wait(
            lambda: _window_tab_states(roost) == ["off", "on"],
            5.0,
            "the checkmark follows the newly focused tab",
        )

        roost.focus(tab_a)
        roost._wait(
            lambda: _window_tab_states(roost) == ["on", "off"],
            5.0,
            "the checkmark follows focus back to the first tab",
        )

        roost.close_tab(tab_b)
        roost._wait(
            lambda: _window_tab_titles(roost) == ["Tab 1"],
            5.0,
            "closing a tab drops its Window row",
        )

    def test_project_rows_track_open_and_close(self, roost):
        name = f"pytest-menu-{uuid.uuid4().hex[:6]}"
        pid = roost.create_project(name=name, cwd="/tmp")
        roost._wait(
            lambda: name in _window_project_titles(roost),
            5.0,
            "a new project adds a Window row",
        )
        roost.delete_project(pid)
        roost._wait(
            lambda: name not in _window_project_titles(roost),
            5.0,
            "deleting the project drops its Window row",
        )


class TestGating:
    """§ AC4: palette-open and IME-composing gating — see the module
    docstring for why rename-editor/confirm-delete gating isn't driven
    here."""

    def test_palette_open_disables_non_toggle_items_and_blanks_clipboard(
        self, roost, palette
    ):
        palette.palette_open("commands")
        roost._wait(
            lambda: _file_new_tab_enabled(roost) is False,
            5.0,
            "palette-open gating disables File -> New Tab",
        )

        menus = _menus_by_title(roost.app_menu_dump())
        file_items = _items_by_title(menus["File"])
        assert file_items["New Tab"]["enabled"] is False
        assert file_items["Close Tab"]["enabled"] is False

        edit_items = _items_by_title(menus["Edit"])
        assert edit_items["Copy"]["key_equivalent"] == ""
        assert edit_items["Paste"]["key_equivalent"] == ""

        view_items = _items_by_title(menus["View"])
        for toggle in PALETTE_TOGGLE_TITLES:
            assert view_items[toggle]["enabled"] is True, toggle

        app_items = _items_by_title(menus[APP])
        assert app_items[f"Quit {APP}"]["enabled"] is True

        before = len(roost.tabs())
        with pytest.raises(RoostError) as excinfo:
            roost.app_menu_activate(["File", "New Tab"])
        assert excinfo.value.code == "invalid-param"
        assert len(roost.tabs()) == before

        palette.palette_dismiss()
        roost._wait(
            lambda: _file_new_tab_enabled(roost) is True,
            5.0,
            "dismissing the palette restores File -> New Tab",
        )
        menus = _menus_by_title(roost.app_menu_dump())
        edit_items = _items_by_title(menus["Edit"])
        assert edit_items["Copy"]["key_equivalent"] == "c"
        assert edit_items["Paste"]["key_equivalent"] == "v"

    def test_ime_composing_disables_everything_including_palette_toggles(
        self, roost, project
    ):
        tab_id = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
        roost.focus(tab_id)
        roost.tab_feed_ime(tab_id, "preedit", text="あ")
        try:
            roost._wait(
                lambda: _file_new_tab_enabled(roost) is False,
                5.0,
                "IME composition disables File -> New Tab",
            )
            menus = _menus_by_title(roost.app_menu_dump())
            edit_items = _items_by_title(menus["Edit"])
            assert edit_items["Copy"]["key_equivalent"] == ""
            assert edit_items["Paste"]["key_equivalent"] == ""

            # Unlike palette-open gating, text-capture gating spares
            # NOTHING custom — the four palette toggles go dead too
            # (command_enabled's rule: text_capture outranks the
            # palette-toggle exemption).
            view_items = _items_by_title(menus["View"])
            for toggle in PALETTE_TOGGLE_TITLES:
                assert view_items[toggle]["enabled"] is False, toggle

            with pytest.raises(RoostError) as excinfo:
                roost.app_menu_activate(["File", "New Tab"])
            assert excinfo.value.code == "invalid-param"

            # The composition must never have reached the PTY — this is
            # the terminal-side half of the same guarantee the blanked
            # Copy/Paste equivalents exist for.
            assert roost.tab_capture_pty_input(tab_id, drain=False) == b""
        finally:
            roost.tab_feed_ime(tab_id, "clear")

        roost._wait(
            lambda: _file_new_tab_enabled(roost) is True,
            5.0,
            "ending the IME session restores File -> New Tab",
        )


class TestActivateErrors:
    """Unknown and ambiguous paths, per plan § 3.12's documented
    caveats."""

    def test_unknown_path_errors(self, roost):
        with pytest.raises(RoostError) as excinfo:
            roost.app_menu_activate(["File", "Not A Real Menu Item"])
        assert excinfo.value.code == "invalid-param"

    def test_ambiguous_title_errors(self, roost):
        name = f"pytest-menu-dup-{uuid.uuid4().hex[:6]}"
        pid_a = roost.create_project(name=name, cwd="/tmp")
        pid_b = roost.create_project(name=name, cwd="/tmp")
        try:
            roost._wait(
                lambda: _window_project_titles(roost).count(name) == 2,
                5.0,
                "two same-named project rows appear",
            )
            with pytest.raises(RoostError) as excinfo:
                roost.app_menu_activate(["Window", name])
            assert excinfo.value.code == "invalid-param"
        finally:
            roost.delete_project(pid_a)
            roost.delete_project(pid_b)
