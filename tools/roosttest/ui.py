"""Launch / quit a Roost UI for tests, and resolve its socket path.

Both UIs speak the same IPC, so the test driver is one client
parameterized by target; only launch/quit/socket differ per UI (Swift
`Roost.app` vs the `roost` GTK binary). Mirrors `tools/screenshot/lib.sh`.
"""

from __future__ import annotations

import os
import platform
import subprocess
import time
from pathlib import Path

from client import Roost, RoostError, scaled_timeout

REPO_ROOT = Path(__file__).resolve().parents[2]
TARGETS = ("mac", "gtk")

# Seed config the harness points the UI at via ROOST_CONFIG, so the
# command-launcher tests have a deterministic command list (see
# fixtures/launcher.conf + test_launcher.py). Applies only to UIs this
# harness launches; a developer's already-running UI keeps its own config
# (the launcher tests skip when the seed isn't active).
SEED_CONFIG = Path(__file__).resolve().parent / "fixtures" / "launcher.conf"


def socket_path(target: str) -> Path:
    home = Path.home()
    if target == "mac":
        return home / "Library/Caches/Roost/roost.sock"
    if target == "gtk":
        if platform.system() == "Darwin":
            return home / "Library/Caches/Roost-gtk/roost.sock"
        xdg = os.environ.get("XDG_RUNTIME_DIR") or f"/tmp/roost-{os.getuid()}"
        return Path(xdg) / "roost/roost.sock"
    raise ValueError(f"unknown target {target!r} (want mac|gtk)")


def is_alive(target: str) -> bool:
    try:
        c = Roost(socket_path(target))
        try:
            c.identify()
            return True
        finally:
            c.close()
    except (OSError, RoostError):
        return False


def wait_alive(target: str, timeout: float = 30.0) -> None:
    """Block until the UI is *ready to drive*, not merely until the socket
    answers.

    Two startup stages to clear:
      1. The IPC server binds early (so `identify` works the instant the
         process starts), but the workspace + tab machinery come up
         afterward on the UI main loop. Wait until a tab exists.
      2. The UI's workspace-event subscription comes up at the end of
         bootstrap. Confirm it's live by round-tripping a probe tab —
         open it, require it to materialize (dump succeeds), then close
         it. No fixed sleep.

    A tab opened via IPC *before* the subscription is live no longer
    races permanently: both UIs reconcile against a full snapshot as the
    first thing the subscription does (resync-on-subscribe — GTK
    `events.rs`, Mac `RoostEvent.resync`), so it materializes regardless.
    This probe is therefore a readiness gate (don't make the first test
    absorb boot latency), not a workaround for a dropped event.
    """
    timeout = scaled_timeout(timeout)
    deadline = time.monotonic() + timeout
    # (1) booted: at least one tab exists.
    while True:
        try:
            c = Roost(socket_path(target))
            try:
                if c.tabs():
                    break
            finally:
                c.close()
        except (OSError, RoostError):
            pass
        if time.monotonic() >= deadline:
            raise TimeoutError(f"{target} UI did not boot within {timeout}s")
        time.sleep(0.25)

    # (2) subscription live: a freshly opened tab must materialize.
    c = Roost(socket_path(target))
    try:
        boot_project = int(c.list()[0]["id"])
        while time.monotonic() < deadline:
            probe = c.open_tab(boot_project, cwd="/tmp")
            if _materializes(c, probe, deadline):
                c.close_tab(probe)
                return
            c.close_tab(probe)  # event was missed; retry once sub is live
        raise TimeoutError(f"{target} UI event subscription not live within {timeout}s")
    finally:
        c.close()


def _materializes(c: Roost, tab_id: int, deadline: float, window: float = 3.0) -> bool:
    end = min(deadline, time.monotonic() + scaled_timeout(window))
    while time.monotonic() < end:
        try:
            c.dump(tab_id)
            return True
        except RoostError as e:
            if e.code != "not-found":
                raise
        time.sleep(0.1)
    return False


def launch(target: str) -> None:
    """Start the UI if it isn't already running. Returns once its socket
    answers `identify`."""
    if is_alive(target):
        return
    if target == "mac":
        if platform.system() != "Darwin":
            raise RuntimeError("mac target requires macOS")
        app = REPO_ROOT / "mac/build/Roost.app"
        if not app.is_dir():
            subprocess.run(["./scripts/bundle.sh", "debug"], cwd=REPO_ROOT / "mac", check=True)
        _launch_mac(app)
    elif target == "gtk":
        binary = REPO_ROOT / "target/debug/roost"
        if not binary.is_file():
            subprocess.run(["cargo", "build", "-p", "roost-linux"], cwd=REPO_ROOT, check=True)
        env = {**os.environ, "RUST_LOG": os.environ.get("RUST_LOG", "warn"),
               "ROOST_CONFIG": str(SEED_CONFIG)}
        # Detached: outlive this call; the quit() path SIGTERMs it by pid.
        subprocess.Popen([str(binary)], cwd=REPO_ROOT, env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
        wait_alive(target)
    else:
        raise ValueError(f"unknown target {target!r}")


def _launch_mac(app: Path) -> None:
    """Clean any dead leftover, `open` the bundle, wait until ready —
    retrying the open once if the first launch never becomes ready.

    Why the cleanup+retry only here: the macos-latest GUI session is the
    one launch path that can inherit a poisoned environment. A prior
    Roost that crashed (or was force-killed) releases its IPC socket
    cleanly — `IPCServer` re-binds over a stale socket — but the
    single-instance flock (`roost.lock`) has *no* liveness recovery, so a
    fresh `open` silently terminates against the held lock and never
    answers. `_mac_cleanup()` clears that before launching; the second
    attempt also absorbs a slow/contended LaunchServices spawn under CI
    load. (GTK launches a detached binary on a fresh DISPLAY — no shared
    state, so it needs neither.)
    """
    last: TimeoutError | None = None
    for attempt in (1, 2):
        _mac_cleanup()
        # `open --env` injects the seed config into the launched app
        # (LaunchServices otherwise drops the caller's env).
        subprocess.run(["open", "--env", f"ROOST_CONFIG={SEED_CONFIG}", str(app)], check=True)
        try:
            wait_alive("mac")
            return
        except TimeoutError as e:
            last = e
    raise last  # type: ignore[misc]


def _roost_running() -> bool:
    return subprocess.run(["pgrep", "-x", "Roost"],
                          stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL).returncode == 0


def _wait_gone(timeout: float) -> bool:
    """Poll until no Roost process remains, or `timeout` elapses.

    Early-exits the instant the process dies, so a clean quit (or nothing
    running) costs ~0 — the bound only bites a process that won't die.
    """
    deadline = time.monotonic() + timeout
    while _roost_running():
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.1)
    return True


def _mac_cleanup() -> None:
    """Make the next Mac launch start from a clean slate.

    Reached only when no *healthy* instance answers `identify` (launch()
    returns early otherwise), so we never disturb a developer's running
    app — and when nothing is running this is a pure no-op (no waits).

    Lock invariant: a process holding the single-instance flock must be
    *confirmed dead* before we unlink the lock/socket. The flock lives on
    the inode, not the path — unlinking it out from under a still-live
    (wedged) process frees the path, so the launch retry creates a fresh
    lock inode and a second instance runs alongside the old one. So
    escalate quit → SIGTERM → SIGKILL and only unlink once nothing's left.
    SIGKILL is uncatchable, so a wedged app can't keep us from a clean
    slate; if even that fails (an unreapable zombie), fail loud rather than
    double-instance.
    """
    home = Path.home()
    if _roost_running():
        # Graceful first; bound the Apple Event so a hung app can't wedge us
        # (osascript would otherwise block on the default AE reply timeout).
        try:
            subprocess.run(["osascript", "-e", 'tell application "Roost" to quit'],
                           check=False, timeout=5)
        except subprocess.TimeoutExpired:
            pass
        if not _wait_gone(3.0):
            subprocess.run(["pkill", "-x", "Roost"], check=False)         # SIGTERM
            if not _wait_gone(2.0):
                subprocess.run(["pkill", "-9", "-x", "Roost"], check=False)   # SIGKILL
                if not _wait_gone(5.0):
                    raise RuntimeError(
                        "Roost survived SIGKILL — refusing to unlink its lock "
                        "(would risk a second instance against a fresh lock inode)")
    cache = home / "Library/Caches/Roost"
    (cache / "roost.sock").unlink(missing_ok=True)
    (cache / "roost.lock").unlink(missing_ok=True)
    # Fresh workspace, opt-in only: a crash-left state.json would reopen
    # stale tabs and add variance. Gated behind ROOST_TEST_RESET_STATE
    # (CI sets it) so a local `make e2e-mac` never wipes a developer's
    # real saved tab layout — there's no env to redirect the state path
    # (it derives from HOME, see BundleProfile.swift), so we must guard
    # the delete itself.
    if os.environ.get("ROOST_TEST_RESET_STATE") == "1":
        (home / "Library/Application Support/Roost/state.json").unlink(missing_ok=True)


def quit(target: str) -> None:
    if not is_alive(target):
        return
    if target == "mac":
        subprocess.run(["osascript", "-e", 'tell application "Roost" to quit'], check=False)
    else:
        c = Roost(socket_path(target))
        try:
            pid = c.identify()["pid"]
        finally:
            c.close()
        subprocess.run(["kill", str(pid)], check=False)
    deadline = time.monotonic() + 10
    while is_alive(target) and time.monotonic() < deadline:
        time.sleep(0.25)
