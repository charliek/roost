"""Right-click menus, through their test ops (plan 073 D9, #338).

`app.context_menu_dump` lists the menu a row shows, and
`app.context_menu_activate` runs one item through the one dispatcher a
click reaches — the macOS popup and the Linux overlay call it too. So
these cases pin the menu's content and every item's effect without a
pointer.

The UI learns about a tab or project the engine just opened or closed a
moment after the op that did it returns, so a case waits for the row to
reach the window (`wait_tab_attached`, `_wait_unlisted`) before asking for
its menu.

Copy Path reads the native clipboard, so it is in
`test_context_menu_clipboard.py`, on the clipboard lanes.

On Linux, `app.context_menu_open` shows the overlay a right-click shows,
and the last cases drive it with `app.key_event`. macOS draws a native
popup instead, which the op does not open.

Skipped under `--roost-target mac`: the Swift app has no context-menu ops.
"""

from __future__ import annotations

import contextlib
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

import agent_jail
import pytest
import session as sessionlib
import ui
from client import Roost, RoostError, scaled_timeout
from test_newtab_cwd import (
    LIVE_CWD,
    _active_tab_in_live_cwd,
    _assert_shell_in,
    _press_new_tab,
)
from util import drain, drain_until_match, roostctl_path, spawned_tab_id, wait_tab_attached

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

OPEN_FOLDER = "Open in Finder" if sys.platform == "darwin" else "Open in File Manager"

#: Typed while New Tab Here's tab opens.
TYPED_AHEAD = "echo MARK_$((6*7))"

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="the app.context_menu_* ops require ROOST_TEST_MODE=1",
)


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("the context-menu ops are iced's; Roost.app answers unknown-op")


def tab_target(tab: int) -> dict:
    return {"tab_id": str(tab)}


def project_target(project: int) -> dict:
    return {"project_id": str(project)}


def item(action: str, label: str) -> dict:
    return {"action": action, "label": label, "enabled": True}


SEPARATOR = {"separator": True}

TAB_MENU = [
    item("rename_tab", "Rename…"),
    item("new_tab_here", "New Tab Here"),
    item("copy_tab_path", "Copy Path"),
    SEPARATOR,
    item("close_tab", "Close Tab"),
]


def refused(call, *args) -> RoostError:
    with pytest.raises(RoostError) as raised:
        call(*args)
    return raised.value


def _listed_tab(roost, project: int) -> int:
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    return tab


def _shown(roost, tab: int) -> None:
    roost.focus(tab)
    roost._wait(lambda: roost.app_selected_tab_id() == tab, 5.0, f"tab {tab} on screen")


def _wait_unlisted(roost, target: dict) -> None:
    def unlisted() -> bool:
        try:
            roost.context_menu_dump(target)
        except RoostError as error:
            return error.code == "invalid-param"
        return False

    roost._wait(unlisted, 5.0, f"the window to drop {target}")


def test_dump_lists_a_tabs_and_a_projects_items(roost, project):
    tab = _listed_tab(roost, project)
    assert roost.context_menu_dump(tab_target(tab)) == TAB_MENU
    assert roost.context_menu_dump(project_target(project)) == [
        item("new_tab", "New Tab"),
        item("rename_project", "Rename…"),
        item("copy_project_path", "Copy Path"),
        item("open_project_folder", OPEN_FOLDER),
        SEPARATOR,
        item("close_project", "Close Project…"),
    ]


def test_new_tab_here_opens_where_an_unshown_tab_is_and_takes_the_keys(roost, project):
    """The source tab is in a project that is not on screen. The new tab
    lands in the source's project, in the source's directory, and takes
    the selection — and keys typed while it opens reach it, never the tab
    that was on screen (072-D2's pending keyboard)."""
    source = _active_tab_in_live_cwd(roost, project)

    other = roost.create_project(name=f"pytest-{uuid.uuid4().hex[:8]}", cwd="/tmp")
    try:
        shown = _listed_tab(roost, other)
        _shown(roost, shown)
        drain(roost, shown)
        before = {int(row["id"]) for row in roost.tabs()}

        roost.context_menu_activate(tab_target(source), "new_tab_here")
        roost.type_text(TYPED_AHEAD)
        roost.key_event("Enter")

        opened = spawned_tab_id(roost, before, "New Tab Here opened a tab")
        assert int(roost.tab(opened)["project_id"]) == project
        roost._wait(
            lambda: roost.app_selected_tab_id() == opened,
            5.0,
            "the new tab takes the selection",
        )
        drain_until_match(roost, opened, rb"echo MARK_\$\(\(6\*7\)\)")
        assert b"MARK" not in drain(roost, shown), "the tab that was on screen was sent the keys"
        _assert_shell_in(roost, opened, LIVE_CWD)
    finally:
        roost.delete_project(other)


def _opens_where_new_tab_does(roost, project: int, shown_project: int) -> None:
    """The project row's New Tab lands where the ⌘T gesture does, in the
    remembered tab's directory (#589). `shown_project` is on screen, so the
    row's own project is not the active one."""
    source = _active_tab_in_live_cwd(roost, project)
    shown = _listed_tab(roost, shown_project)

    _shown(roost, source)
    typed = _press_new_tab(roost, roost)
    expected = roost.tab(typed)["cwd"]
    assert expected == LIVE_CWD, "the precondition: the gesture opens in the live cwd"
    _assert_shell_in(roost, typed, LIVE_CWD)

    _shown(roost, shown)
    before = {int(row["id"]) for row in roost.tabs()}
    roost.context_menu_activate(project_target(project), "new_tab")
    opened = spawned_tab_id(roost, before, "the project row's New Tab opened a tab")
    assert int(roost.tab(opened)["project_id"]) == project
    assert roost.tab(opened)["cwd"] == expected
    _assert_shell_in(roost, opened, LIVE_CWD)


def test_a_project_rows_new_tab_opens_where_the_new_tab_gesture_would(roost, project):
    """The palette's `new_tab` row runs the same dispatch as ⌘T: both reach
    `new_tab_dispatch`, which opens from `active_tab_key()`.
    `app.keybind_dispatch` is paste-only, so it cannot press the gesture."""
    other = roost.create_project(name=f"pytest-{uuid.uuid4().hex[:8]}", cwd="/tmp")
    try:
        _opens_where_new_tab_does(roost, project, other)
    finally:
        roost.delete_project(other)


@contextlib.contextmanager
def _session_backend_ui(target: str):
    """The UI relaunched on `local-backend = session`, inside a private
    runtime dir so the session it spawns is not the developer's, then the
    in-process UI put back for whatever module runs next."""
    if sys.platform == "darwin":
        pytest.skip(
            "the session is isolated through XDG_RUNTIME_DIR, which is Linux "
            "only; the macOS variant is #390"
        )
    state_dir = ui.session_state_dir()
    config = ui.owned_session_config_path()
    if state_dir is None or config is None:
        pytest.skip("the session backend needs a harness-owned UI")

    root = Path(tempfile.mkdtemp(prefix="roost-cm-", dir="/tmp")).resolve()
    run = root / "run"
    agent_jail.make_private_runtime_dir(run)
    private = {
        "XDG_RUNTIME_DIR": str(run),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "state"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "ROOST_SESSION_BIN": str(sessionlib.session_binary()),
    }
    saved = {key: os.environ.get(key) for key in private}
    original_config = config.read_text()
    derived = state_dir / ui.DERIVED_SESSION_SUBDIR
    stop = None
    try:
        ui.quit(target)
        os.environ.update(private)
        (state_dir / "state.json").unlink(missing_ok=True)
        shutil.rmtree(derived, ignore_errors=True)
        lines = [
            line
            for line in original_config.splitlines()
            if not line.strip().startswith("local-backend")
        ]
        config.write_text("\n".join([*lines, "local-backend = session"]) + "\n")
        ui.launch(target, state_dir=state_dir, force=True)
        client = Roost(ui.socket_path(target))
        try:
            assert client.identify()["local_backend"] == "session"
            sessionlib.wait_until(
                lambda: (client.sidebar_local_band() or {}).get("state") == "connected",
                scaled_timeout(60.0),
                "the local session to connect",
            )
            yield client
        finally:
            client.close()
    finally:
        try:
            with contextlib.suppress(Exception):
                ui.quit(target)
            stop = subprocess.run(
                [roostctl_path(), "session", "stop"],
                capture_output=True,
                text=True,
                timeout=scaled_timeout(60.0),
            )
        finally:
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
            config.write_text(original_config)
            (state_dir / "state.json").unlink(missing_ok=True)
            # The derived session dir is left to the harness's sweep, which
            # proves the daemon's state lock is free before it deletes it.
            if stop is not None and stop.returncode == 0:
                shutil.rmtree(root, ignore_errors=True)
            ui.launch(target, state_dir=state_dir, force=True)
    assert stop.returncode == 0, f"`roostctl session stop` failed: {stop.stdout}{stop.stderr}"


def test_a_project_rows_new_tab_opens_where_the_gesture_would_on_the_session_backend(target):
    """The same case where the tabs live in a `roost-session`, which is
    what a fresh install runs."""
    with _session_backend_ui(target) as roost:
        project = roost.create_project(name=f"pytest-{uuid.uuid4().hex[:8]}", cwd="/tmp")
        other = roost.create_project(name=f"pytest-{uuid.uuid4().hex[:8]}", cwd="/tmp")
        _opens_where_new_tab_does(roost, project, other)


def test_close_tab_closes_it(roost, project):
    _listed_tab(roost, project)
    tab = _listed_tab(roost, project)
    roost.context_menu_activate(tab_target(tab), "close_tab")
    roost.wait_gone(tab)


def test_close_project_asks_first_and_enter_closes_it(roost, project):
    tab = _listed_tab(roost, project)
    _shown(roost, tab)
    roost.context_menu_activate(project_target(project), "close_project")
    try:
        assert roost.project(project) is not None, "the project closed before it was confirmed"
        roost._wait(
            lambda: not roost.app_active_terminal_focused(),
            5.0,
            "the confirm card to own the keyboard",
        )
        roost.key_event("Enter")
        roost._wait(lambda: roost.project(project) is None, 5.0, "Enter to close the project")
    finally:
        if roost.project(project) is not None:
            roost.key_event("Escape")


def test_rename_opens_the_editor_and_escape_hands_the_keyboard_back(roost, project):
    tab = _listed_tab(roost, project)
    _shown(roost, tab)
    roost._wait(roost.app_active_terminal_focused, 5.0, "the terminal to own the keyboard")
    roost.context_menu_activate(tab_target(tab), "rename_tab")
    roost._wait(
        lambda: not roost.app_active_terminal_focused(),
        5.0,
        "the rename editor to take the keyboard",
    )
    roost.key_event("Escape")
    roost._wait(roost.app_active_terminal_focused, 5.0, "Escape to hand it back")


def test_an_item_on_a_closed_tab_is_refused(roost, project):
    """The stale action: the menu is rebuilt from the live rows before an
    item runs, so a tab that closed after its menu was listed refuses
    rather than acting on nothing."""
    _listed_tab(roost, project)
    tab = _listed_tab(roost, project)
    assert item("close_tab", "Close Tab") in roost.context_menu_dump(tab_target(tab))
    roost.close_tab(tab)
    roost.wait_gone(tab)
    _wait_unlisted(roost, tab_target(tab))

    error = refused(roost.context_menu_activate, tab_target(tab), "close_tab")
    assert error.code == "invalid-param", error
    assert "no longer exists" in error.message, error


def test_an_item_not_on_the_rows_menu_is_refused(roost, project):
    tab = _listed_tab(roost, project)
    absent = refused(roost.context_menu_activate, tab_target(tab), "open_project_folder")
    assert absent.code == "invalid-param", absent
    assert "is not on this row's menu" in absent.message, absent

    unknown = refused(roost.context_menu_activate, tab_target(tab), "remove_host")
    assert unknown.code == "invalid-param", unknown
    assert "is not a context-menu action" in unknown.message, unknown
    assert roost.tab(tab) is not None


def _overlay_platform() -> None:
    if sys.platform == "darwin":
        pytest.skip("macOS draws the native popup, which no test op opens")


def test_the_open_menu_walks_with_the_arrow_keys_and_runs_on_enter(roost, project):
    """Four `ArrowDown`s pass Rename…, New Tab Here and Copy Path, step
    over the separator to Close Tab, and `Enter` runs it."""
    _overlay_platform()
    _listed_tab(roost, project)
    tab = _listed_tab(roost, project)
    assert roost.context_menu_dump(tab_target(tab)) == TAB_MENU
    roost.context_menu_open(tab_target(tab))
    for _ in range(4):
        roost.key_event("ArrowDown")
    roost.key_event("Enter")
    roost.wait_gone(tab)


def test_the_open_menu_swallows_every_other_key_and_escape_closes_it(roost, project):
    _overlay_platform()
    tab = _listed_tab(roost, project)
    _shown(roost, tab)
    roost._wait(roost.app_active_terminal_focused, 5.0, "the terminal to own the keyboard")
    drain(roost, tab)

    roost.context_menu_open(tab_target(tab))
    roost.key_event("q")
    roost.key_event("Escape")
    roost._wait(roost.app_active_terminal_focused, 5.0, "Escape to close the menu")
    roost.key_event("z")
    typed = drain_until_match(roost, tab, rb"z")
    assert b"q" not in typed, f"a key typed at the open menu reached the terminal: {typed!r}"


def test_escape_closes_the_menu_each_time_it_opens(roost, project):
    _overlay_platform()
    tab = _listed_tab(roost, project)
    _shown(roost, tab)
    roost._wait(roost.app_active_terminal_focused, 5.0, "the terminal to own the keyboard")
    for showing in ("first", "second"):
        roost.context_menu_open(tab_target(tab))
        assert not roost.app_active_terminal_focused(), f"the {showing} menu took no keyboard"
        roost.key_event("Escape")
        roost._wait(roost.app_active_terminal_focused, 5.0, f"Escape to close the {showing} menu")


def test_open_is_not_supported_on_macos(roost, project):
    if sys.platform != "darwin":
        pytest.skip("Linux draws the menu as an overlay the op opens")
    tab = _listed_tab(roost, project)
    error = refused(roost.context_menu_open, tab_target(tab))
    assert error.code == "not-supported", error
