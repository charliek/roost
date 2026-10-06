"""Real CGEvent and Accessibility input against a branch Roost-Iced.app (plan 074 §D7, §D8).

The iced UI's test ops (`app.key_event`, `app.context_menu_*`) start after
winit and AppKit have had their say; this module starts before them. Its
events are real: posted by `tools/input/mac/roost-input-mac` through
`tools/input/mac/runner.py` (runner mode through the TCC anchor app on the
harness Mac, direct mode on CI).

**It owns its UI** (`owns_ui`), so conftest's shared launcher never runs for
it. Each test launches the branch's `mac/build/Roost-Iced.app` (or
`ROOST_ICED_APP`) with `open -n` under `ROOST_BUNDLE_PROFILE=linux`: the
`Roost-linux` namespace, apart from an installed Roost-Iced.app and its
`Roost-iced` socket. Each launch gets its own `ROOST_STATE_DIR`,
`ROOST_TEST_MODE=1` and a config seeded with `fixtures/launcher.conf`'s safety
lines (`local-backend = in-process`, so no `roost-session` starts;
`agent-hooks = off`, so nothing is written into a real agent's dotfiles) plus
the test's own keys. The branch build shares the installed app's bundle id, so
the launched app is handled by **pid** only, and only once its environment
proves it is this launch's (`real_input_ui`): frontmost by pid, quit by pid
with SIGTERM — never by bundle id, `osascript` or process name.

`Roost-linux` is one fixed namespace, not a sandbox: a run holds
`/tmp/roost-real-input.lock`, refuses to start while that namespace's socket
answers, creates the namespace's directories itself (each with this run's
marker file), proves the launched pid owns the socket, and copies
`~/Library/Logs/Roost-linux` into the artifacts before cleaning up what it
created, still marked, after owning the namespace throughout, and while holding
the namespace's socket lock: the logs directory goes; the caches directory and
its `roost.lock` stay.
Nothing here asserts on what the profile changes (the app id, the window
title, the App menu's title).

# The scenarios (§D8)

Every test launches its own UI, so each Option-side config, full screen and
each quit-and-relaunch gets a fresh process. `SCENARIOS` lists them, for the
CI promotion rule (§D7), which needs every one in the JUnit report as passed:

1. Option as Meta by side, one launch per `macos-option-as-alt` value: the
   configured side's Option+B sends `ESC b`, the other side types the US
   layout's `∫`.
2. Shift+Enter: `ESC[27;2;13~`, then `ESC[13;2u` once the kitty keyboard
   protocol is pushed; plain Enter is `\\r`.
3. The native right-click popup, on a tab pill (right-click and ctrl-click)
   and a project row: its items match `app.context_menu_dump`, and pressing
   Rename… opens that row's rename editor.
4. Full screen through the green button, out through the View menu item.
5. Secure Keyboard Entry, seen from outside: one listen-only event tap,
   live throughout, sees the keys posted into Roost until the menu row turns
   it on, none while it is on, and the keys again once it is off; a password
   prompt turns it on in auto mode.
6. Selection auto-scroll, a real drag held past the grid's top, and past the
   window's bottom from a paged-up view: the selection pasteboard spans more
   rows than the viewport.
7. The window frame: moved and resized through AX, it comes back after a
   relaunch, and a saved frame off every display opens on one.
8. #189's seed: with SGR mouse tracking on, a real click reports its press
   and release.
9. The helper's foreign key-window guard against a real foreign app
   (`tools/input/mac/fixtures/key_panel.swift`, built per run): a
   non-activating panel that takes the keyboard in front of Roost refuses the
   key, and a background app that claims the keyboard from behind Roost's
   window (what Zed or a second Roost-Iced does, #604) does not. Each asserts
   through the helper's `claimants` that the fixture really is in the shape it
   stands for, so neither passes vacuously.

Pointer positions come from the window's AX frame and `app.window_metrics`
read together (`geometry`), afresh for every gesture: both go stale on any
move, resize or full-screen change. While a native popup is open the UI's
main thread is inside AppKit's menu tracking loop, so nothing here makes an
IPC call between opening one and its closing.

Byte assertions read `tab.capture_pty_input` from a tab running `/bin/cat`,
which keeps a shell's own terminal queries out of what is captured. The
selection is read from the selection pasteboard (`clipboard.dump
selection`), never the general one.

`make e2e-iced-real-input-mac` runs it with `ROOST_REQUIRE_REAL_INPUT=1`, under
which unavailable real input is a failure rather than a skip.
"""

from __future__ import annotations

import contextlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

import pytest
import session
import ui
import util
from client import Roost, scaled_timeout
from real_input_ui import LOGS, Namespace, RealInputUI, platform_mismatch
from test_password_input import DONE, PROMPT_HELPER, READY, SECRET
from test_tab_dump_scrollback import numbered

sys.path.insert(0, str(ui.REPO_ROOT / "tools" / "input" / "mac"))
import coords  # noqa: E402
import runner as real_input  # noqa: E402

pytestmark = pytest.mark.owns_ui

#: Every test in this module, in order. `test_real_input_mac_scenarios`
#: (roosttest_unit) keeps it equal to the functions below.
SCENARIOS = (
    "test_preflight",
    "test_option_as_meta_left",
    "test_option_as_meta_right",
    "test_shift_enter",
    "test_native_context_popup",
    "test_full_screen",
    "test_secure_keyboard_entry_manual",
    "test_secure_keyboard_entry_auto",
    "test_selection_autoscroll_up",
    "test_selection_autoscroll_down",
    "test_window_frame",
    "test_mouse_tracking_click",
    "test_foreign_panel_in_front_blocks_keys",
    "test_background_claimant_does_not_block_keys",
)

# US-layout virtual keycodes (`kVK_*`, HIToolbox Events.h).
KEYCODES = {"a": 0, "z": 6, "x": 7, "b": 11, "q": 12, "y": 16, "j": 38, "k": 40}
KEY_A = KEYCODES["a"]
KEY_B = KEYCODES["b"]
KEY_Q = KEYCODES["q"]
KEY_RETURN = 36
KEY_ESCAPE = 53

ESC_B = b"\x1bb"
INTEGRAL = "∫".encode()

#: `crates/roost-iced/src/input.rs`'s one-time note that Option's side was
#: assumed, and `main.rs`'s first line, which says the log is this launch's.
ASSUMED_SIDE = "no left/right Option key event seen"
STARTUP_LINE = "resolved bundle identity"

# The chrome a click aims at, from `crates/roost-iced/src/chrome.rs` and the
# layout in `app.rs`: the tab band is the `BAND_HEIGHT` above the terminal
# and pads its first pill 8 points in; a pill is at least 80 points wide,
# with an active one's close button in its last 24. The sidebar's first
# project row sits under its PROJECTS band, after the list's 4-point top
# padding.
BAND_HEIGHT = 32.0
TAB_BAR_PADDING_X = 8.0
INTO_THE_PILL = 30.0
PROJECT_LIST_PADDING_TOP = 4.0
ROW_HEIGHT = 32.0

APP_MENU_ROW = "Secure Keyboard Entry"
FULL_SCREEN_ROWS = ("Enter Full Screen", "Exit Full Screen")

SEEDED_LINES = 600
PAGES_UP = 4
#: Where the held pointer goes past the grid, and for how long (§D8 #6).
PAST_THE_EDGE = 40.0
HOLD_MS = 1000
#: How long full screen must hold still before an exit is sent
#: (`settle_in_full_screen`).
FULL_SCREEN_QUIET_S = 2.0

TAP_SECONDS = 60.0
#: How long one AX probe for an open popup looks before it says there is none.
POPUP_PROBE_MS = 300
#: The password-prompt fixture's interpreter. The launched app is its own
#: TCC-responsible process, so a child of it that reads a removable volume —
#: the harness venv's Python, on the harness Mac's external disk — raises a
#: "would like to access files on a removable volume" prompt for the shared
#: bundle id. The system's Python lives on the boot volume.
SYSTEM_PYTHON = "/usr/bin/python3"

KEY_PANEL_SOURCE = ui.REPO_ROOT / "tools" / "input" / "mac" / "fixtures" / "key_panel.swift"
FOREIGN_CLAIM = "holds a key window in front of"


@pytest.fixture(scope="module", autouse=True)
def _macos_iced_only(target):
    if (reason := platform_mismatch(platform.system(), target)) is not None:
        real_input.skip_or_fail(reason)


@pytest.fixture(scope="module")
def artifacts() -> Path:
    base = os.environ.get("ROOST_E2E_ARTIFACT_DIR")
    path = Path(base) if base else Path(tempfile.mkdtemp(prefix="roost-real-input-"))
    path = path.expanduser().resolve()
    path.mkdir(parents=True, exist_ok=True)
    return path


@pytest.fixture(scope="module")
def namespace(artifacts):
    """Hold `Roost-linux` for the module (see `real_input_ui.Namespace`)."""
    held = Namespace(artifacts)
    if (reason := held.acquire()) is not None:
        real_input.skip_or_fail(reason)
    try:
        yield held
    finally:
        held.close()


@pytest.fixture(scope="module")
def helper(namespace, artifacts):
    """The helper, once a preflight says this machine can do real input now
    (`runner.readiness`): every test sits behind that gate."""
    app = _bundle_app()
    with real_input.unavailable_skips():
        handle = real_input.Helper(artifacts)
    try:
        with real_input.unavailable_skips():
            report = handle.require_ready()
        (artifacts / "run.json").write_text(
            json.dumps(
                {"mode": handle.mode, "helper": str(handle.binary), "app": str(app), "preflight": report}
            )
        )
        yield handle
    finally:
        handle.close()


def _bundle_app() -> Path:
    app = ui.iced_bundle_app() or ui.REPO_ROOT / "mac" / "build" / "Roost-Iced.app"
    if not (app / "Contents" / "MacOS" / ui.ICED_BUNDLE_EXECUTABLE_NAME).is_file():
        pytest.fail(
            f"no Roost-Iced bundle at {app}: assemble it with mac/scripts/bundle-iced.sh debug "
            "(make e2e-iced-real-input-mac does)"
        )
    return app.resolve()


@pytest.fixture(scope="module")
def key_panel_binary():
    """`fixtures/key_panel.swift`, built for this run on the boot volume: no
    grant covers it, and nothing else ever runs it."""
    build = Path(tempfile.mkdtemp(prefix="roost-key-panel-", dir="/tmp"))
    binary = build / "key_panel"
    try:
        built = subprocess.run(
            ["swiftc", "-O", "-o", str(binary), str(KEY_PANEL_SOURCE)],
            capture_output=True,
            text=True,
            check=False,
            timeout=300,
        )
        if built.returncode != 0:
            pytest.fail(f"swiftc could not build {KEY_PANEL_SOURCE}: {built.stderr[-2000:]}")
        yield binary
    finally:
        shutil.rmtree(build, ignore_errors=True)


@pytest.fixture
def launch_ui(namespace):
    """Launch a fresh UI with this test's config keys (`launch_ui({"macos-
    option-as-alt": "left"})`); `relaunch()` restarts one on its own state,
    and every UI launched here is quit, and its state removed, at the end."""
    launched: list[RealInputUI] = []

    def launch(config: dict[str, str] | None = None) -> RealInputUI:
        launched.append(RealInputUI(_bundle_app(), config or {}, namespace))
        return launched[-1].launch()

    try:
        yield launch
    finally:
        errors = []
        for running in reversed(launched):
            try:
                running.quit()
            except Exception as error:  # quit them all, then report the first
                errors.append(error)
        if errors:
            raise errors[0]


# ---------------------------------------------------------------------------
# Shared steps
# ---------------------------------------------------------------------------


def settle(read, holds, timeout: float, what: str):
    """Poll `read()` until its value `holds`; the value, or an
    AssertionError naming the last one read."""
    seen = []

    def ready():
        seen[:] = [read()]
        return holds(seen[0])

    try:
        session.wait_until(ready, timeout, what, 0.1)
    except TimeoutError as error:
        raise AssertionError(f"{error}; last read: {seen[0] if seen else None!r}") from error
    return seen[0]


def geometry(roost: RealInputUI, helper) -> tuple[coords.Frame, coords.Metrics]:
    """The window's AX frame and the UI's `app.window_metrics`, once the two
    describe the same window: after a move, resize or full-screen change
    either can lag the other."""

    def read():
        frame = coords.Frame.from_window(helper.window(roost.pid))
        metrics = coords.Metrics.from_window_metrics(roost.client.window_metrics())
        try:
            coords.check(frame, metrics)
        except AssertionError as stale:
            return frame, metrics, str(stale)
        return frame, metrics, None

    frame, metrics, _ = settle(
        read, lambda reading: reading[2] is None, 10, "the AX frame and window_metrics to agree"
    )
    return frame, metrics


def same_frame(a: coords.Frame, b: coords.Frame) -> bool:
    """The same size exactly and the same origin within a point."""
    return (a.width, a.height) == (b.width, b.height) and abs(a.x - b.x) <= 1 and abs(a.y - b.y) <= 1


def cat_tab(roost: RealInputUI, helper) -> int:
    """A `/bin/cat` tab, on screen with the keyboard, and nothing captured yet."""
    tab = roost.open_tab(["/bin/cat"])
    roost.bring_to_front(helper)
    util.drain(roost.client, tab)
    return tab


def typed(roost: RealInputUI, helper, tab: int, code: int, flags=()) -> bytes:
    """What one real chord sends to the tab. A plain `q` follows it and keys
    reach the PTY in order, so once the `q` is in, everything before it is
    the chord's."""
    helper.key(roost.pid, code, flags)
    helper.key(roost.pid, KEY_Q)
    got = util.drain_until_match(roost.client, tab, rb"q\Z")
    return got[:-1]


def require_us_layout(helper) -> None:
    source = helper.preflight()["input_source"]
    if source != real_input.US_INPUT_SOURCE:
        real_input.skip_or_fail(f"the expected bytes are the US layout's; the input source is {source!r}")


def require_no_secure_input(helper) -> int | None:
    """Skip (or fail, when required) unless nobody holds Secure Input: one
    held elsewhere blinds the tap, and `ioreg` names the frontmost app, not
    the holder, so Roost's own could not be told apart from it."""
    holder = helper.preflight()["secure_input_pid"]
    if holder is not None:
        real_input.skip_or_fail(f"Secure Input is already on (ioreg names pid {holder})")
    return holder


# ---------------------------------------------------------------------------
# The scenarios
# ---------------------------------------------------------------------------


def report_displays(helper) -> None:
    """Each display's bounds and top insets. CI's pytest runs without `-s`, so
    a passing test's stdout never reaches the log; the job summary does."""
    lines = [
        f"display {d['id']} main={d['main']} bounds={d['bounds']} "
        f"safe_area_top={d.get('safe_area_top')} menu_bar_inset={d.get('menu_bar_inset')}"
        for d in helper.preflight()["displays"]
    ]
    print("\n".join(lines))
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        try:
            with open(summary, "a", encoding="utf-8") as out:
                out.write("\n".join(f"- {line}" for line in lines) + "\n")
        except OSError as err:
            print(f"GITHUB_STEP_SUMMARY not written: {err}")


def test_preflight(helper, launch_ui):
    """Behind the `helper` fixture's readiness gate (the read-only grant
    checks, an unlocked console, a US input source), one real operation: a
    real key posted into Roost comes back out of `tab.capture_pty_input`."""
    report_displays(helper)
    with real_input.unavailable_skips():
        roost = launch_ui()
        tab = roost.open_tab(["/bin/cat"])
        roost.bring_to_front(helper)
        util.drain(roost.client, tab)
        helper.key(roost.pid, KEY_A)
        got = util.drain_until_match(roost.client, tab, re.escape(b"a"))
        assert got + util.drain(roost.client, tab) == b"a"


def option_as_meta(helper, launch_ui, side: str) -> None:
    """The configured Option sends `ESC b`; the other types `∫`. Roost tells
    the sides apart by the Option key's own flagsChanged, which the helper
    posts with the real side keycode: the bytes are the assertion. The log
    line saying a side was assumed is supporting evidence only, trusted
    absent only once this launch's startup line is in the log."""
    require_us_layout(helper)
    roost = launch_ui({"macos-option-as-alt": side})
    tab = cat_tab(roost, helper)
    other = {"left": "right", "right": "left"}[side]
    assert typed(roost, helper, tab, KEY_B, (f"alt-{side}",)) == ESC_B
    assert typed(roost, helper, tab, KEY_B, (f"alt-{other}",)) == INTEGRAL

    log = LOGS / "roost.log"
    text = log.read_text(errors="replace") if log.is_file() else ""
    start = text.rfind(STARTUP_LINE)
    assert start >= 0, f"{log} has no {STARTUP_LINE!r} line, so its silence proves nothing"
    assert ASSUMED_SIDE not in text[start:], "Roost saw no Option key of its own and assumed a side"


def test_option_as_meta_left(helper, launch_ui):
    option_as_meta(helper, launch_ui, "left")


def test_option_as_meta_right(helper, launch_ui):
    option_as_meta(helper, launch_ui, "right")


def test_shift_enter(helper, launch_ui):
    """Roost's half of the gx Shift+Enter report (§D10): the legacy
    modified-Enter form, then the kitty form once the protocol is pushed."""
    roost = launch_ui()
    tab = cat_tab(roost, helper)
    assert typed(roost, helper, tab, KEY_RETURN, ("shift",)) == b"\x1b[27;2;13~"
    assert typed(roost, helper, tab, KEY_RETURN) == b"\r"
    roost.client.tab_feed_pty_bytes(tab, b"\x1b[>1u")
    assert typed(roost, helper, tab, KEY_RETURN, ("shift",)) == b"\x1b[13;2u"
    assert typed(roost, helper, tab, KEY_RETURN) == b"\r"


def popup_open(helper, pid: int, at) -> bool:
    """Whether AX finds an open popup of `pid` near `at`."""
    try:
        helper.popup(pid, at, wait_ms=POPUP_PROBE_MS)
    except real_input.RealInputError as error:
        if "no open popup menu" in str(error):
            return False
        raise
    return True


def close_popup(helper, pid: int, at) -> None:
    """Return only once no popup is open: while one is, the UI thread is in
    its tracking loop and an IPC call would hang. A pressed item closes its
    menu itself, so Escape goes only to one still open after a short wait —
    sent early, it could land on the editor the item opened instead."""
    try:
        settle(lambda: popup_open(helper, pid, at), lambda open_: not open_, 2, "the popup to close")
    except AssertionError:
        helper.key(pid, KEY_ESCAPE)
        settle(lambda: popup_open(helper, pid, at), lambda open_: not open_, 5, "Escape to close the popup")


def rename_through_popup(
    roost: RealInputUI, helper, click: str, at, target: dict, action: str, name, new_name: str
):
    """Open `target`'s native popup with a real `click` at `at`, check it
    lists what `app.context_menu_dump` does, press `action`'s item, and see
    that row's rename editor — which opens with its draft selected — take
    `new_name` as the row's name."""
    entries = roost.client.context_menu_dump(target)
    want = [(entry["label"], entry["enabled"]) for entry in entries if "action" in entry]
    label = next(entry["label"] for entry in entries if entry.get("action") == action)
    try:
        helper.mouse(click, roost.pid, at)
        popup = helper.popup(roost.pid, at)
        listed = [(item["title"], item["enabled"]) for item in popup["items"] if item.get("title")]
        assert listed == want, f"the {click} popup at {at} is not {target}'s menu"
        helper.press(roost.pid, ["popup", label], at=at)
    finally:
        close_popup(helper, roost.pid, at)
    Roost._wait(lambda: not roost.client.app_active_terminal_focused(), 5, f"{label} to open an editor")
    helper.key(roost.pid, [KEYCODES[char] for char in new_name])
    helper.key(roost.pid, KEY_RETURN)
    settle(name, lambda value: value == new_name, 5, f"the row to be renamed {new_name!r}")
    Roost._wait(roost.client.app_active_terminal_focused, 5, "the terminal to take the keyboard back")


def test_native_context_popup(helper, launch_ui):
    roost = launch_ui()
    project = int(roost.client.list()[0]["id"])
    tab = roost.client.project_tab_ids(project)[0]
    roost.bring_to_front(helper)
    frame, metrics = geometry(roost, helper)
    pill = coords.window_point(
        frame,
        metrics,
        metrics.terminal_left + TAB_BAR_PADDING_X + INTO_THE_PILL,
        metrics.terminal_top - BAND_HEIGHT / 2,
    )
    project_row = coords.window_point(
        frame,
        metrics,
        min(60.0, metrics.terminal_left / 2),
        BAND_HEIGHT + PROJECT_LIST_PADDING_TOP + ROW_HEIGHT / 2,
    )

    def tab_title() -> str:
        return roost.client.tab(tab)["title"]

    def project_name() -> str:
        return roost.client.project(project)["name"]

    tab_target, project_target = {"tab_id": str(tab)}, {"project_id": str(project)}
    rename_through_popup(roost, helper, "right-click", pill, tab_target, "rename_tab", tab_title, "zq")
    rename_through_popup(roost, helper, "ctrl-click", pill, tab_target, "rename_tab", tab_title, "jk")
    rename_through_popup(
        roost, helper, "right-click", project_row, project_target, "rename_project", project_name, "xy"
    )


def full_screen_rows(menus: list[dict]) -> list[tuple[str, str]]:
    """Every (menu, row) titled for full screen, in a menu bar dump."""
    return [
        (menu["title"], item["title"])
        for menu in menus
        for item in menu.get("items") or []
        if item.get("title") in FULL_SCREEN_ROWS
    ]


def settle_in_full_screen(roost: RealInputUI, helper, report: dict) -> None:
    """Wait until the window fills a display in full screen, the UI's content
    fills the window, and both have held still for `FULL_SCREEN_QUIET_S`.

    AXFullScreen turns while the window is still animating into its Space, and
    AppKit drops a toggle sent then — winit records it anyway, so Roost's menu
    reads Enter Full Screen while the window stays full screen. Nothing in AX
    marks the animation's end, so the wait is for quiet: on the harness Mac 9
    of 10 exits sent as AXFullScreen turned were dropped, and 24 of 24 sent
    half a second or more later landed."""
    translated = [d["id"] for d in report["displays"] if d.get("insets") == "unavailable-translated"]
    assert not translated, (
        f"displays {translated} have no inset readings because the helper runs under Rosetta: "
        "build the helper natively (arm64)"
    )
    candidates = coords.full_screen_frames(report)
    quiet = scaled_timeout(FULL_SCREEN_QUIET_S)
    since: list = [None, 0.0]

    def read():
        window = helper.window(roost.pid)
        frame = coords.Frame.from_window(window)
        if window["full_screen"] is not True or frame not in candidates:
            return None
        return frame if roost.client.window_metrics()["window_height"] == frame.height else None

    def held(frame) -> bool:
        now = time.monotonic()
        if frame is None or frame != since[0]:
            since[:] = [frame, now]
            return False
        return now - since[1] >= quiet

    settle(read, held, 10 + FULL_SCREEN_QUIET_S, "the window to settle in full screen")


def test_full_screen(helper, launch_ui):
    """In by the green button, out by Roost's own View menu item — the
    button is an invalid element once the window is full screen (§2.12)."""
    roost = launch_ui()
    roost.bring_to_front(helper)
    before, _ = geometry(roost, helper)
    report = helper.preflight()
    assert full_screen_rows(helper.menu_bar(roost.pid)["menus"]) == [("View", "Enter Full Screen")]

    helper.press(roost.pid, ["window", "AXFullScreenButton"])
    settle_in_full_screen(roost, helper, report)
    settle(
        lambda: full_screen_rows(roost.client.app_menu_dump()),
        lambda rows: rows == [("View", "Exit Full Screen")],
        10,
        "app.menu_dump to offer Exit Full Screen",
    )
    assert full_screen_rows(helper.menu_bar(roost.pid)["menus"]) == [("View", "Exit Full Screen")]
    geometry(roost, helper)

    helper.press(roost.pid, ["menu-bar", "View", "Exit Full Screen"])
    settle(
        lambda: helper.window(roost.pid),
        lambda window: window["full_screen"] is False
        and same_frame(coords.Frame.from_window(window), before),
        15,
        "the window to leave full screen at its old frame",
    )
    settle(
        lambda: full_screen_rows(roost.client.app_menu_dump()),
        lambda rows: rows == [("View", "Enter Full Screen")],
        10,
        "app.menu_dump to offer Enter Full Screen again",
    )
    assert full_screen_rows(helper.menu_bar(roost.pid)["menus"]) == [("View", "Enter Full Screen")]
    geometry(roost, helper)


def secure_state(roost: RealInputUI, holds, what: str) -> dict:
    return settle(roost.client.app_secure_input, holds, 5, what)


def ioreg_holder(helper, held: bool) -> int | None:
    """`ioreg`'s secure-input pid once it says Secure Input is `held`. It
    names the frontmost app, not whoever enabled it, so it corroborates
    `app.secure_input` and proves nothing about ownership on its own."""
    return settle(
        lambda: helper.preflight()["secure_input_pid"],
        lambda pid: (pid is not None) is held,
        10,
        f"ioreg to {'name' if held else 'drop'} a secure-input pid",
    )


def tap_key_downs(tap) -> list[int]:
    """The keycodes of every key-down the tap has logged so far. Its log is
    appended while it runs, so a read can catch a line half-written: that
    read counts as nothing new."""
    try:
        return [event["keycode"] for event in tap.events() if event["type"] == "key_down"]
    except json.JSONDecodeError:
        return []


def type_into(roost: RealInputUI, helper, tab: int, text: str, owned: bool = False) -> list[int]:
    """Post `text` into Roost, and see the tab receive it; its keycodes. The
    helper posts under Secure Input only when told Roost `owned` it."""
    codes = [KEYCODES[char] for char in text]
    helper.key(roost.pid, codes, allow_secure_input=owned)
    got = util.drain_until_match(roost.client, tab, re.escape(text.encode()))
    assert got == text.encode(), f"Roost did not receive {text!r}: {got!r}"
    return codes


def in_order(sub: list[int], seq: list[int]) -> bool:
    """Whether `sub` occurs in `seq` in order, other codes between allowed."""
    rest = iter(seq)
    return all(code in rest for code in sub)


def test_secure_keyboard_entry_manual(helper, launch_ui):
    """Off, on through the App menu's row, off again, under ONE listen-only
    tap — what a keylogger sees. The tap sees the keys typed before Secure
    Input and the keys typed after it, and none typed under it; events reach
    it in order, so once it has the later keys, the blind ones would already
    be there."""
    require_no_secure_input(helper)
    roost = launch_ui()
    tab = cat_tab(roost, helper)
    app_menu = next(
        menu["title"]
        for menu in helper.menu_bar(roost.pid)["menus"]
        if any(item.get("title") == APP_MENU_ROW for item in menu.get("items") or [])
    )
    toggle = ["menu-bar", app_menu, APP_MENU_ROW]
    try:
        assert roost.client.app_secure_input()["manual"] is False
        with helper.event_tap(scaled_timeout(TAP_SECONDS)) as tap:
            before = type_into(roost, helper, tab, "xyz")
            settle(
                lambda: tap_key_downs(tap),
                lambda downs: in_order(before, downs),
                5,
                "the tap to see keys typed with Secure Input off",
            )

            helper.press(roost.pid, toggle)
            state = secure_state(roost, lambda state: state["owned"], "the menu row to take Secure Input")
            assert (state["manual"], state["app_active"], state["indicator"]) == (True, True, True), state
            ioreg_holder(helper, held=True)
            blind = type_into(roost, helper, tab, "jk", owned=True)

            helper.press(roost.pid, toggle)
            state = secure_state(roost, lambda state: not state["owned"], "the menu row to give it back")
            assert (state["manual"], state["indicator"]) == (False, False), state
            ioreg_holder(helper, held=False)
            after = type_into(roost, helper, tab, "qa")
            downs = settle(
                lambda: tap_key_downs(tap),
                lambda downs: in_order(before + after, downs),
                5,
                "the same tap to see keys again once Secure Input is off",
            )
            assert not set(blind) & set(downs), f"the tap saw keys typed under Secure Input: {downs}"
            result = tap.stop()
        assert (result["enabled_at_end"], result["stopped_by"]) == (True, "stop-file"), (
            f"the tap was not live from start to end: {result}"
        )
    except BaseException:
        # Leave the remembered toggle off; quitting releases the rest.
        with contextlib.suppress(Exception):
            if roost.client.app_secure_input()["manual"]:
                roost.client.palette_open()
                roost.client.palette_activate("toggle_secure_input")
        raise


def test_secure_keyboard_entry_auto(helper, launch_ui):
    """A password prompt in the active tab takes Secure Input in auto mode,
    and answering it gives it back."""
    require_no_secure_input(helper)
    roost = launch_ui()
    tab = roost.open_tab([SYSTEM_PYTHON, "-c", PROMPT_HELPER])
    roost.bring_to_front(helper)
    roost.client.wait_text(tab, READY)
    roost.client.wait_password_input(tab, True)
    state = secure_state(roost, lambda state: state["owned"], "the prompt to take Secure Input")
    assert state == {
        "desired": True,
        "owned": True,
        "indicator": True,
        "manual": False,
        "auto": True,
        "app_active": True,
        "password_input": True,
    }
    ioreg_holder(helper, held=True)

    roost.client.send(tab, SECRET + "\n")
    roost.client.wait_text(tab, DONE)
    state = secure_state(roost, lambda state: not state["owned"], "echo coming back to give it back")
    assert (state["password_input"], state["indicator"]) == (False, False), state
    ioreg_holder(helper, held=False)


def drag_and_hold(roost: RealInputUI, helper, press, held) -> None:
    """A real drag from `press` to `held`, held there, then released. The
    hold scales with `ROOST_TEST_TIMEOUT_SCALE`: a loaded UI runs fewer of
    its 50 ms auto-scroll ticks in the same second, and the selection is only
    copied on release."""
    hold_ms = int(scaled_timeout(HOLD_MS))
    helper.mouse("drag", roost.pid, path=[press, held], hold_ms=hold_ms, deadline_ms=20_000 + hold_ms)


class Seeded:
    """A tab at the live bottom of a numbered history many screens long.
    Every row carries this test's own token, so a selection pasteboard left
    by an earlier test or run never reads as this drag's."""

    def __init__(self, roost: RealInputUI, helper):
        self.roost = roost
        self.token = uuid.uuid4().hex[:6]
        self.tab = cat_tab(roost, helper)
        body = "\x1b[2J\x1b[H" + "".join(f"{self.line(i)}\r\n" for i in range(SEEDED_LINES))
        roost.client.tab_feed_pty_bytes(self.tab, body.encode())
        roost.client.wait_text(self.tab, self.line(SEEDED_LINES - 1), timeout=30.0)
        self.last_col = len(self.line(0)) - 1

    def line(self, index: int) -> str:
        return f"row-{index:04d}-{self.token}"

    def selected(self) -> list[int]:
        """The rows on the selection pasteboard, or [] unless all are ours."""
        text = self.roost.client.clipboard_dump("selection") or ""
        rows = [row for row in text.split("\n") if row]
        if not rows or not all(row.startswith("row-") and row.endswith(self.token) for row in rows):
            return []
        return [numbered(row) for row in rows]


def assert_one_run(rows: list[int], viewport_rows: int) -> None:
    assert rows == list(range(rows[0], rows[-1] + 1)), f"not one unbroken run: {rows}"
    assert len(rows) > viewport_rows, f"{len(rows)} rows selected, the viewport holds {viewport_rows}"


def test_selection_autoscroll_up(helper, launch_ui):
    """Pressed on the live bottom row, dragged 40 points above the grid and
    held: on release the selection pasteboard has more rows than fit."""
    roost = launch_ui()
    seeded = Seeded(roost, helper)
    dumped = roost.client.dump(seeded.tab)
    viewport_rows = dumped["rows"]
    anchor = dumped["rows_text"].index(seeded.line(SEEDED_LINES - 1))
    frame, metrics = geometry(roost, helper)
    press = coords.cell_center(frame, metrics, seeded.last_col, anchor)
    left, top = coords.cell_origin(frame, metrics, 0, 0)
    held = (left + metrics.cell_width / 2, top - PAST_THE_EDGE)
    drag_and_hold(roost, helper, press, held)
    rows = settle(
        seeded.selected,
        lambda rows: bool(rows) and rows[-1] == SEEDED_LINES - 1,
        10,
        "the drag's selection on the selection pasteboard",
    )
    assert_one_run(rows, viewport_rows)


def test_selection_autoscroll_down(helper, launch_ui):
    """From a view paged up, pressed on its top row and dragged 40 points
    below the window: the selection runs on past the starting viewport. The
    window is placed first so that there is a display under it."""
    roost = launch_ui()
    display = coords.displays(helper.preflight())[0]
    height = min(560.0, display.height - 200.0)
    helper.window_set(roost.pid, (display.x + 60.0, display.y + 60.0, 860.0, height))
    geometry(roost, helper)
    seeded = Seeded(roost, helper)
    dumped = roost.client.dump(seeded.tab)
    viewport_rows = dumped["rows"]
    live_top = numbered(dumped["rows_text"][0])
    for _ in range(PAGES_UP):
        roost.client.key_event("PageUp")
    top = live_top - PAGES_UP * viewport_rows
    settle(
        lambda: numbered(roost.client.dump(seeded.tab)["rows_text"][0]),
        lambda row: row == top,
        10,
        "the page-ups",
    )

    frame, metrics = geometry(roost, helper)
    below = frame.y + frame.height + PAST_THE_EDGE
    assert any(screen.contains(frame.x, below) for screen in coords.displays(helper.preflight())), (
        f"no display under the window's bottom edge at y={below}"
    )
    press = coords.cell_center(frame, metrics, 0, 0)
    held = (coords.cell_center(frame, metrics, seeded.last_col, 0)[0], below)
    drag_and_hold(roost, helper, press, held)
    rows = settle(
        seeded.selected,
        lambda rows: bool(rows) and rows[0] == top,
        10,
        "the drag's selection on the selection pasteboard",
    )
    assert_one_run(rows, viewport_rows)


def saved_frame(roost: RealInputUI) -> dict | None:
    try:
        return json.loads((roost.state_dir / "state.json").read_text()).get("window")
    except FileNotFoundError:
        return None


def test_window_frame(helper, launch_ui):
    """Moved and resized through AX, the frame is saved, and a relaunch on
    the same state opens it there. A saved frame off every display opens on
    one. On a single display AppKit already pulls such a window back on
    screen when it is shown, so that half pins what the user sees, not
    `macos/window_frame.rs`'s own screen check: with the check disabled, every
    off-screen, partly off-screen and oversized frame tried on the harness Mac
    still opened on its display. The check's own rules are
    `app::window_frame`'s `fit_on_screens` unit tests."""
    roost = launch_ui()
    screens = coords.displays(helper.preflight())
    display = screens[0]
    wanted = coords.Frame(display.x + 80.0, display.y + 90.0, 860.0, min(560.0, display.height - 200.0))
    helper.window_set(roost.pid, (wanted.x, wanted.y, wanted.width, wanted.height))
    frame, metrics = geometry(roost, helper)
    assert same_frame(frame, wanted), f"window-set asked for {wanted}, AX reads {frame}"
    settle(
        lambda: saved_frame(roost),
        lambda saved: saved is not None
        and abs(saved["content_width"] - metrics.window_width) < 0.5
        and abs(saved["content_height"] - metrics.window_height) < 0.5,
        10,
        "state.json to save the new size",
    )

    roost.relaunch()
    settle(
        lambda: coords.Frame.from_window(helper.window(roost.pid)),
        lambda reopened: same_frame(reopened, frame),
        10,
        f"the relaunched window at {frame}",
    )

    roost.stop()
    state = json.loads((roost.state_dir / "state.json").read_text())
    off_right = max(screen.x + screen.width for screen in screens) + 2000.0
    state["window"]["outer_x"] = off_right
    (roost.state_dir / "state.json").write_text(json.dumps(state))
    roost.launch()
    reopened = settle(
        lambda: coords.Frame.from_window(helper.window(roost.pid)),
        lambda reopened: coords.on_one_display(reopened, screens),
        10,
        f"a window saved at x={off_right} to open on a display {screens}",
    )
    assert (reopened.width, reopened.height) == (frame.width, frame.height), (
        "the frame fits its display, so only its origin may move"
    )


def test_mouse_tracking_click(helper, launch_ui):
    """#189's seed: with SGR mouse tracking on, a real click on a cell is
    reported as that cell's press and release."""
    roost = launch_ui()
    tab = cat_tab(roost, helper)
    roost.client.tab_feed_pty_bytes(tab, b"\x1b[?1000h\x1b[?1006h")
    util.drain(roost.client, tab)
    frame, metrics = geometry(roost, helper)
    col, row = 5, 3
    helper.mouse("click", roost.pid, coords.cell_center(frame, metrics, col, row))
    want = f"\x1b[<0;{col + 1};{row + 1}M\x1b[<0;{col + 1};{row + 1}m".encode()
    got = util.drain_until_match(roost.client, tab, re.escape(want))
    assert got + util.drain(roost.client, tab) == want


class KeyPanel:
    """A running `key_panel` fixture, launched by this process and quit by pid."""

    def __init__(self, binary: Path, artifacts: Path, *args: str):
        self.out = artifacts / f"key_panel-{args[0]}-{uuid.uuid4().hex[:8]}.out"
        with open(self.out, "wb") as sink:
            self._proc = subprocess.Popen(
                [str(binary), *args],
                stdin=subprocess.DEVNULL,
                stdout=sink,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        self.pid = self._proc.pid

    def __enter__(self) -> "KeyPanel":
        try:
            self.wait_for(f"ready {self.pid}")
        except BaseException:
            self.quit()
            raise
        return self

    def __exit__(self, *exc) -> None:
        self.quit()

    def said(self, prefix: str) -> list[str]:
        return [line for line in self.out.read_text().splitlines() if line.startswith(prefix)]

    def wait_for(self, line: str) -> None:
        def said() -> bool:
            if self._proc.poll() is not None:
                raise AssertionError(f"key_panel exited {self._proc.returncode}: {self.out.read_text()!r}")
            return bool(self.said(line))

        settle(said, bool, 10, f"key_panel to say {line!r}")

    def quit(self) -> None:
        if self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._proc.kill()
                self._proc.wait(timeout=5)


def fixture_claim(helper, roost: RealInputUI, fixture: KeyPanel, holds, what: str) -> dict:
    """The fixture's `claimants` row against Roost's window, once `holds` it;
    an AssertionError naming the whole report otherwise."""
    report = None

    def fixture_row():
        nonlocal report
        report = helper.claimants(roost.pid)
        return next((row for row in report["claimants"] if row["pid"] == fixture.pid), None)

    try:
        return settle(fixture_row, lambda found: found is not None and holds(found), 5, what)
    except AssertionError as error:
        raise AssertionError(f"{error}; claimants: {json.dumps(report)}") from error


def test_foreign_panel_in_front_blocks_keys(helper, launch_ui, key_panel_binary, artifacts):
    """The negative control: with Roost still the front process, a
    non-activating panel takes the keyboard in front of it, and the helper
    refuses the key over that claim; nothing reaches Roost or the panel, and
    once the panel is gone the same key reaches Roost."""
    roost = launch_ui()
    tab = roost.open_tab(["/bin/cat"])
    trigger = artifacts / f"key_panel-{uuid.uuid4().hex[:8]}.trigger"
    try:
        with KeyPanel(key_panel_binary, artifacts, "panel", str(trigger)) as panel:
            roost.bring_to_front(helper)
            util.drain(roost.client, tab)
            trigger.touch()
            panel.wait_for("key ")
            fixture_claim(
                helper,
                roost,
                panel,
                lambda row: row["claims_key"] and row["position"] == "ahead",
                "the panel to claim the keyboard from a window located in front of Roost",
            )
            assert helper.preflight(pid=roost.pid)["target"]["frontmost"], (
                "Roost is not the front process with the panel up, so a refusal would prove nothing"
            )
            with pytest.raises(real_input.RealInputRefused, match=FOREIGN_CLAIM) as refused:
                helper.key(roost.pid, KEY_A)
            # What the helper released on its way out, not the panel's event
            # loop, says no key-down went out ahead of a refused key-up.
            assert refused.value.released == [], str(refused.value)
            assert util.drain(roost.client, tab) == b""
    finally:
        trigger.unlink(missing_ok=True)
    assert panel.said("text ") == [], "a key reached the panel"
    roost.bring_to_front(helper)
    assert typed(roost, helper, tab, KEY_A) == b"a"


def test_background_claimant_does_not_block_keys(helper, launch_ui, key_panel_binary, artifacts):
    """#604: an app that claims the keyboard from a window behind Roost's
    (a background Zed or a second Roost-Iced does) takes none of it, so the
    key goes through."""
    roost = launch_ui()
    tab = roost.open_tab(["/bin/cat"])
    with KeyPanel(key_panel_binary, artifacts, "claim") as claimant:
        fixture_claim(
            helper, roost, claimant, lambda row: row["claims_key"], "the fixture's window to claim the keyboard"
        )
        roost.bring_to_front(helper)
        util.drain(roost.client, tab)
        fixture_claim(
            helper,
            roost,
            claimant,
            lambda row: row["claims_key"] and row["position"] == "behind",
            "the fixture to claim the keyboard from behind Roost; without both, "
            "the fixture no longer reproduces #604",
        )
        assert typed(roost, helper, tab, KEY_A) == b"a"
