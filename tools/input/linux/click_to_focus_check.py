#!/usr/bin/env python3
"""Real-click regression for terminal keyboard focus (GTK / Linux).

Covers the two focus behaviors that only a *real* pointer press can
exercise — the IPC e2e suite can't reach them because
`tab.dispatch_mouse_event` writes mouse-report bytes straight to the PTY
and never enters the GTK gesture stack, and the IPC project switch never
focuses a sidebar row:

  * click-to-focus — clicking the terminal body grabs keyboard focus.
  * project-switch focus — clicking a sidebar project row focuses the
    new project's terminal (without the idle-deferred grab, focus stays
    on the clicked GtkListBoxRow and the cursor goes hollow).

It is self-contained — it spins up its own headless Xvfb + a throwaway
Roost instance and injects clicks with `xdotool` (XTEST) — so it needs
no `/dev/uinput`, no single-monitor setup, and no COSMIC. That makes it
runnable on any box with `Xvfb` + `xdotool` (the on-desktop Wayland
injectors in this directory remain the way to test against a live COSMIC
session). It is NOT yet wired into CI; run it locally:

    make test-click-to-focus            # or:
    python3 tools/input/linux/click_to_focus_check.py

The isolation argument for each check: the target click has no handler
other than the focus path under test, and a non-terminal click defocuses
first, so the unfocused->focused transition pins that path specifically.

Focus is read via the `app.active_terminal_focused` IPC op (GTK logical
focus, observable without a window manager). Exits 0 on PASS, 1 on FAIL,
0 with a SKIP message when Xvfb/xdotool/the binary are unavailable.
"""

from __future__ import annotations

import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import NoReturn


def _xenv(display: str) -> dict:
    """Child env for the X tools: inherit PATH etc., override DISPLAY.
    A bare {"DISPLAY": ...} would drop PATH and only work by falling back
    to os.defpath for the executable lookup."""
    return {**os.environ, "DISPLAY": display}

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "tools" / "roosttest"))


def _skip(msg: str) -> NoReturn:
    # In CI (ROOST_REQUIRE_REAL_INPUT=1) Xvfb/xdotool/the binary are all
    # present, so a "skip" means a real setup failure, not an unsupported
    # environment — surface it as a failure rather than a silent pass.
    if os.environ.get("ROOST_REQUIRE_REAL_INPUT") == "1":
        print(f"FAIL (real-input required): {msg}")
        sys.exit(1)
    print(f"SKIP: {msg}")
    sys.exit(0)


def _free_display() -> str:
    for n in range(99, 130):
        if not Path(f"/tmp/.X{n}-lock").exists():
            return f":{n}"
    _skip("no free X display in :99..:129")


def _wait_window_mapped(display: str, timeout: float = 10.0) -> str:
    """Wait until the Roost toplevel is realized + mapped under Xvfb, and
    return its X window id. Driving a tab before the window maps lets the
    new-tab grab_focus no-op against an unmapped widget (the same hazard
    the e2e harness avoids with its boot-readiness gate)."""
    env = _xenv(display)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = subprocess.run(
            ["xdotool", "search", "--name", "Roost"],
            env=env, capture_output=True, text=True,
        )
        for wid in found.stdout.split():
            geo = subprocess.run(
                ["xdotool", "getwindowgeometry", wid],
                env=env, capture_output=True, text=True,
            )
            m = re.search(r"Geometry:\s*(\d+)x(\d+)", geo.stdout)
            if m and int(m.group(2)) > 200:
                return wid
        time.sleep(0.2)
    raise TimeoutError("roost window never mapped under Xvfb")


def _click_until(click, coord, r, want: bool, what: str, timeout: float = 8.0) -> None:
    """Click `coord` until the active-terminal focus reaches `want`,
    re-clicking each iteration. Tolerates the ~1s startup window where a
    grab or click may not register yet, without a fixed sleep. Returns
    immediately (no click) if focus already matches."""
    deadline = time.monotonic() + timeout
    while True:
        if r.app_active_terminal_focused() == want:
            return
        if time.monotonic() > deadline:
            raise AssertionError(f"{what} (focus never became {want})")
        click(*coord)


def _connect(make_client, timeout: float = 15.0):
    """Retry until the listener accepts: the socket file appears at
    bind() but a connect can still be refused until the accept loop is
    up, so poll the connection itself rather than just the file."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            return make_client()
        except (ConnectionRefusedError, FileNotFoundError) as e:
            last = e
            time.sleep(0.1)
    raise TimeoutError(f"could not connect to roost socket: {last}")


def _check_click_to_focus(r, click, wait_tab_attached) -> None:
    """F1: clicking the terminal body grabs keyboard focus."""
    pid = r.create_project(name="click-focus", cwd="/tmp")
    tab = r.open_tab(pid, cwd="/tmp")
    wait_tab_attached(r, tab)

    # Move focus OFF the terminal first, so the terminal-body click below
    # is a real unfocused->focused transition (not a no-op on an already-
    # focused terminal — the new-tab grab may or may not have landed
    # depending on startup timing). The sidebar-toggle button (header,
    # left edge) takes focus and has no terminal-refocus side effect.
    _click_until(click, (68, 27), r, want=False,
                 what="move focus off the terminal via the toggle button")

    # Click deep in the terminal body (clear of the sidebar). The ONLY
    # thing that grabs focus on a terminal-body click is the click-to-
    # focus gesture under test, so unfocused->focused pins it specifically.
    m = r.window_metrics()
    sb = int(m.get("sidebar_width", 0) or 0)
    w, h = int(m["window_width"]), int(m["window_height"])
    _click_until(click, (sb + int((w - sb) * 0.6), int(h * 0.55)), r, want=True,
                 what="grab focus by clicking the terminal body (click-to-focus)")


def _check_project_switch_focus(r, click) -> None:
    """F2 + #1: clicking a sidebar project row grabs focus on the new
    project's terminal (F2) AND syncs the workspace core's active
    selection — what identify / persistence / notification routing read
    (#1). Real-click only: the IPC switch path goes through the core, so
    it can reproduce neither the focus strand nor the core desync."""
    # Row 0 is the throwaway-state bootstrap project (list() is creation
    # order == sidebar order here). Establish a *different* active project
    # in both core and UI via the IPC focus path (which routes through the
    # core), so the row-0 click below is a genuine switch and the core
    # starts off row 0.
    projects = r.list()
    row0 = int(projects[0]["id"])
    other = next(p for p in projects if int(p["id"]) != row0)
    r.focus(int(other["tabs"][0]["id"]))
    r._wait(lambda: r.identify()["active_project_id"] == int(other["id"]),
            timeout=4.0, what="baseline: a non-row-0 project active in the core")

    # Move focus off the terminal via the sidebar-toggle button (it takes
    # focus and flips sidebar visibility). Poll-click so a dropped XTEST
    # click is retried, then ensure the sidebar ends expanded for the row
    # click (re-expanding keeps focus on the button, not the terminal).
    _click_until(click, (68, 27), r, want=False,
                 what="move focus off the terminal via the sidebar toggle")
    if r.window_metrics().get("sidebar_collapsed"):
        click(68, 27)
    r._wait(lambda: not r.window_metrics().get("sidebar_collapsed"),
            timeout=4.0, what="sidebar expanded for the project-switch click")

    # Click sidebar row 0. X is derived (mid-sidebar); Y is the first row
    # under the PROJECTS header, a constant for the controlled Xvfb screen
    # + default test theme (the test fails loudly via the timeouts below
    # if it drifts). One click is a deterministic switch to row 0.
    sb = int(r.window_metrics().get("sidebar_width", 0) or 0)
    click(max(10, sb // 2), 100)

    # F2: the idle-deferred grab lands focus on the new project's terminal.
    r._wait(lambda: r.app_active_terminal_focused(), timeout=8.0,
            what="terminal focus after switching projects via a sidebar-row click")
    # #1: the click must also sync the core's active selection, not just
    # the UI. Without the core-sync this stays on the previous project.
    r._wait(lambda: r.identify()["active_project_id"] == row0, timeout=4.0,
            what="core active project to track the sidebar-row click (#1 core-sync)")


def _check_alt_digit_switches_project_not_tab(r, send_key, wait_tab_attached) -> None:
    """Alt+digit must switch PROJECTS only — never tabs (Linux).

    AdwTabView's built-in Alt+1..9 / Alt+0 tab shortcuts collide with our
    Linux Alt+digit = SwitchProject. With the row-0 project already active
    (SwitchProject a no-op), the collision flips the tab and the core
    desyncs within a few presses. Real-input only — the IPC path can't
    reproduce the GTK shortcut-manager race.
    """
    projects = r.list()
    row0 = int(projects[0]["id"])
    row1 = int(projects[1]["id"])  # creation order == sidebar order here
    # Row-0 project needs >=2 tabs; add a 2nd and select it, with row-0 the
    # active project (via the core path, which is reliable).
    t2 = r.open_tab(row0, cwd="/tmp")
    wait_tab_attached(r, t2)
    r.focus(t2)
    r._wait(lambda: (idy := r.identify())["active_tab_id"] == t2
            and idy["active_project_id"] == row0,
            timeout=4.0, what="row-0 active with its 2nd tab selected")

    # Alt+1 targets sidebar row 0 = already active, so SwitchProject is a
    # no-op; the tab/project must NOT move. Press enough times to clear the
    # pre-fix non-determinism (the collision manifested by ~press 3).
    for i in range(8):
        send_key("alt+1")
        idy = r.identify()
        assert idy["active_tab_id"] == t2, \
            f"Alt+1 #{i+1} changed the active tab (AdwTabView Alt+digit collision)"
        assert idy["active_project_id"] == row0, \
            f"Alt+1 #{i+1} changed the active project"

    # Alt+2 must still drive SwitchProject — switch to the 2nd project.
    send_key("alt+2")
    r._wait(lambda: r.identify()["active_project_id"] == row1, timeout=4.0,
            what="Alt+2 switches to the 2nd project (SwitchProject still works)")


def _two_tabs_first_selected(r, wait_tab_attached):
    """Open two tabs in the active project and leave the FIRST selected +
    core-active. Returns (pid, t1, t2). Shared setup for the tab-switch
    gesture checks below."""
    pid = r.identify()["active_project_id"]
    t1 = r.open_tab(pid, cwd="/tmp")
    wait_tab_attached(r, t1)
    t2 = r.open_tab(pid, cwd="/tmp")
    wait_tab_attached(r, t2)
    r.focus(t1)
    r._wait(lambda: r.identify()["active_tab_id"] == t1 and r.app_selected_tab_id() == t1,
            timeout=4.0, what="first tab active + displayed before the gesture")
    return pid, t1, t2


def _check_ctrl_pagedown_syncs_core(r, send_key, wait_tab_attached) -> None:
    """#229: AdwTabView's built-in Ctrl+PageDown tab nav must sync the
    workspace core, not just the on-screen selection. Real-input only: the
    shortcut lives in the GTK shortcut stack, unreachable over IPC."""
    _pid, t1, _t2 = _two_tabs_first_selected(r, wait_tab_attached)
    send_key("ctrl+Page_Down")
    # Core must follow the displayed tab AND have actually moved off t1.
    r._wait(lambda: (a := r.identify()["active_tab_id"]) == r.app_selected_tab_id()
            and a != t1,
            timeout=6.0, what="Ctrl+PageDown syncs the core to the next tab (#229)")


def _check_cycle_tab_syncs_core(r, send_key, wait_tab_attached) -> None:
    """cycle_tab (Alt+Shift+]) must sync the core too (it set the selection
    without syncing before this fix). Keybind action; real-input only."""
    _pid, t1, _t2 = _two_tabs_first_selected(r, wait_tab_attached)
    send_key("alt+shift+bracketright")
    r._wait(lambda: (a := r.identify()["active_tab_id"]) == r.app_selected_tab_id()
            and a != t1,
            timeout=6.0, what="cycle_tab (Alt+Shift+]) syncs the core to the next tab")


def _check_pill_click_syncs_core(r, click, wait_tab_attached) -> None:
    """#228: clicking a tab pill must sync the workspace core to that tab.
    Real-input only: tab.dispatch_mouse_event writes PTY bytes and never
    enters the GTK gesture stack, and the IPC switch routes through the core
    so it can't reproduce the desync."""
    pid = r.identify()["active_project_id"]
    t1 = r.open_tab(pid, cwd="/tmp")
    wait_tab_attached(r, t1)
    t2 = r.open_tab(pid, cwd="/tmp")
    wait_tab_attached(r, t2)
    # Newest tab (t2) is selected; click t1's pill (leftmost in the strip).
    r._wait(lambda: r.app_selected_tab_id() == t2, timeout=4.0,
            what="newest tab displayed before the pill click")
    sb = int(r.window_metrics().get("sidebar_width", 0) or 0)
    # The AdwTabBar pill strip sits just below the headerbar (~Y=73 on the
    # controlled Xvfb screen + default test theme); the first pill starts
    # just right of the sidebar, so a point ~60px in lands inside it for
    # both the expanded and packed pill layouts. The timeout fails loudly
    # if these drift. Poll-click so a dropped XTEST click is retried.
    deadline = time.monotonic() + 8.0
    while True:
        if r.app_selected_tab_id() == t1 and r.identify()["active_tab_id"] == t1:
            return
        if time.monotonic() > deadline:
            raise AssertionError("pill click did not switch+sync to the first tab (#228)")
        click(sb + 60, 73)


def main() -> int:
    roost_bin = REPO / "target" / "debug" / "roost"
    if not roost_bin.exists():
        _skip(f"{roost_bin} not built (cargo build -p roost-linux)")
    for tool in ("Xvfb", "xdotool"):
        if shutil.which(tool) is None:
            _skip(f"{tool} not installed")

    from client import Roost  # noqa: E402 — path set above
    from util import wait_tab_attached  # noqa: E402

    display = _free_display()
    run = Path(tempfile.mkdtemp(prefix="roost-click-focus-"))
    xdg, state = run / "xdg", run / "state"
    xdg.mkdir(parents=True)
    state.mkdir(parents=True)
    sock = xdg / "roost" / "roost.sock"

    xvfb = subprocess.Popen(
        ["Xvfb", display, "-screen", "0", "1400x1000x24"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    roost = None
    try:
        time.sleep(1.0)
        env = {
            **os.environ, "DISPLAY": display,
            "XDG_RUNTIME_DIR": str(xdg), "XDG_STATE_HOME": str(state),
            # Throwaway state.json so the project list is deterministic
            # (a fresh bootstrap project at sidebar row 0), independent of
            # the developer's real workspace.
            "ROOST_STATE_DIR": str(state),
            "ROOST_TEST_MODE": "1",
        }
        # Roost is a unique GApplication (registers its app-id on the
        # session bus), so a second instance on the developer's bus would
        # forward-and-exit instead of starting. Drop the bus address: with
        # no session bus the app can't find siblings and runs as its own
        # standalone primary — and it won't touch the developer's session
        # or autostart portals (which FUSE-mount under XDG_RUNTIME_DIR).
        env.pop("DBUS_SESSION_BUS_ADDRESS", None)
        roost = subprocess.Popen(
            [str(roost_bin)], env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        def click(x: int, y: int) -> None:
            subprocess.run(
                ["xdotool", "mousemove", str(x), str(y), "click", "1"],
                env=_xenv(display), check=True,
            )
            time.sleep(0.4)

        def send_key(combo: str) -> None:
            # No WM under Xvfb, so set X input focus on the Roost window
            # explicitly before injecting. --clearmodifiers releases any
            # held modifier so rapid Alt+N presses don't stick together.
            subprocess.run(["xdotool", "windowfocus", wid],
                           env=_xenv(display), check=False)
            subprocess.run(["xdotool", "key", "--clearmodifiers", combo],
                           env=_xenv(display), check=False)
            time.sleep(0.35)

        r = _connect(lambda: Roost(str(sock)))
        try:
            wid = _wait_window_mapped(display)
            _check_click_to_focus(r, click, wait_tab_attached)
            _check_project_switch_focus(r, click)
            _check_alt_digit_switches_project_not_tab(r, send_key, wait_tab_attached)
            _check_ctrl_pagedown_syncs_core(r, send_key, wait_tab_attached)
            _check_cycle_tab_syncs_core(r, send_key, wait_tab_attached)
            _check_pill_click_syncs_core(r, click, wait_tab_attached)
        finally:
            r.close()
    finally:
        # Escalate to SIGKILL if a process ignores SIGTERM, and reap it,
        # before removing the runtime dir — otherwise a lingering child
        # can race the rmtree against still-open sockets/files.
        if roost is not None:
            try:
                os.killpg(os.getpgid(roost.pid), signal.SIGTERM)
                roost.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(roost.pid), signal.SIGKILL)
                    roost.wait()
                except ProcessLookupError:
                    pass
        try:
            xvfb.terminate()
            xvfb.wait(timeout=5)
        except subprocess.TimeoutExpired:
            xvfb.kill()
            xvfb.wait()
        shutil.rmtree(run, ignore_errors=True)

    print("PASS: click-to-focus, project-switch focus, Alt+digit project-only "
          "switching, and tab-switch core-sync (pill click / Ctrl+PageDown / "
          "cycle_tab) all verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
