#!/usr/bin/env python3
"""Real-click regression for terminal click-to-focus (GTK / Linux).

Click-to-focus can't be exercised by the IPC e2e suite: `tab.dispatch_
mouse_event` writes mouse-report bytes straight to the PTY and never
enters the `GestureClick` controller that grabs keyboard focus. So this
check drives a *real* pointer press through the GTK gesture stack.

It is self-contained — it spins up its own headless Xvfb + a throwaway
Roost instance and injects clicks with `xdotool` (XTEST) — so it needs
no `/dev/uinput`, no single-monitor setup, and no COSMIC. That makes it
runnable on any box with `Xvfb` + `xdotool` (the on-desktop Wayland
injectors in this directory remain the way to test against a live COSMIC
session). It is NOT yet wired into CI; run it locally:

    make test-click-to-focus            # or:
    python3 tools/input/linux/click_to_focus_check.py

The isolation argument: a click in the terminal *body* has no handler
other than the click-to-focus gesture that would grab focus, so the
positive result pins the gesture specifically. The sequence:

  1. open a tab            -> terminal holds logical focus (baseline)
  2. click the sidebar-toggle button (takes focus, no refocus side
     effect)               -> terminal LOSES focus
  3. click the terminal body -> terminal REGAINS focus  (the gesture)

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
            pid = r.create_project(name="click-focus", cwd="/tmp")
            tab = r.open_tab(pid, cwd="/tmp")
            wait_tab_attached(r, tab)

            # Move focus OFF the terminal first, so the terminal-body
            # click below is a real unfocused->focused transition (not a
            # no-op on an already-focused terminal — the new-tab grab may
            # or may not have landed depending on startup timing). The
            # sidebar-toggle button (header, left edge) takes focus and
            # has no terminal-refocus side effect.
            _click_until(click, (68, 27), r, want=False,
                         what="move focus off the terminal via the toggle button")

            # Click deep in the terminal body (clear of the sidebar). The
            # ONLY thing that grabs focus on a terminal-body click is the
            # click-to-focus gesture under test, so unfocused->focused
            # here pins that gesture specifically.
            m = r.window_metrics()
            sb = int(m.get("sidebar_width", 0) or 0)
            w, h = int(m["window_width"]), int(m["window_height"])
            _click_until(click, (sb + int((w - sb) * 0.6), int(h * 0.55)), r, want=True,
                         what="grab focus by clicking the terminal body (click-to-focus)")
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

    print("PASS: terminal click-to-focus grabs keyboard focus")
    return 0


if __name__ == "__main__":
    sys.exit(main())
