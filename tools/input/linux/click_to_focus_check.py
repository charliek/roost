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
    print(f"SKIP: {msg}")
    sys.exit(0)


def _free_display() -> str:
    for n in range(99, 130):
        if not Path(f"/tmp/.X{n}-lock").exists():
            return f":{n}"
    _skip("no free X display in :99..:129")


def _wait_window_mapped(display: str, timeout: float = 10.0) -> None:
    """Wait until the Roost toplevel is realized + mapped under Xvfb.
    Driving a tab before the window maps lets the new-tab grab_focus
    no-op against an unmapped widget (the same hazard the e2e harness
    avoids with its boot-readiness gate)."""
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
                return
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
    """F2: switching projects by clicking a sidebar row grabs focus on
    the new project's terminal. Real-click only — the IPC switch path
    doesn't focus a sidebar row, so it never reproduced the strand bug
    (focus left on the clicked GtkListBoxRow, cursor hollow)."""
    # Expand the sidebar AND move focus onto the toggle button (off the
    # terminal): a toggle click flips visibility and takes focus, so
    # re-expand if that first click collapsed it, then wait for the
    # expand to settle before reading geometry / clicking a row.
    click(68, 27)
    if r.window_metrics().get("sidebar_collapsed"):
        click(68, 27)
    r._wait(lambda: not r.window_metrics().get("sidebar_collapsed"),
            timeout=4.0, what="sidebar expanded for the project-switch click")
    if r.app_active_terminal_focused():
        raise AssertionError("expected the terminal unfocused after toggling the sidebar")

    # Click sidebar row 0 — the throwaway-state bootstrap project, never
    # the active one here (the click-to-focus check left its own project
    # active), so this is a real project switch. X is derived (mid-
    # sidebar); Y is the first row under the PROJECTS header, a constant
    # for the controlled Xvfb screen + default test theme (the test fails
    # loudly via the timeout below if it drifts). One click is a
    # deterministic switch; poll for the idle-deferred grab to land —
    # without the fix, focus stays on the row and this times out.
    sb = int(r.window_metrics().get("sidebar_width", 0) or 0)
    click(max(10, sb // 2), 100)
    r._wait(lambda: r.app_active_terminal_focused(), timeout=8.0,
            what="terminal focus after switching projects via a sidebar-row click")


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

        r = _connect(lambda: Roost(str(sock)))
        try:
            _wait_window_mapped(display)
            _check_click_to_focus(r, click, wait_tab_attached)
            _check_project_switch_focus(r, click)
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
                except ProcessLookupError:
                    pass
        try:
            xvfb.terminate()
            xvfb.wait(timeout=5)
        except subprocess.TimeoutExpired:
            xvfb.kill()
        shutil.rmtree(run, ignore_errors=True)

    print("PASS: click-to-focus and project-switch both grab terminal focus")
    return 0


if __name__ == "__main__":
    sys.exit(main())
