"""Secure Keyboard Entry — plan 074 §D3, #593.

On macOS the iced UI holds Secure Keyboard Entry while it is the active
app and either the remembered toggle (`macos-secure-keyboard-entry`) is
on, or `macos-auto-secure-input` is on and the active tab is at a password
prompt. `app.secure_input` reports the owner's inputs, what they ask for,
what it holds, and whether the tab band draws the lock.

# What runs where

* **macOS iced:** the App menu's row and its checkmark (`app.menu_dump`);
  one setting behind the menu, the palette and the keybind; the active
  tab's prompt (`test_password_input.py`'s termios helper) moving
  `password_input`, with a background tab's prompt not counting; and, off
  CI, the toggle surviving a relaunch and `macos-secure-input-indication =
  false` hiding the lock.
* **Linux iced:** the feature does not exist, and the action must still be
  harmless: the bound chord is consumed and writes nothing, the palette
  has no row, and `app.secure_input` reads all false at a prompt.
* **The Swift app (`--roost-target mac`)** has none of this; skipped.

Every macOS case asserts the formula against the inputs the op reports,
never an outcome: a bare binary on a CI runner may never be the active
app, and then nothing is ever desired.

The keybind is `fixtures/launcher.conf`'s `ctrl+shift+k`, so no case has
to relaunch to bind it. Condition waits only.
"""

from __future__ import annotations

import os
import sys
import warnings

import pytest

import ui
from client import Roost, RoostError, Timeout
from test_password_input import DONE, READY, SECRET, prompt_argv
from util import (
    BARE_SHELL_ARGV,
    config_value,
    drain,
    drain_until_match,
    skip_on_ci,
    wait_for_config_line,
    wait_tab_attached,
)

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

#: The App menu's title under the iced profile (`test_menu_bar.py`'s `APP`).
APP = "Roost-Iced"
ROW = "Secure Keyboard Entry"
ACTION = "toggle_secure_input"
KEY = "macos-secure-keyboard-entry"
#: `fixtures/launcher.conf`'s binding for the action.
CHORD = ("k", ["ctrl", "shift"])

ALL_FALSE = {
    "desired": False,
    "owned": False,
    "indicator": False,
    "manual": False,
    "auto": False,
    "app_active": False,
    "password_input": False,
}

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="app.secure_input and app.key_event require ROOST_TEST_MODE=1 in the UI's launch env",
)


@pytest.fixture
def iced(target):
    if target != "iced":
        pytest.skip("the Swift app has no Secure Keyboard Entry and no app.secure_input")


@pytest.fixture
def mac(iced):
    if sys.platform != "darwin":
        pytest.skip("Secure Keyboard Entry is macOS-only")


@pytest.fixture
def linux(iced):
    if sys.platform == "darwin":
        pytest.skip("the off-macOS no-op")


@pytest.fixture
def config_path():
    path = ui.owned_session_config_path()
    if path is None:
        pytest.skip("toggling writes config.conf, which needs a harness-owned copy")
    return path


def assert_formula(state: dict, indication: bool = True) -> None:
    want = state["app_active"] and (
        state["manual"] or (state["auto"] and state["password_input"])
    )
    assert state["desired"] is want, f"desired disagrees with its inputs: {state}"
    assert state["owned"] is state["desired"], f"the Enable did not follow desired: {state}"
    assert state["indicator"] is (state["owned"] and indication), (
        f"the lock disagrees with owned under indication={indication}: {state}"
    )


def wait_state(roost: Roost, holds, what: str, timeout: float = 5.0) -> dict:
    """The first `app.secure_input` reading that `holds`."""
    seen: list[dict] = []

    def ready() -> bool:
        seen.append(roost.app_secure_input())
        return bool(holds(seen[-1]))

    try:
        roost._wait(ready, timeout, what)
    except Timeout as error:
        raise AssertionError(f"{error}; the last reading was {seen[-1]}") from error
    return seen[-1]


def activate(roost: Roost) -> None:
    """Ask the window to the front. Best effort: a bare binary on a CI
    runner may never become active, which the formula assertions allow
    for; on a desktop it does, and then every case drives the Carbon
    calls. Which path ran is said in the run's warnings summary, which
    pytest prints without `-s` — the native proof is C11's real-input
    case, so a reader must be able to tell this one did not make it."""
    roost.call("app.activate", {})
    try:
        wait_state(roost, lambda state: state["app_active"], "the app to become active", 2.0)
    except AssertionError:
        warnings.warn(
            "secure_input: app_active stayed false, so this case checked the formula "
            "only and made no EnableSecureEventInput/DisableSecureEventInput call",
            stacklevel=2,
        )
    else:
        print("secure_input: the app is active; the Carbon calls are exercised")


def menu_row(roost: Roost) -> dict:
    """The App menu's Secure Keyboard Entry row, which must sit right after
    Settings…."""
    app = next(menu for menu in roost.app_menu_dump() if menu["title"] == APP)
    items = [item for item in app["items"] if not item["separator"]]
    titles = [item["title"] for item in items]
    settings = titles.index("Settings…")
    assert titles[settings + 1] == ROW, titles
    return items[settings + 1]


def expect_manual(roost: Roost, config, want: bool) -> None:
    """The toggle landed everywhere it shows: the owner, the formula, the
    menu's checkmark and `config.conf`."""
    state = wait_state(roost, lambda state: state["manual"] is want, f"manual == {want}")
    assert_formula(state)
    assert menu_row(roost)["state"] == ("on" if want else "off")
    wait_for_config_line(config, KEY, f"{KEY} = {'true' if want else 'false'}")


def press_chord_into(roost: Roost, tab: int) -> None:
    """Press the bound chord, then an `x` the terminal does take. The `x`
    bounds the wait: keys are handled in order, so a chord that leaked to
    the PTY would be in the capture ahead of it."""
    drain(roost, tab)
    roost.key_event(CHORD[0], CHORD[1])
    roost.key_event("x")
    typed = drain_until_match(roost, tab, rb"x")
    assert typed == b"x", f"the bound chord reached the PTY: {typed!r}"


def shell_tab(roost: Roost, project: int) -> int:
    tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, tab)
    return tab


# ---------------------------------------------------------------------------
# macOS
# ---------------------------------------------------------------------------


def test_the_app_menu_row_follows_settings_and_checks_the_toggle(mac, roost):
    row = menu_row(roost)
    assert row["action"] == ACTION
    assert (row["key_equivalent"], row["modifiers"]) == ("k", ["shift", "ctrl"])
    assert row["enabled"] is True
    state = roost.app_secure_input()
    assert row["state"] == ("on" if state["manual"] else "off")
    assert_formula(state)


def test_the_menu_the_palette_and_the_keybind_flip_one_setting(
    mac, roost, project, palette, config_path
):
    activate(roost)
    tab = shell_tab(roost, project)
    assert roost.app_secure_input()["manual"] is False, "the seed config leaves it off"
    try:
        roost.app_menu_activate([APP, ROW])
        expect_manual(roost, config_path, True)

        palette.palette_open()
        rows = palette.palette_query("secure keyboard")["items"]
        assert [row["title"] for row in rows if row["id"] == ACTION] == [
            "Toggle Secure Keyboard Entry"
        ]
        palette.palette_activate(ACTION)
        expect_manual(roost, config_path, False)

        press_chord_into(roost, tab)
        expect_manual(roost, config_path, True)
    finally:
        palette.palette_dismiss()
        if roost.app_secure_input()["manual"]:
            palette.palette_open()
            palette.palette_activate(ACTION)
            wait_for_config_line(config_path, KEY, f"{KEY} = false")


def test_the_active_tabs_password_prompt_drives_auto(mac, roost, project):
    activate(roost)
    helper = roost.open_tab(project, cwd="/tmp", argv=prompt_argv())
    other = None
    try:
        roost.wait_text(helper, READY)
        roost.wait_password_input(helper, True)
        state = wait_state(
            roost, lambda state: state["password_input"], "the active tab's prompt to count"
        )
        assert (state["auto"], state["manual"]) == (True, False), state
        assert_formula(state)

        other = shell_tab(roost, project)
        state = wait_state(
            roost,
            lambda state: not state["password_input"],
            "a prompt in a background tab to stop counting",
        )
        assert_formula(state)
        assert roost.tab(helper)["password_input"] is True, "the helper left its prompt"

        roost.focus(helper)
        state = wait_state(
            roost, lambda state: state["password_input"], "the prompt to count again"
        )
        assert_formula(state)

        roost.send(helper, SECRET + "\n")
        roost.wait_text(helper, DONE)
        state = wait_state(
            roost,
            lambda state: not state["password_input"],
            "answering the prompt to take it down",
        )
        assert_formula(state)
    finally:
        for tab in (other, helper):
            if tab is not None:
                roost.close_tab(tab)


def test_the_toggle_survives_a_relaunch_and_indication_hides_the_lock(
    mac, roost, target, config_path
):
    """The remembered half, through `config.conf`, and the lock's own key:
    with `macos-secure-input-indication = false` an owned enable draws
    nothing."""
    skip_on_ci(
        "quit + relaunch is unreliable on CI's macOS runner",
        alt_coverage="the config parse tests and expect_manual's config.conf line",
    )
    original = config_path.read_text()
    roost.palette_dismiss()
    roost.palette_open()
    roost.palette_activate(ACTION)
    wait_for_config_line(config_path, KEY, f"{KEY} = true")
    roost.close()
    try:
        ui.quit(target)
        ui.launch(target)
        with Roost(ui.socket_path(target)) as relaunched:
            activate(relaunched)
            state = wait_state(
                relaunched, lambda state: state["manual"], "the toggle to come back on"
            )
            assert_formula(state)
            assert menu_row(relaunched)["state"] == "on"

        config_path.write_text(
            config_path.read_text() + "macos-secure-input-indication = false\n"
        )
        ui.quit(target)
        ui.launch(target)
        with Roost(ui.socket_path(target)) as hidden:
            activate(hidden)
            state = wait_state(hidden, lambda state: state["manual"], "the toggle")
            assert_formula(state, indication=False)
    finally:
        config_path.write_text(original)
        ui.quit(target)
        ui.launch(target)


# ---------------------------------------------------------------------------
# Off macOS
# ---------------------------------------------------------------------------


def test_off_macos_the_owner_reads_all_false_at_a_prompt(linux, roost, project):
    helper = roost.open_tab(project, cwd="/tmp", argv=prompt_argv())
    try:
        roost.wait_text(helper, READY)
        roost.wait_password_input(helper, True)
        assert roost.app_secure_input() == ALL_FALSE
        roost.send(helper, SECRET + "\n")
        roost.wait_text(helper, DONE)
    finally:
        roost.close_tab(helper)


def test_off_macos_the_bound_chord_is_consumed_and_writes_nothing(
    linux, roost, project, palette, config_path
):
    """The action is recognised — the chord never reaches the PTY — and
    does nothing. The config writer is one ordered queue, so once a later
    write has landed, a secure-input write the chord queued would have too."""
    tab = shell_tab(roost, project)
    press_chord_into(roost, tab)
    assert roost.app_secure_input() == ALL_FALSE

    for _ in range(2):
        palette.palette_open()
        palette.palette_activate("toggle_sidebar_agents")
    wait_for_config_line(config_path, "show-sidebar-agents", "show-sidebar-agents = true")
    assert config_value(config_path, KEY) is None, "the off-macOS no-op wrote config.conf"


def test_off_macos_the_palette_has_no_row(linux, palette):
    palette.palette_open()
    assert ACTION not in palette.palette_item_ids(palette.palette_query("secure"))
    with pytest.raises(RoostError):
        palette.palette_activate(ACTION)
