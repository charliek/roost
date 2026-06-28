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
  * HARD  — the process survives each drag with no `frame_callback` /
            surface-destroyed / GDK critical in the captured compositor + UI
            logs, AND the content-area SIDEBAR drag actually reorders. The
            sidebar reorder is a hard gate (mirroring the X11 sibling): if it
            no-ops, the injected uinput pointer never reached the compositor, so
            the test exercised NOTHING and must fail rather than greenwash —
            this is what stops a vacuous PASS when seat/libinput setup silently
            drops the device.
  * BEST-EFFORT — the title-row PILL drag reorders. Its press races the
            window-move so synthetic arming is racy; the X11 harness concedes
            the same (real_input_check.py:393). Best-effort, never fails.

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
import time
from pathlib import Path

# Reuse the X11 harness's helpers (screenshot + color/text element location are
# driven through roostctl, not X11; `_skip` is the shared SKIP-vs-FAIL contract)
# so the two guards can't drift. Importing is side-effect-free (its main() is
# __main__-guarded) and it puts tools/roosttest on sys.path for client/util.
_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
from real_input_check import (  # noqa: E402
    ROOST_BIN,
    ROOSTCTL,
    SCALE,
    _active_pill,
    _connect,
    _project_rows,
    _skip,
)

INJECT = _HERE / "inject_pointer.py"

# A bad-drag marker in either log fails the hard gate. These are the exact
# tokens the crash (and the criticals it emits on the way down) print — kept
# anchored rather than matching bare "abort"/"assertion", which a benign log
# line could contain.
BAD_LOG = re.compile(r"frame_callback|GDK_SURFACE_DESTROYED|G[dt]k-CRITICAL|Gdk-ERROR")


def _uinput_drag(w: int, h: int, x0: int, y0: int, x1: int, y1: int,
                 steps: int = 12) -> None:
    """Press-move-release as ONE inject_pointer.py invocation (one uinput device
    session) so the motion is contiguous — a gap between button-down and the
    first motion lets GTK settle the press as a click and the drag never arms.
    Coordinates are output-absolute (== window-local, cage fullscreens us)."""
    ops = [f"move {x0} {y0}", "down LEFT"]
    for i in range(1, steps + 1):
        ops.append(f"move {int(x0 + (x1 - x0) * i / steps)} "
                   f"{int(y0 + (y1 - y0) * i / steps)}")
    ops.append("up LEFT")
    subprocess.run([sys.executable, str(INJECT), str(w), str(h), *ops], check=True)
    time.sleep(0.5)


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


def _drag_until_reorder(order, do_drag, proc, label: str, *,
                        hard: bool, attempts: int = 5) -> bool:
    """Retry one drag until the order changes. Process survival each attempt is
    always a HARD gate (raises on a crash). The reorder itself is HARD when
    `hard=True` (the reliable content-area sidebar drag — a no-op there means the
    injected input never reached the compositor, so the test did NOTHING and
    must fail rather than greenwash) and BEST-EFFORT otherwise (the title-row
    pill drag, whose press races the window-move — the X11 sibling concedes the
    same). The drag coords are stable across a no-op attempt — the layout only
    moves once the drag arms, which breaks the loop — so the caller locates the
    element ONCE before calling. Returns True if the reorder was observed."""
    before = order()
    after = before
    for _ in range(attempts):
        do_drag()
        assert proc.poll() is None, f"compositor/app exited during {label} drag"
        after = order()
        if after != before:
            break
    if after == before:
        msg = (f"{label} drag never reordered under headless cage — the injected "
               f"uinput pointer isn't reaching the compositor (seat/libinput), so "
               f"the Wayland drag path was NOT exercised")
        if hard:
            raise AssertionError(msg + " — hard gate, a no-op means the test did nothing")
        print(f"  {label} reorder: {msg} (best-effort; process survived)")
        return False
    assert sorted(after) == sorted(before), f"{label} set changed: {before} -> {after}"
    print(f"  {label} reorder OK: {before} -> {after}")
    return True


def _check_sidebar_reorder(r, rc, tmp: Path, cage, w: int, h: int, sb: int) -> None:
    """Reliable gate: drag the top project row down past the others."""
    for nm in ("Apple", "Banana", "Cherry"):
        r.create_project(name=nm, cwd="/tmp")
    time.sleep(0.5 * SCALE)
    rows = _project_rows(rc, tmp, sb)
    assert len(rows) >= 3, f"could not locate >=3 project rows (found {rows})"
    x = max(12, sb // 2)
    _drag_until_reorder(
        lambda: [int(p["id"]) for p in r.list()],
        lambda: _uinput_drag(w, h, x, rows[0], x, rows[-1] + 30),
        cage, "sidebar", hard=True)


def _check_tab_reorder(r, rc, tmp: Path, cage, w: int, h: int, wait_tab_attached) -> None:
    """Secondary: drag the active pill right (best-effort — title-row race)."""
    pid = r.create_project(name="tabdrag", cwd="/tmp")
    ids = []
    for nm in ("alpha", "bravo", "charlie"):
        t = r.open_tab(pid, cwd="/tmp")
        wait_tab_attached(r, t)
        r.set_title(t, nm)
        ids.append(t)
    r.focus(ids[0])
    time.sleep(0.4 * SCALE)
    loc = _active_pill(rc, tmp)
    if not loc:
        print("  tab reorder: could not locate the active pill (best-effort)")
        return
    cx, cy = loc

    def order():
        proj = next(p for p in r.list() if int(p["id"]) == int(pid))
        return [int(t["id"]) for t in proj["tabs"]]

    _drag_until_reorder(order, lambda: _uinput_drag(w, h, cx, cy, cx + 320, cy),
                        cage, "tab", hard=False)


def main() -> int:
    if not ROOST_BIN.exists():
        _skip(f"{ROOST_BIN} not built (cargo build -p roost-linux)")
    if shutil.which("cage") is None:
        _skip("cage not installed")
    if not Path("/dev/uinput").exists():
        _skip("/dev/uinput absent (need `modprobe uinput` + a writable node)")

    from client import Roost  # noqa: E402 — path set above
    from util import wait_tab_attached  # noqa: E402

    run = Path(__import__("tempfile").mkdtemp(prefix="roost-wldrag-"))
    xdg, state = run / "xdg", run / "state"
    xdg.mkdir(parents=True)
    state.mkdir(parents=True)
    sock = xdg / "roost" / "roost.sock"
    cage_log = run / "cage.log"
    # roost-linux writes $XDG_STATE_HOME/roost/roost.log AND tees to stdout
    # (captured into cage_log since Roost is cage's child); the hard log-clean
    # gate reads both.
    roost_log = state / "roost" / "roost.log"

    env = {
        **os.environ,
        "XDG_RUNTIME_DIR": str(xdg),
        "XDG_STATE_HOME": str(state),
        "ROOST_STATE_DIR": str(state),
        "ROOST_TEST_MODE": "1",
        "GDK_BACKEND": "wayland",
        "GTK_A11Y": "none",
    }
    # Headless wlroots, software render — keep a caller-provided value (the CI
    # job sets headless,libinput) else default.
    env.setdefault("WLR_BACKENDS", "headless")
    env.setdefault("WLR_RENDERER", "pixman")
    env.pop("DISPLAY", None)
    env.pop("DBUS_SESSION_BUS_ADDRESS", None)

    cage = None
    try:
        # cage runs Roost as its single fullscreen client and exits when Roost
        # exits, so `cage.poll()` doubles as the app-liveness check.
        with cage_log.open("wb") as cf:
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
            # cage fullscreens the client, so the window size IS the output size
            # inject_pointer.py maps its absolute device onto (one window_metrics
            # call, no extra screenshot). Headless GTK is scale=1, so these
            # logical points equal the screenshot's pixel space.
            wm = r.window_metrics()
            w, h = int(wm.get("window_width") or 0), int(wm.get("window_height") or 0)
            sb = int(wm.get("sidebar_width") or 220)
            if not (w and h):
                _skip("window_metrics returned no window size")
            _check_sidebar_reorder(r, rc, run, cage, w, h, sb)
            _check_tab_reorder(r, rc, run, cage, w, h, wait_tab_attached)
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

    print("PASS: Wayland pointer-drag guard — no surface abort")
    return 0


if __name__ == "__main__":
    sys.exit(main())
