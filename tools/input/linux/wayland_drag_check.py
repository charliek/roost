#!/usr/bin/env python3
"""Wayland pointer-drag guard for the GTK / Linux UI.

The X11 sibling (`real_input_check.py`) proves the `GtkGestureDrag` reorder
logic works, but on X11 — it can't exercise the GDK-Wayland backend, which is
exactly where the old GtkDnD drag-icon surface aborted the whole process
(`gdksurface-wayland.c:348:frame_callback`). This drives a REAL pointer drag
under a headless WAYLAND compositor so a future reintroduction of a
Wayland-surface-aborting drag path is caught on the primary backend.

Self-contained: launches `cage` (a kiosk wlroots compositor) under
`WLR_BACKENDS=headless` running a throwaway Roost as its single FULLSCREEN
client — so the window fills the output and window-local pixel coordinates
(found by the same in-process screenshot color-scan the X11 harness uses) equal
compositor-absolute coordinates. The drag is injected with the stdlib
`inject_pointer.py` (`/dev/uinput` absolute pointer). For cage to *see* that
uinput device it must run a libinput backend on a seat — see the
`e2e-gtk-wayland-drag` CI job (`seatd` + `WLR_BACKENDS=headless,libinput`).

Assertions, in priority order:
  * HARD  — the process survives each drag, AND no `frame_callback` / abort /
            GDK `CRITICAL` appears in the captured compositor + UI logs.
  * BEST-EFFORT — the reorder actually happens (`project.list` / `tab.list`
            order changes). Synthetic Wayland arming is racy; the
            content-area SIDEBAR drag is the reliable gate (it leads), the
            title-row PILL drag is secondary (the X11 harness already concedes
            this — see real_input_check.py:393).

Exits 0 PASS / 1 FAIL / 0 with SKIP when a dependency is missing, unless
`ROOST_REQUIRE_REAL_INPUT=1` (set in CI) turns a SKIP into a FAIL.

NB: cage is *generic* wlroots Wayland, not cosmic-comp — it guards GTK's generic
GDK-Wayland path (where the crash lived); COSMIC-specific quirks still need a
real COSMIC box (which the developer confirms separately).
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

# Reuse the backend-agnostic helpers from the X11 harness (screenshot +
# color/text element location are driven through roostctl, not X11) so the two
# guards can't drift. Importing is side-effect-free (its main() is __main__-guarded).
_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
from real_input_check import (  # noqa: E402
    ROOST_BIN,
    ROOSTCTL,
    SCALE,
    _active_pill,
    _connect,
    _project_rows,
    _screenshot,
)

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "tools" / "roosttest"))
INJECT = _HERE / "inject_pointer.py"

# A bad-drag string in either log fails the hard gate. `frame_callback` is the
# exact abort; the GTK/GDK CRITICAL/abort markers catch the criticals such a
# crash emits on the way down.
BAD_LOG = re.compile(r"frame_callback|GDK_SURFACE_DESTROYED|Gdk-CRITICAL|"
                     r"Gtk-CRITICAL|assertion|abort", re.IGNORECASE)


def _skip(msg: str) -> NoReturn:
    if os.environ.get("ROOST_REQUIRE_REAL_INPUT") == "1":
        print(f"FAIL (real-input required): {msg}")
        sys.exit(1)
    print(f"SKIP: {msg}")
    sys.exit(0)


def _uinput_drag(w: int, h: int, x0: int, y0: int, x1: int, y1: int,
                 steps: int = 12) -> None:
    """Press-move-release as ONE inject_pointer.py invocation (one uinput device
    session) so the motion is contiguous — a gap between button-down and the
    first motion lets GTK settle the press as a click and the drag never arms.
    Coordinates are output-absolute (== window-local, cage fullscreens us)."""
    ops = [f"move {x0} {y0}", "down LEFT"]
    for i in range(1, steps + 1):
        xx = int(x0 + (x1 - x0) * i / steps)
        yy = int(y0 + (y1 - y0) * i / steps)
        ops += [f"move {xx} {yy}"]
    ops += ["up LEFT"]
    subprocess.run([sys.executable, str(INJECT), str(w), str(h), *ops], check=True)
    time.sleep(0.5)


def _shot_dims(rc, tmp: Path) -> tuple[int, int]:
    """The output (== fullscreen window) size, read from a screenshot — this is
    the WIDTHxHEIGHT inject_pointer.py maps its absolute device onto."""
    shot = tmp / "dims.png"
    _screenshot(rc, shot)
    out = subprocess.run(
        [sys.executable, str(REPO / "tools" / "screenshot" / "pngtool.py"),
         "info", str(shot)], capture_output=True, text=True).stdout.split()
    return int(out[0]), int(out[1])


def _logs_clean(*logs: Path) -> str | None:
    for lp in logs:
        try:
            txt = lp.read_text(errors="replace")
        except OSError:
            continue
        m = BAD_LOG.search(txt)
        if m:
            return f"{lp.name}: {m.group(0)!r}"
    return None


def _check_sidebar_reorder(r, rc, tmp: Path, cage, w: int, h: int, sb: int) -> bool:
    """Reliable gate: drag the top project row down past the others. Returns
    True if the reorder was observed (best-effort), raises on a crash (hard)."""
    for nm in ("Apple", "Banana", "Cherry"):
        r.create_project(name=nm, cwd="/tmp")
    time.sleep(0.5 * SCALE)

    def order():
        return [int(p["id"]) for p in r.list()]

    before = order()
    after = before
    for _ in range(5):
        rows = _project_rows(rc, tmp, sb)
        assert len(rows) >= 3, f"could not locate >=3 project rows (found {rows})"
        x = max(12, sb // 2)
        _uinput_drag(w, h, x, rows[0], x, rows[-1] + 30)
        assert cage.poll() is None, "compositor/app exited during project-row drag"
        after = order()
        if after != before:
            break
    if after == before:
        print("  sidebar reorder: drag did not arm under headless cage "
              "(best-effort; process survived)")
        return False
    assert sorted(after) == sorted(before), f"project set changed: {before} -> {after}"
    print(f"  sidebar reorder OK: {before} -> {after}")
    return True


def _check_tab_reorder(r, rc, tmp: Path, cage, w: int, h: int, wait_tab_attached) -> bool:
    """Secondary: drag the active pill right. Best-effort (title-row race);
    process survival is the hard gate."""
    pid = r.create_project(name="tabdrag", cwd="/tmp")
    ids = []
    for nm in ("alpha", "bravo", "charlie"):
        t = r.open_tab(pid, cwd="/tmp")
        wait_tab_attached(r, t)
        r.set_title(t, nm)
        ids.append(t)
    r.focus(ids[0])
    time.sleep(0.4 * SCALE)

    def order():
        proj = next(p for p in r.list() if int(p["id"]) == int(pid))
        return [int(t["id"]) for t in proj["tabs"]]

    before = order()
    after = before
    for _ in range(5):
        loc = _active_pill(rc, tmp)
        if not loc:
            break
        cx, cy = loc
        _uinput_drag(w, h, cx, cy, cx + 320, cy)
        assert cage.poll() is None, "compositor/app exited during tab drag"
        after = order()
        if after != before:
            break
    if after == before:
        print("  tab reorder: pill drag did not arm under headless cage "
              "(best-effort; sidebar check is the gate)")
        return False
    assert sorted(after) == sorted(before), f"tab set changed: {before} -> {after}"
    print(f"  tab reorder OK: {before} -> {after}")
    return True


def main() -> int:
    if not ROOST_BIN.exists():
        _skip(f"{ROOST_BIN} not built (cargo build -p roost-linux)")
    if shutil.which("cage") is None:
        _skip("cage not installed")
    if not Path("/dev/uinput").exists():
        _skip("/dev/uinput absent (need `modprobe uinput` + a writable node)")

    from client import Roost  # noqa: E402 — path set above
    from util import wait_tab_attached  # noqa: E402

    run = Path(tempfile.mkdtemp(prefix="roost-wldrag-"))
    xdg, state = run / "xdg", run / "state"
    xdg.mkdir(parents=True)
    state.mkdir(parents=True)
    sock = xdg / "roost" / "roost.sock"
    cage_log = run / "cage.log"
    # roost-linux writes $XDG_STATE_HOME/roost/roost.log AND tees to stdout
    # (captured into cage_log below since roost is cage's child); the hard
    # log-clean gate reads both.
    roost_log = state / "roost" / "roost.log"

    env = {
        **os.environ,
        "XDG_RUNTIME_DIR": str(xdg),
        "XDG_STATE_HOME": str(state),
        "ROOST_STATE_DIR": str(state),
        "ROOST_TEST_MODE": "1",
        # Headless wlroots, software render; force the Wayland GDK backend so a
        # missing display can't silently fall back to X11.
        "WLR_BACKENDS": os.environ.get("WLR_BACKENDS", "headless"),
        "WLR_RENDERER": os.environ.get("WLR_RENDERER", "pixman"),
        "GDK_BACKEND": "wayland",
        "GTK_A11Y": "none",
    }
    env.pop("DISPLAY", None)
    env.pop("DBUS_SESSION_BUS_ADDRESS", None)

    cage = None
    try:
        # cage runs Roost as its single fullscreen client and exits when Roost
        # exits, so `cage.poll()` doubles as the app-liveness check.
        cf = cage_log.open("wb")
        cage = subprocess.Popen(
            ["cage", "--", str(ROOST_BIN)], env=env,
            stdout=cf, stderr=subprocess.STDOUT, start_new_session=True,
        )
        rc = [str(ROOSTCTL), "--socket", str(sock)]
        r = _connect(lambda: Roost(str(sock)))
        try:
            time.sleep(2.0 * SCALE)  # let cage map + Roost render its first frame
            if cage.poll() is not None:
                _skip("cage/Roost exited before the drag (compositor setup "
                      "failed — likely no seat/libinput for uinput)")
            w, h = _shot_dims(rc, run)
            sb = int(r.window_metrics().get("sidebar_width", 0) or 0) or 220
            armed_sidebar = _check_sidebar_reorder(r, rc, run, cage, w, h, sb)
            armed_tab = _check_tab_reorder(r, rc, run, cage, w, h, wait_tab_attached)
            assert cage.poll() is None, "compositor/app exited during the checks"
            bad = _logs_clean(roost_log, cage_log)
            assert bad is None, f"bad marker in logs (a Wayland drag abort?): {bad}"
        finally:
            r.close()
    finally:
        if cage is not None:
            try:
                os.killpg(os.getpgid(cage.pid), signal.SIGTERM)
                cage.wait(timeout=5)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                try:
                    os.killpg(os.getpgid(cage.pid), signal.SIGKILL)
                    cage.wait()
                except ProcessLookupError:
                    pass
        shutil.rmtree(run, ignore_errors=True)

    note = ("reorder armed" if (armed_sidebar or armed_tab)
            else "process survived (reorder did not arm — best-effort)")
    print(f"PASS: Wayland pointer-drag guard — no surface abort; {note}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
