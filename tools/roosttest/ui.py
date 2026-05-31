"""Launch / quit a Roost UI for tests, and resolve its socket path.

Both UIs speak the same IPC, so the test driver is one client
parameterized by target; only launch/quit/socket differ per UI (Swift
`Roost.app` vs the `roost` GTK binary). Mirrors `tools/screenshot/lib.sh`.
"""

from __future__ import annotations

import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from client import Roost, RoostError, scaled_timeout

# The throwaway state dir for a harness-owned session (set in
# `start_session`, removed in `end_session`). `ROOST_STATE_DIR` points the
# launched UI at this so its `state.json` never touches the developer's
# real saved tabs — and is wiped between runs so no stale layout leaks in.
# Only set when the harness *launches* the UI; a reused dev instance keeps
# its own state. None when reusing or before a session starts.
_SESSION_STATE_DIR: Path | None = None

# Env vars to strip from a harness-launched GTK UI's inherited environment.
# These are either per-tab values Roost injects itself (a stale inherited
# value would leak into every tab — `pty.rs` keeps a pre-set
# ROOST_SHELL_FEATURES instead of injecting the default) or selectors the
# harness sets explicitly (config/profile/state). Keeps a UI launched from
# inside a Roost tab, or from a shell with these exported, hermetic.
_UI_ENV_SANITIZE = (
    "ROOST_SHELL_FEATURES",
    "ROOST_SHELL_INTEGRATION",
    "ROOST_RESOURCES_DIR",
    "ROOST_TAB_ID",
    "ROOST_SOCKET",
    "ROOST_BUNDLE_PROFILE",
    "ROOST_STATE_DIR",
    "ROOST_CONFIG",
)

REPO_ROOT = Path(__file__).resolve().parents[2]
TARGETS = ("mac", "gtk")

# Seed config the harness points the UI at via ROOST_CONFIG, so the
# command-launcher tests have a deterministic command list (see
# fixtures/launcher.conf + test_launcher.py). Applies only to UIs this
# harness launches; a developer's already-running UI keeps its own config
# (the launcher tests skip when the seed isn't active).
SEED_CONFIG = Path(__file__).resolve().parent / "fixtures" / "launcher.conf"

# Throwaway UserDefaults suite for a harness-launched Mac app, so sidebar
# prefs (RoostSidebarVisible/width) never touch the developer's real
# defaults — the UserDefaults analog of the throwaway ROOST_STATE_DIR
# (ROOST_STATE_DIR can't reach UserDefaults). A fixed name so it persists
# across the sidebar test's mid-test relaunch; cleaned up in `end_session`.
MAC_TEST_DEFAULTS_SUITE = "ai.stridelabs.Roost.e2e"


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


def start_session(target: str, *, fresh: bool) -> bool:
    """Ensure a UI is running for the test session. Returns True if the
    harness started (and therefore owns) it — the caller quits it at
    teardown.

    Normal mode reuses a developer's already-running UI (and leaves it
    alone). Fresh mode (`--roost-fresh` / `ROOST_TEST_FRESH=1`) instead
    force-quits any running instance so the harness owns a hermetic one —
    seeded config + an isolated, throwaway `ROOST_STATE_DIR`. A
    harness-launched UI ALWAYS gets the throwaway state dir, so no run ever
    reads or writes the developer's real `state.json`.
    """
    global _SESSION_STATE_DIR
    if fresh and is_alive(target):
        print(
            f"WARNING: --roost-fresh is force-quitting the running {target} "
            f"Roost instance (its session/tabs will be closed)",
            file=sys.stderr,
        )
        quit(target)
    if is_alive(target):
        return False  # reuse the developer's running UI (non-fresh)
    _SESSION_STATE_DIR = Path(tempfile.mkdtemp(prefix="roost-e2e-state-"))
    launch(target, state_dir=_SESSION_STATE_DIR, force=fresh)
    return True


def end_session(target: str) -> None:
    """Quit a harness-owned UI and remove its throwaway state (state dir +,
    on Mac, the isolated UserDefaults suite)."""
    global _SESSION_STATE_DIR
    quit(target)
    if _SESSION_STATE_DIR is not None:
        shutil.rmtree(_SESSION_STATE_DIR, ignore_errors=True)
        _SESSION_STATE_DIR = None
    if target == "mac":
        subprocess.run(["defaults", "delete", MAC_TEST_DEFAULTS_SUITE],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


def launch(target: str, *, state_dir: Path | None = None, force: bool = False) -> None:
    """Start the UI. Returns once its socket answers `identify`. No-op if
    already running unless `force` (fresh mode, where the caller has
    already asked the running instance to quit). `state_dir`, when given,
    is passed as `ROOST_STATE_DIR` so the UI isolates its `state.json`."""
    if is_alive(target) and not force:
        return
    # A mid-test relaunch (e.g. the sidebar-persistence test's quit→launch)
    # calls this bare; reuse the session's throwaway dir so state persists
    # across the relaunch and never falls back to the dev's real state.json.
    if state_dir is None:
        state_dir = _SESSION_STATE_DIR
    if target == "mac":
        if platform.system() != "Darwin":
            raise RuntimeError("mac target requires macOS")
        app = REPO_ROOT / "mac/build/Roost.app"
        if not app.is_dir():
            subprocess.run(["./scripts/bundle.sh", "debug"], cwd=REPO_ROOT / "mac", check=True)
        _launch_mac(app, state_dir=state_dir)
    elif target == "gtk":
        binary = REPO_ROOT / "target/debug/roost"
        if not binary.is_file():
            subprocess.run(["cargo", "build", "-p", "roost-linux"], cwd=REPO_ROOT, check=True)
        # GTK inherits the full parent env, so sanitize vars that would
        # send the UI somewhere other than what the harness drives. Drop
        # the per-tab vars Roost injects itself (else a value leaked from
        # the shell that launched pytest — e.g. ROOST_SHELL_FEATURES=
        # no-title from a dev's ~/.bashrc — rides into the UI and every
        # tab inherits it, breaking hermetic assertions), plus the profile
        # selector. Then set our own config/state explicitly.
        env = {**os.environ, "RUST_LOG": os.environ.get("RUST_LOG", "warn")}
        for leaked in _UI_ENV_SANITIZE:
            env.pop(leaked, None)
        env["ROOST_CONFIG"] = str(SEED_CONFIG)
        if state_dir is not None:
            env["ROOST_STATE_DIR"] = str(state_dir)
        # Detached: outlive this call; the quit() path SIGTERMs it by pid.
        subprocess.Popen([str(binary)], cwd=REPO_ROOT, env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
        wait_alive(target)
    else:
        raise ValueError(f"unknown target {target!r}")


def _launch_mac(app: Path, *, state_dir: Path | None = None) -> None:
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
        # (LaunchServices otherwise drops the caller's env). Forward
        # ROOST_TEST_MODE + ROOST_STATE_DIR the same way so the bundled UI
        # sees the test-mode gate and writes state.json to the throwaway
        # dir (not the dev's ~/Library/Application Support/Roost). The GTK
        # launch path inherits parent env directly via `**os.environ`.
        # This hand-maintained allowlist is the one place a new override
        # can silently no-op on Mac, so keep it in sync with `launch`.
        argv = [
            "open",
            "--env", f"ROOST_CONFIG={SEED_CONFIG}",
        ]
        if "ROOST_TEST_MODE" in os.environ:
            argv += ["--env", f"ROOST_TEST_MODE={os.environ['ROOST_TEST_MODE']}"]
        if state_dir is not None:
            argv += ["--env", f"ROOST_STATE_DIR={state_dir}"]
        # Isolate UserDefaults-backed prefs (sidebar visibility/width) to a
        # throwaway suite so a harness run never reads/writes the dev's prefs.
        argv += ["--env", f"ROOST_DEFAULTS_SUITE={MAC_TEST_DEFAULTS_SUITE}"]
        argv += [str(app)]
        subprocess.run(argv, check=True)
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
    # Fresh workspace comes from the throwaway `ROOST_STATE_DIR` the harness
    # passes at launch (an empty dir = no stale tabs), so there's nothing to
    # delete here and the developer's real state.json is never touched. This
    # replaced the old ROOST_TEST_RESET_STATE-gated unlink.


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
