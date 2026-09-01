"""Launch / quit a Roost UI for tests, and resolve its socket path.

All UIs speak the same IPC, so the test driver is one client
parameterized by target. Per-target launch and isolation details live in
the explicit capability table below instead of two-target conditionals.
"""

from __future__ import annotations

import fcntl
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

from client import Roost, RoostError, scaled_timeout

# The throwaway state dir for a harness-owned session (set in
# `start_session`, removed in `end_session`). `ROOST_STATE_DIR` points the
# launched UI at this so its `state.json` never touches the developer's
# real saved tabs — and is wiped between runs so no stale layout leaks in.
# Only set when the harness *launches* the UI; a reused dev instance keeps
# its own state. None when reusing or before a session starts.
#
# The directory also holds the UI's **state lock** (`state.lock`), so
# removing it is not a plain `rmtree` — see `_remove_session_state`.
_SESSION_STATE_DIR: Path | None = None

# Harness-launched Rust UI process handle + file capturing its
# stdout+stderr (the UI tees its log to stdout). Retained so `wait_alive`
# can tell a UI that crashed on boot (process already exited) from one
# that's merely slow, and surface the captured log instead of an opaque
# "did not boot". `None` until launch; the Mac path launches via
# `open` (no direct child) and leaves these unset.
_ICED_PROC: "subprocess.Popen[bytes] | None" = None
_ICED_LOG: Path | None = None

# Size of the Mac app's own file log at the moment `_launch_mac` ran `open`.
# The Mac app is launched through LaunchServices, so there is no child whose
# stderr we can capture; its startup diagnostics only exist in
# `~/Library/Logs/<label>/roost.log`, which persists across launches. The
# offset is what makes "what this launch said" separable from the developer's
# accumulated log.
# Byte offset into the Mac app's persistent log, recorded at each launch
# attempt. `None` means the harness has not launched the Mac UI in this
# process — distinct from 0, which would mean "read the whole file" and
# would let a refusal line from a previous day satisfy `_boot_refusal`.
_MAC_LOG_OFFSET: int | None = None

# The bundle-launched (`ROOST_ICED_APP`) sibling of `_MAC_LOG_OFFSET` +
# `_ICED_PROC`, scoped to the `iced` target only. Bundle mode launches
# Roost-Iced.app via LaunchServices (`open`), same as `_launch_mac` — no
# direct child, so there is no stdout to capture and no `Popen` handle to
# poll/wait on. `_ICED_BUNDLE_LOG_OFFSET` mirrors `_MAC_LOG_OFFSET`'s
# "everything after this point in the persistent log is this launch's"
# convention, and doubles as the "are we currently in bundle-launch mode"
# flag consulted by `_launch_output`/`_boot_refusal`. `_ICED_BUNDLE_PID` is
# the identify-verified pid of the launched Roost-Iced process, recorded
# only once its identity is confirmed (see `_launch_iced_bundle`) — this is
# the sole handle teardown (`_quit_iced_bundle`) signals against, deliberately
# pid-based rather than process-name-based so it can never reach the Swift
# Roost.app.
_ICED_BUNDLE_LOG_OFFSET: int | None = None
_ICED_BUNDLE_PID: int | None = None

# Must stay in sync with `mac/scripts/bundle-iced.sh`'s `APP_NAME`/`BUNDLE_ID`.
ICED_BUNDLE_EXECUTABLE_NAME = "Roost-Iced"
ICED_BUNDLE_APP_ID = "ai.stridelabs.Roost.iced"

# The UI's own words when it refuses to start because another process holds
# the state lock (`crates/roost-iced/src/main.rs`,
# `mac/Sources/Roost/App.swift` — both share this wording).
_STATE_LOCK_REFUSAL = "is using this state directory"

# Env vars to strip from a harness-launched Rust UI's inherited environment.
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


@dataclass(frozen=True)
class TargetSpec:
    """Launch and isolation capabilities for one UI implementation."""

    name: str
    profile: str
    mac_label: str
    linux_namespace: str
    rust_package: str | None = None
    binary_name: str | None = None
    isolates_user_defaults: bool = False


TARGET_SPECS = {
    "mac": TargetSpec(
        name="mac",
        profile="mac",
        mac_label="Roost",
        linux_namespace="roost",
        isolates_user_defaults=True,
    ),
    "iced": TargetSpec(
        name="iced",
        profile="iced",
        mac_label="Roost-iced",
        linux_namespace="roost-iced",
        rust_package="roost-iced",
        binary_name="roost-iced",
    ),
}
TARGETS = tuple(TARGET_SPECS)

# Seed config the harness points the UI at via ROOST_CONFIG, so the
# command-launcher tests have a deterministic command list (see
# fixtures/launcher.conf + test_launcher.py). Applies only to UIs this
# harness launches; a developer's already-running UI keeps its own config
# (the launcher tests skip when the seed isn't active).
#
# This is the TRACKED template. A harness-launched UI is never pointed at
# it directly: `start_session` copies it into the session's throwaway
# state dir (`_session_config_path()`), and every launch/relaunch of that
# UI uses the copy instead. Otherwise a test that writes back a config key
# (e.g. `show-sidebar-agents`, plan 007) would mutate this repo file and
# race whichever target runs concurrently in CI.
SEED_CONFIG = Path(__file__).resolve().parent / "fixtures" / "launcher.conf"

# Throwaway UserDefaults suite for a harness-launched Mac app, so sidebar
# prefs (RoostSidebarVisible/width) never touch the developer's real
# defaults — the UserDefaults analog of the throwaway ROOST_STATE_DIR
# (ROOST_STATE_DIR can't reach UserDefaults). A fixed name so it persists
# across the sidebar test's mid-test relaunch; cleaned up in `end_session`.
MAC_TEST_DEFAULTS_SUITE = "ai.stridelabs.Roost.e2e"


def _clear_mac_test_defaults() -> None:
    subprocess.run(
        ["defaults", "delete", MAC_TEST_DEFAULTS_SUITE],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def socket_path(target: str) -> Path:
    try:
        spec = TARGET_SPECS[target]
    except KeyError as error:
        raise ValueError(
            f"unknown target {target!r} (want {'|'.join(TARGETS)})"
        ) from error
    home = Path.home()
    if platform.system() == "Darwin":
        return home / f"Library/Caches/{spec.mac_label}/roost.sock"
    xdg = Path(os.environ.get("XDG_RUNTIME_DIR", ""))
    if xdg.is_absolute():
        return xdg / spec.linux_namespace / "roost.sock"
    return Path(f"/tmp/{spec.linux_namespace}-{os.getuid()}") / "roost.sock"


def socket_lock_path(target: str) -> Path:
    """The **socket/bind lock**, beside the socket.

    One of the UI's two permanent locks (`docs/reference/paths.md`). It
    follows `XDG_RUNTIME_DIR`, guards the probe→unlink→bind sequence and
    the bound socket's lifetime, and a `ROOST_STATE_DIR` override never
    moves it. Mirrors `BundleProfile::socket_lock_path`.
    """
    return socket_path(target).with_name("roost.lock")


def state_lock_path(state_dir: Path) -> Path:
    """The **state lock**, beside `state.json`.

    The other permanent lock. It follows `ROOST_STATE_DIR`, so a harness
    session's throwaway state dir gets its own, and it is held for as long
    as a UI is writing that `state.json`. Mirrors
    `BundleProfile::state_lock_path`.

    Its filename deliberately differs from the socket lock's: the two
    directories can be the same directory, and one filename would make a
    process contend with itself (flock is per open file description).
    """
    return state_dir / "state.lock"


def rust_binary_path(target: str) -> tuple[Path, bool]:
    """Resolve a Rust UI binary and whether the path was explicit.

    The per-target override is essential when the repository is mounted into
    a Linux shed: the mounted ``target/`` contains Mach-O artifacts, while the
    guest build lives in a shed-local Cargo target directory.
    """
    try:
        spec = TARGET_SPECS[target]
    except KeyError as error:
        raise ValueError(
            f"unknown target {target!r} (want {'|'.join(TARGETS)})"
        ) from error
    if spec.binary_name is None:
        raise ValueError(f"target {target!r} is not a Rust UI")
    env_name = f"ROOST_{target.upper()}_BIN"
    if override := os.environ.get(env_name):
        path = Path(override).expanduser()
        if not path.is_absolute():
            path = REPO_ROOT / path
        return path, True
    return REPO_ROOT / "target/debug" / spec.binary_name, False


def iced_bundle_app() -> Path | None:
    """Resolve + validate `ROOST_ICED_APP`, or `None` when unset.

    Validated eagerly, right here where the override is read — never lazily
    at the point `open` fails — so a bad path or platform raises a clear
    exception at launch time instead of the harness silently falling back
    to `target/debug/roost-iced` (see `rust_binary_path`, which stays
    untouched and unconsulted when this returns non-None).
    """
    raw = os.environ.get("ROOST_ICED_APP")
    if not raw:
        return None
    if platform.system() != "Darwin":
        raise RuntimeError(
            f"ROOST_ICED_APP={raw!r} is set but this platform is "
            f"{platform.system()!r}: the bundle launch path (LaunchServices "
            "`open`) is macOS-only"
        )
    app = Path(raw).expanduser()
    if not app.is_absolute():
        app = REPO_ROOT / app
    if not app.is_dir():
        raise FileNotFoundError(f"ROOST_ICED_APP does not exist: {app}")
    executable = app / "Contents/MacOS" / ICED_BUNDLE_EXECUTABLE_NAME
    if not executable.is_file():
        raise FileNotFoundError(
            f"ROOST_ICED_APP={app} is missing its executable: {executable}"
        )
    return app


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


def _rust_log_path(target: str) -> Path:
    """Where a harness-launched Rust UI's stdout+stderr are captured. Honors
    `ROOST_E2E_LOG_DIR` (CI points it at a dir it collects + uploads as a
    build artifact); falls back to the system temp dir for local runs."""
    base = Path(os.environ.get("ROOST_E2E_LOG_DIR") or tempfile.gettempdir())
    base.mkdir(parents=True, exist_ok=True)
    return base / f"roost-{target}-ui.log"


def _mac_ui_log_path() -> Path:
    """The Mac app's own file log. `open` gives us no child to capture, so
    this file is the only place its startup diagnostics land."""
    return Path.home() / f"Library/Logs/{TARGET_SPECS['mac'].mac_label}/roost.log"


def _iced_bundle_ui_log_path() -> Path:
    """The bundle-launched Roost-Iced.app's own persistent file log —
    `TARGET_SPECS["iced"].mac_label` ("Roost-iced"), the same directory a
    bare `ROOST_BUNDLE_PROFILE=iced` binary logs to. `open` gives no child
    to capture, same reasoning as `_mac_ui_log_path`."""
    return Path.home() / f"Library/Logs/{TARGET_SPECS['iced'].mac_label}/roost.log"


def _launch_output(target: str) -> str:
    """What the harness-launched UI has written *since this launch*.

    Rust targets get a fresh capture file per launch (opened `wb`), so the
    whole file is this launch. Mac appends to a persistent log, so read from
    the offset `_launch_mac` recorded. Empty when the harness did not launch
    this UI.
    """
    if target == "mac":
        # Mirrors the `proc is None` guard below: no recorded offset means
        # this process never launched the Mac UI, so nothing in that log is
        # ours to read.
        if _MAC_LOG_OFFSET is None:
            return ""
        log: Path | None = _mac_ui_log_path()
        offset = _MAC_LOG_OFFSET
    elif target == "iced" and _ICED_BUNDLE_LOG_OFFSET is not None:
        # Bundle mode: same reasoning as the mac branch above, scoped to
        # the iced target's own log file.
        log = _iced_bundle_ui_log_path()
        offset = _ICED_BUNDLE_LOG_OFFSET
    else:
        proc, log = (_ICED_PROC, _ICED_LOG) if target == "iced" else (None, None)
        if proc is None:
            return ""
        offset = 0
    if log is None or not log.exists():
        return ""
    try:
        # A log that rotated/truncated since the offset was recorded is
        # smaller than the offset itself — seeking there would just read
        # nothing forever. Read from the top instead of past the file.
        if log.stat().st_size < offset:
            offset = 0
        with open(log, "rb") as handle:
            handle.seek(offset)
            return handle.read().decode(errors="replace")
    except OSError:
        return ""


def _boot_failure_detail(target: str) -> str:
    """Diagnostic suffix for a UI boot timeout: whether the launched UI
    already exited (so it crashed, not just slow) and the tail of what it
    logged since launch."""
    parts = []
    proc = _ICED_PROC if target == "iced" else None
    if proc is not None and (rc := proc.poll()) is not None:
        parts.append(f" — UI process exited (code {rc}) before becoming ready")
    tail = _launch_output(target).splitlines()[-40:]
    if tail:
        parts.append("\n--- captured UI log (last 40 lines) ---\n" + "\n".join(tail))
    return "".join(parts)


def _boot_refusal(target: str) -> str | None:
    """The message for a UI that *exited refusing to start*, or None while it
    may still be coming up.

    The two-lock design added a startup path that deliberately does not
    boot: another process holds the state lock for this `ROOST_STATE_DIR`,
    so the UI exits rather than write one `state.json` from two processes.
    Without this, that refusal reaches the developer as a bare `wait_alive`
    timeout and sends them hunting a hang that isn't one.

    Only consulted once the UI is gone — a Rust exit code of 0 is the
    activate-the-running-instance path, not a refusal.
    """
    if target == "mac":
        if _roost_running():
            return None
    elif target == "iced" and _ICED_BUNDLE_LOG_OFFSET is not None:
        # Bundle mode has no direct child to poll (LaunchServices detaches
        # it), so liveness is a name probe, same shape as the mac branch —
        # read-only, unlike teardown's pid-based signals.
        if _roost_iced_bundle_running():
            return None
    else:
        proc = _ICED_PROC if target == "iced" else None
        rc = proc.poll() if proc is not None else None
        if rc is None or rc == 0:
            return None
    line = next(
        (
            line.strip()
            for line in _launch_output(target).splitlines()
            if _STATE_LOCK_REFUSAL in line
        ),
        None,
    )
    if line is None:
        return None
    return (
        f"{target} UI refused to start: {line}\n"
        "Another process holds the state lock for this ROOST_STATE_DIR, so "
        "the UI exited rather than write one state.json from two processes. "
        "This is a refusal, not a hang."
    )


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
    first thing the subscription does (resync-on-subscribe — iced
    `engine_feed.rs`, Mac `RoostEvent.resync`), so it materializes regardless.
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
        # A UI that exited *refusing* to start will never answer; report the
        # refusal now instead of burning the deadline (and, on Mac, instead
        # of `_launch_mac` spending its retry on the same refusal). Not a
        # TimeoutError, on purpose — that retry catches TimeoutError only.
        if (refusal := _boot_refusal(target)) is not None:
            raise RuntimeError(refusal)
        if time.monotonic() >= deadline:
            raise TimeoutError(
                f"{target} UI did not boot within {timeout}s{_boot_failure_detail(target)}"
            )
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
        raise TimeoutError(
            f"{target} UI event subscription not live within {timeout}s"
            f"{_boot_failure_detail(target)}"
        )
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


def _session_config_path() -> Path:
    """The session's throwaway copy of `SEED_CONFIG`, inside the session
    state dir. Both targets get their own state dir (each test run only
    drives one target at a time), so the copy never races the other
    target — and a test that toggles `show-sidebar-agents` writes back
    into this copy instead of mutating the tracked fixture."""
    assert _SESSION_STATE_DIR is not None, "session config requires an active session"
    return _SESSION_STATE_DIR / "launcher.conf"


def owned_session_config_path() -> Path | None:
    """Return the config only when this harness owns the launched UI.

    Config-mutating functional tests must call this before performing any UI
    action. A reused developer instance has no session state dir and therefore
    returns ``None``; tests must skip rather than touching or "restoring" the
    developer's config. Resolve and contain the path defensively so a future
    launch refactor cannot silently redirect writes to the tracked seed.
    """
    if _SESSION_STATE_DIR is None:
        return None
    root = _SESSION_STATE_DIR.resolve()
    path = _session_config_path().resolve()
    try:
        path.relative_to(root)
    except ValueError as error:
        raise AssertionError(f"session config {path} escapes owned root {root}") from error
    if path == SEED_CONFIG.resolve():
        raise AssertionError("owned session config must differ from the tracked seed")
    return path


def owned_process(target: str) -> "subprocess.Popen[bytes] | None":
    """The harness-launched Rust UI child for `target`, or None.

    The lifecycle tests that assert on the UI *process* (it exits on its
    own) need the handle the harness already holds — polling by pid would
    race a replacement onto the same number.
    """
    return _ICED_PROC if target == "iced" else None


def session_state_dir() -> Path | None:
    """The throwaway `ROOST_STATE_DIR` of a harness-owned session, or None
    when the harness is reusing a developer's UI (whose state is never
    read by a test)."""
    return _SESSION_STATE_DIR


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
    if fresh and target == "mac":
        # A killed/interrupted prior test cannot leave sidebar preferences in
        # the fixed isolated suite. Mid-test relaunches call launch() directly
        # and therefore retain the current test's preferences as intended.
        _clear_mac_test_defaults()
    _SESSION_STATE_DIR = Path(tempfile.mkdtemp(prefix="roost-e2e-state-"))
    shutil.copyfile(SEED_CONFIG, _session_config_path())
    # `config.rs`'s provider discovery resolves `providers/` as a sibling
    # of the config file path (`providers_dir()`), so the copy needs that
    # sibling too or `test_provider.py`'s fixture-provider discovery goes
    # dark under the isolated copy.
    seed_providers_dir = SEED_CONFIG.parent / "providers"
    if seed_providers_dir.is_dir():
        shutil.copytree(seed_providers_dir, _SESSION_STATE_DIR / "providers")
    launch(target, state_dir=_SESSION_STATE_DIR, force=fresh)
    return True


def end_session(target: str) -> None:
    """Quit a harness-owned UI and remove its throwaway state (state dir +,
    on Mac, the isolated UserDefaults suite).

    The two cleanups are ordered to match the UI's own acquisition order —
    socket side (`_cleanup_owned_rust_runtime`, which proves the socket lock
    is free) before state side (`_remove_session_state`, which proves the
    state lock is). Both probes are `LOCK_NB` so they cannot deadlock, but
    out-of-order non-blocking probes produce spurious refusals when a
    harness runs beside anything else taking the same pair.
    """
    global _SESSION_STATE_DIR, _ICED_PROC, _ICED_LOG
    quit(target)
    _cleanup_owned_rust_runtime(target)
    _ICED_PROC = None
    _ICED_LOG = None
    if _SESSION_STATE_DIR is not None:
        _remove_session_state(target, _SESSION_STATE_DIR)
        _SESSION_STATE_DIR = None
    if target == "mac":
        _clear_mac_test_defaults()


def _cleanup_owned_rust_runtime(target: str) -> None:
    """Remove a harness-owned Rust UI's socket + **socket lock** after it has
    exited.

    Single-instance locks are inode-scoped, so unlinking while the child is
    merely shutting down could permit a second instance. The direct child
    handle is the ownership proof and the exit wait is the safety fence; the
    flock, the `identify` silence, and the before/after (dev, ino) check are
    the proof that nothing else has taken the socket meanwhile.

    This is the socket/bind lock only. `state.lock` lives in the state dir
    and is cleared by `_remove_session_state`.
    """
    process = _ICED_PROC if target == "iced" else None
    if process is None:
        return
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(
            f"harness-owned {target} UI did not exit; refusing runtime cleanup"
        ) from error
    socket = socket_path(target)
    lock = socket_lock_path(target)
    if not lock.exists():
        if socket.exists():
            raise RuntimeError(
                f"{target} socket remains without its lock; refusing runtime cleanup"
            )
        return
    flags = os.O_RDWR | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(lock, flags)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError(
                f"{target} socket lock is held by another process; refusing cleanup"
            ) from error
        if _answering_pid(target) is not None:
            raise RuntimeError(
                f"a replacement {target} UI owns the socket; refusing cleanup"
            )
        locked = os.fstat(fd)
        named = os.stat(lock, follow_symlinks=False)
        if (locked.st_dev, locked.st_ino) != (named.st_dev, named.st_ino):
            raise RuntimeError(f"{target} socket lock was replaced; refusing cleanup")
        socket.unlink(missing_ok=True)
        named = os.stat(lock, follow_symlinks=False)
        if (locked.st_dev, locked.st_ino) != (named.st_dev, named.st_ino):
            raise RuntimeError(f"{target} socket lock changed during cleanup")
        lock.unlink()
    finally:
        os.close(fd)


def _remove_session_state(target: str, state_dir: Path) -> None:
    """Delete the session's throwaway state dir — but only once nothing holds
    its **state lock**.

    `state.lock` is an inode, not a name. Deleting it out from under a live
    UI frees the name, so the next launch creates a *fresh* lock inode, takes
    it happily, and two processes write one `state.json`. That is precisely
    the failure the state lock exists to prevent, reintroduced by the
    cleanup — and an unconditional `rmtree(..., ignore_errors=True)` would do
    it silently, with no liveness proof at all.

    So take the lock first, the same shape `_cleanup_owned_rust_runtime` uses
    for the socket lock, and raise rather than delete if it is held: a held
    state lock means a UI is still writing that `state.json`.
    """
    lock = state_lock_path(state_dir)
    if not lock.exists():
        # Never launched (or already cleaned): nothing claims this dir. The
        # UI creates the lock eagerly at startup and never unlinks it, so an
        # absent lock beside a used state dir is not a live instance.
        shutil.rmtree(state_dir, ignore_errors=True)
        return
    flags = os.O_RDWR | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(lock, flags)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError(
                f"{target} state lock {lock} is held: a UI is still writing "
                "state.json there; refusing to remove the session state dir"
            ) from error
        locked = os.fstat(fd)
        named = os.stat(lock, follow_symlinks=False)
        if (locked.st_dev, locked.st_ino) != (named.st_dev, named.st_ino):
            raise RuntimeError(
                f"{target} state lock was replaced; refusing state cleanup"
            )
        # Unlink the lock while still holding it, then sweep the rest. Errors
        # on the sweep are ignorable (a throwaway dir the harness owns); the
        # lock is the part that must not go without proof.
        lock.unlink()
        shutil.rmtree(state_dir, ignore_errors=True)
    finally:
        os.close(fd)


def _answering_pid(target: str) -> int | None:
    try:
        client = Roost(socket_path(target))
        try:
            return int(client.identify()["pid"])
        finally:
            client.close()
    except (OSError, RoostError, KeyError, TypeError, ValueError):
        return None


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
    elif target == "iced" and (bundle_app := iced_bundle_app()) is not None:
        _launch_iced_bundle(bundle_app, state_dir=state_dir)
    elif target == "iced":
        # A prior launch in this same process may have gone through
        # bundle mode (ROOST_ICED_APP was set then, unset now); clear
        # the flag `_launch_output`/`_boot_refusal` key off so this
        # Popen launch isn't mistaken for one still in flight, and clear
        # any pid it recorded — otherwise a stale bundle pid from an
        # earlier (now-dead) bundle would make `quit("iced")` dispatch
        # to bundle teardown instead of terminating this live process.
        global _ICED_BUNDLE_LOG_OFFSET, _ICED_BUNDLE_PID
        _ICED_BUNDLE_LOG_OFFSET = None
        _ICED_BUNDLE_PID = None
        spec = TARGET_SPECS[target]
        assert spec.binary_name is not None and spec.rust_package is not None
        binary, explicit_binary = rust_binary_path(target)
        if not binary.is_file():
            if explicit_binary:
                raise FileNotFoundError(
                    f"explicit {target} binary does not exist: {binary}"
                )
            subprocess.run(
                ["cargo", "build", "-p", spec.rust_package], cwd=REPO_ROOT, check=True
            )
        # Rust UIs inherit the full parent env, so sanitize vars that would
        # send the UI somewhere other than what the harness drives. Drop
        # the per-tab vars Roost injects itself (else a value leaked from
        # the shell that launched pytest — e.g. ROOST_SHELL_FEATURES=
        # no-title from a dev's ~/.bashrc — rides into the UI and every
        # tab inherits it, breaking hermetic assertions), plus the profile
        # selector. Then set our own config/state explicitly.
        env = {
            **os.environ,
            "RUST_LOG": _floor_roost_iced_info(os.environ.get("RUST_LOG", "warn")),
        }
        for leaked in _UI_ENV_SANITIZE:
            env.pop(leaked, None)
        env["ROOST_BUNDLE_PROFILE"] = spec.profile
        env["ROOST_CONFIG"] = str(_session_config_path() if state_dir is not None else SEED_CONFIG)
        if state_dir is not None:
            env["ROOST_STATE_DIR"] = str(state_dir)
        # Capture stdout+stderr (the UI tees its log to stdout, and an early
        # panic that predates the file logger only shows on stderr) so a boot
        # failure isn't blind — `wait_alive` reads this on timeout and CI
        # uploads it. Detached: outlive this call; quit() SIGTERMs it by pid.
        global _ICED_PROC, _ICED_LOG
        log_path = _rust_log_path(target)
        log_fh = open(log_path, "wb")
        try:
            proc = subprocess.Popen(
                [str(binary)], cwd=REPO_ROOT, env=env,
                stdout=log_fh, stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        finally:
            log_fh.close()  # the child holds its own dup of the fd
        _ICED_PROC, _ICED_LOG = proc, log_path
        wait_alive(target)
    else:
        raise ValueError(f"unknown target {target!r}")


def _log_size_or_zero(log: Path) -> int:
    """Everything after this size in `log`'s persistent file belongs to the
    launch about to start — the "offset before open" convention shared by
    `_launch_mac` and `_launch_iced_bundle` (bare-binary Rust launches get a
    fresh capture file per run instead, so this convention only applies to
    `open`-launched bundles, which have no child to give a clean stdout to
    capture)."""
    return log.stat().st_size if log.exists() else 0


def _floor_roost_iced_info(rust_log: str) -> str:
    """Add `roost_iced=info` to `rust_log` unless it already names the
    crate. Several E2E assertions read an INFO-level Rust log line a
    quieter session filter would otherwise silence — a bundle launch's
    "resolved bundle identity" line at boot, and (plan 039 C7) the
    workspace-flush line a signal-driven teardown logs — so both launch
    paths floor `RUST_LOG` through this rather than trusting CI's
    `RUST_LOG=warn` e2e default to carry them. An operator's own
    `roost_iced=...` choice always wins."""
    if "roost_iced" in rust_log:
        return rust_log
    return f"{rust_log},roost_iced=info"


def _forward_env(argv: list[str], name: str) -> None:
    """Append `--env NAME=value` to an `open` argv when the harness's own
    process has `name` set, else leave `argv` untouched. Shared plumbing
    between `_launch_mac` and `_launch_iced_bundle`'s env-forwarding — each
    still decides its own allowlist and forwarding order."""
    if name in os.environ:
        argv += ["--env", f"{name}={os.environ[name]}"]


def _launch_iced_bundle(app: Path, *, state_dir: Path | None = None) -> None:
    """Launch a bundle-assembled Roost-Iced.app via LaunchServices (`open`).

    A target-parameterized sibling of `_launch_mac`, not a copy: every value
    here keys off the `iced` target (`TARGET_SPECS["iced"]`, the bundle's own
    `Roost-Iced` executable name, the iced socket) rather than `_launch_mac`'s
    hardcoded "Roost" identity, so nothing here can silently drift onto the
    Swift app. Unlike `_launch_mac` this does not retry-with-cleanup: that
    dance exists for macos-latest CI flakiness around a specific app; a local
    `make e2e-iced-bundle` run is the only consumer today, and a genuine boot
    failure should surface immediately rather than being retried once.
    """
    global _ICED_BUNDLE_LOG_OFFSET, _ICED_BUNDLE_PID
    log = _iced_bundle_ui_log_path()
    log.parent.mkdir(parents=True, exist_ok=True)
    # Everything after this point in the bundle's persistent log belongs to
    # this launch — same "offset before open" convention as `_launch_mac`.
    _ICED_BUNDLE_LOG_OFFSET = _log_size_or_zero(log)

    config_path = _session_config_path() if state_dir is not None else SEED_CONFIG
    argv = ["open", "--env", f"ROOST_CONFIG={config_path}"]
    if state_dir is not None:
        argv += ["--env", f"ROOST_STATE_DIR={state_dir}"]
    # Enumerated allowlist (plan 027 W5), each forwarded only when the
    # session env actually set it — same technique `_launch_mac` uses for
    # ROOST_TEST_MODE. `ICED_BACKEND` matters here in a way it never does for
    # Mac: without it, a CI cell pinned to tiny-skia would silently launch the
    # bundle under wgpu and the renderer matrix would stop meaning anything.
    # Deliberately NOT forwarded: ROOST_BUNDLE_PROFILE — the bundle-id-derived
    # default profile (W3) is the thing this launch path exists to exercise;
    # forwarding an override would bypass the very path under test.
    # `ROOST_SPARKLE_FEED_URL` is the Sparkle lane's loopback appcast
    # (test_sparkle.py binds a port at import time, before this launch).
    # It only takes effect in the bundle when ROOST_TEST_MODE=1 was
    # forwarded too — the seam checks both (plan 028 § 3.9).
    # `ROOST_SSH_BIN` is the HS-3 lane's seam (test_host_ssh.py sets it at
    # import, before this launch): the UI reads it once per tunnel, out of
    # its own environment, so a bundle that did not receive it would exec
    # the real `ssh`. The bare-binary launch above inherits it via
    # `**os.environ` and needs no entry.
    for name in (
        "ROOST_TEST_MODE",
        "ROOST_TEST_TIMEOUT_SCALE",
        "ICED_BACKEND",
        "ROOST_SPARKLE_FEED_URL",
        "ROOST_SSH_BIN",
    ):
        _forward_env(argv, name)
    # RUST_LOG is forwarded with a floor (see `_floor_roost_iced_info`):
    # the launch path asserts the INFO-level "resolved bundle identity"
    # line after boot, so a session filter like RUST_LOG=warn (CI's
    # default for e2e steps) must not silence the very line the assertion
    # requires.
    rust_log = os.environ.get("RUST_LOG")
    if rust_log is not None:
        argv += ["--env", f"RUST_LOG={_floor_roost_iced_info(rust_log)}"]
    argv += [str(app)]
    subprocess.run(argv, check=True)

    # Everything from here on can fail after a real process is already up.
    # Leaving a launched bundle running on any of these failures would leak
    # it into whatever runs next, so any exception tears it down before
    # propagating — pid-based if identity was confirmed, else a best-effort
    # name kill (safe: `ICED_BUNDLE_EXECUTABLE_NAME` can never match the
    # Swift app's process name).
    try:
        wait_alive("iced")

        pid = _answering_pid("iced")
        if pid is None:
            raise RuntimeError(
                "iced bundle: wait_alive succeeded but identify no longer answers"
            )
        command = _process_command(pid)
        name = Path(command).name if command else None
        if name != ICED_BUNDLE_EXECUTABLE_NAME:
            raise RuntimeError(
                f"iced bundle's identify pid {pid} belongs to process {command!r}, "
                f"expected {ICED_BUNDLE_EXECUTABLE_NAME!r} — refusing to adopt it "
                "for teardown (would risk signalling the wrong process)"
            )
        # Name alone isn't enough — a same-named process from somewhere else
        # on $PATH would pass it. `_process_command` returns the full
        # executable path on macOS (`ps -o comm=`), so confirm it actually
        # lives inside the bundle we launched.
        if not command.startswith(str(app)):
            raise RuntimeError(
                f"iced bundle's identify pid {pid} executable {command!r} is "
                f"not under the launched bundle {app} — refusing to adopt it "
                "for teardown (would risk signalling the wrong process)"
            )
        # Recorded only now that identity is confirmed — this is the sole handle
        # `_quit_iced_bundle` acts on.
        _ICED_BUNDLE_PID = pid

        _assert_bundle_identity_logged()
        if os.environ.get("ROOST_TEST_MODE") == "1":
            _assert_test_mode_canary("iced")
    except Exception:
        if _ICED_BUNDLE_PID is not None:
            _quit_iced_bundle(graceful=scaled_timeout(10.0))
        else:
            subprocess.run(["pkill", "-x", ICED_BUNDLE_EXECUTABLE_NAME], check=False)
        raise


def _assert_bundle_identity_logged() -> None:
    """W3's startup log line (`resolved bundle identity`) is the only
    observable proof the bundle-id probe ran and resolved this launch's
    profile — every mapping arm returns `Iced` today, so the log line is
    the whole test surface. Assert it on every bundle-mode launch (plan 027
    W3/W5) rather than in one test module, so it can never silently regress
    while the two curated e2e modules still pass."""
    output = _launch_output("iced")
    want_field = f'bundle_id="{ICED_BUNDLE_APP_ID}"'
    if "resolved bundle identity" not in output or want_field not in output:
        raise RuntimeError(
            "iced bundle launch did not log the W3 bundle-identity line "
            f"(want a line containing `resolved bundle identity` and "
            f"`{want_field}`); captured log since launch:\n" + output
        )


def _assert_test_mode_canary(target: str) -> None:
    """The first thing a bundle-mode session does once ROOST_TEST_MODE was
    requested: round-trip a test-mode-only op (`tab.feed_pty_bytes`). No
    generic session-start capability probe exists elsewhere in the harness
    (every other module just calls a gated op directly and lets it raise);
    this is the bundle-launch-path's own version of that, so a dropped
    ROOST_TEST_MODE in the `open --env` forwarding surfaces here — loudly,
    at launch — instead of as a confusing `not-enabled` deep inside whichever
    test happens to run first."""
    client = Roost(socket_path(target))
    try:
        pid = int(client.identify()["pid"])
        project = client.create_project(name=f"bundle-canary-{pid}", cwd="/tmp")
        try:
            tab = client.open_tab(project, cwd="/tmp")
            try:
                client.tab_feed_pty_bytes(tab, b"")
            except RoostError as error:
                if error.code == "not-enabled":
                    raise RuntimeError(
                        "iced bundle launch: ROOST_TEST_MODE=1 was requested but "
                        "tab.feed_pty_bytes reports not-enabled — env forwarding "
                        "dropped ROOST_TEST_MODE on the way into the bundle"
                    ) from error
                raise
        finally:
            # Closing this tab (the project's only one) would cascade to
            # delete the project itself (plan 026 D8), so delete the
            # project directly instead of closing the tab first — and
            # swallow "not-found" the way the `project` fixture's own
            # cleanup does (conftest.py): a test may already have removed
            # it via that same cascade.
            try:
                client.delete_project(project)
            except RoostError as error:
                if error.code != "not-found":
                    raise
    finally:
        client.close()


def _launch_mac(app: Path, *, state_dir: Path | None = None) -> None:
    """Clean any dead leftover, `open` the bundle, wait until ready —
    retrying the open once if the first launch never becomes ready.

    Why the cleanup+retry only here: the macos-latest GUI session is the
    one launch path that can inherit a poisoned environment. A prior
    Roost that crashed (or was force-killed) releases its IPC socket
    cleanly — `IPCServer` re-binds over a stale socket — but the
    socket/bind flock (`roost.lock`) has *no* liveness recovery, so a
    fresh `open` silently terminates against the held lock and never
    answers. `_mac_cleanup()` clears that before launching; the second
    attempt also absorbs a slow/contended LaunchServices spawn under CI
    load. (A bare-binary iced launch is a detached process with its own
    fresh env — no shared state, so it needs neither.)
    """
    global _MAC_LOG_OFFSET
    last: TimeoutError | None = None
    for _attempt in (1, 2):
        _mac_cleanup()
        # Everything after this point in the app's persistent log belongs to
        # this attempt, so a boot failure (including a state-lock refusal) is
        # readable without the developer's accumulated history.
        mac_log = _mac_ui_log_path()
        _MAC_LOG_OFFSET = _log_size_or_zero(mac_log)
        # `open --env` injects the seed config into the launched app
        # (LaunchServices otherwise drops the caller's env). Forward
        # ROOST_TEST_MODE + ROOST_STATE_DIR the same way so the bundled UI
        # sees the test-mode gate and writes state.json to the throwaway
        # dir (not the dev's ~/Library/Application Support/Roost). The
        # bare-binary iced launch path inherits parent env directly via
        # `**os.environ`.
        # This hand-maintained allowlist is the one place a new override
        # can silently no-op on Mac, so keep it in sync with `launch`.
        config_path = _session_config_path() if state_dir is not None else SEED_CONFIG
        argv = [
            "open",
            "--env", f"ROOST_CONFIG={config_path}",
        ]
        _forward_env(argv, "ROOST_TEST_MODE")
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


def _roost_iced_bundle_running() -> bool:
    """Read-only liveness probe for the bundle-launched process, used only
    while `_launch_iced_bundle` hasn't yet confirmed a pid via `identify`
    (see `_boot_refusal`). Never used for termination — teardown
    (`_quit_iced_bundle`) is pid-based so it can't hit the Swift Roost.app."""
    return subprocess.run(["pgrep", "-x", ICED_BUNDLE_EXECUTABLE_NAME],
                          stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL).returncode == 0


def _process_command(pid: int) -> str | None:
    """The `comm` (executable path/name) of a running pid, or `None` if it
    isn't running / isn't visible to us. Used to verify `identify`'s
    reported pid actually belongs to the bundle's own process before the
    harness adopts it for teardown."""
    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "comm="],
        capture_output=True, text=True, check=False,
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _wait_pid_gone(pid: int, timeout: float) -> bool:
    """Poll until `pid` is gone, or `timeout` elapses. Early-exits the
    instant it dies, mirroring `_wait_gone`'s cost shape."""
    deadline = time.monotonic() + timeout
    while _pid_alive(pid):
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.1)
    return True


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


def _quit_mac_process(graceful: float = 3.0) -> None:
    """Stop the Mac app and *prove* it is gone: quit → SIGTERM → SIGKILL.

    A polite `osascript ... to quit` is a request, not a proof. The harness
    goes on to delete the session's throwaway state dir — the state lock
    inode included — the moment this returns, so "asked nicely, then deleted
    the lock" is how a still-live app and its replacement come to write one
    `state.json`. Both locks live on inodes, not names, so the only safe
    predicate before touching either is a confirmed-dead process; that is
    strictly stronger than any flock probe and it covers both locks at once.

    SIGKILL is uncatchable, so a wedged app can't keep us from a clean slate;
    if even that fails (an unreapable zombie), fail loud rather than risk a
    second instance. No-op — and no waits — when nothing is running.

    `graceful` is how long a *healthy* app gets to exit on its own before we
    start signalling; `_wait_gone` returns the instant it dies, so a normal
    quit never pays it. Callers that are cleaning up after an app which
    already failed to answer `identify` pass the short default.
    """
    if not _roost_running():
        return
    # Graceful first; bound the Apple Event so a hung app can't wedge us
    # (osascript would otherwise block on the default AE reply timeout).
    try:
        subprocess.run(["osascript", "-e", 'tell application "Roost" to quit'],
                       check=False, timeout=5)
    except subprocess.TimeoutExpired:
        pass
    if _wait_gone(graceful):
        return
    subprocess.run(["pkill", "-x", "Roost"], check=False)             # SIGTERM
    if _wait_gone(2.0):
        return
    subprocess.run(["pkill", "-9", "-x", "Roost"], check=False)       # SIGKILL
    if not _wait_gone(5.0):
        raise RuntimeError(
            "Roost survived SIGKILL — refusing to unlink its locks or delete "
            "its state dir (would risk a second instance against fresh lock "
            "inodes)")


def _mac_cleanup() -> None:
    """Make the next Mac launch start from a clean slate.

    Reached only when no *healthy* instance answers `identify` (launch()
    returns early otherwise), so we never disturb a developer's running
    app — and when nothing is running this is a pure no-op (no waits).

    Lock invariant: a process holding either instance lock must be
    *confirmed dead* before we unlink anything (see `_quit_mac_process`).
    Only the socket/bind lock lives here; the state lock lives in the
    throwaway `ROOST_STATE_DIR` and is handled by `_remove_session_state`.
    """
    _quit_mac_process()
    socket_path("mac").unlink(missing_ok=True)
    socket_lock_path("mac").unlink(missing_ok=True)
    # Fresh workspace comes from the throwaway `ROOST_STATE_DIR` the harness
    # passes at launch (an empty dir = no stale tabs), so there's nothing to
    # delete here and the developer's real state.json is never touched. This
    # replaced the old ROOST_TEST_RESET_STATE-gated unlink.


def _quit_iced_bundle(graceful: float = 10.0) -> None:
    """Stop a bundle-launched Roost-Iced.app and *prove* it is gone — pid-based
    (SIGTERM, wait, escalate to SIGKILL, wait again), never a process-name
    kill: `_quit_mac_process` can reach for `pkill -x Roost` because "Roost"
    unambiguously means the Swift app, but "Roost-Iced" the process name is
    exactly what we're launching here, so a name-based kill would be safe in
    isolation yet is banned on principle (plan 027 W5) — the one thing this
    helper must never do is become copy-pasteable into a context where it
    WOULD hit the Swift app. `end_session` deletes the session state dir
    (state.lock included) immediately after `quit()` returns, so this must
    not return while the bundle might still hold that lock — mirrors
    `_quit_mac_process`'s confirmed-dead discipline.
    """
    global _ICED_BUNDLE_PID
    pid = _ICED_BUNDLE_PID
    if pid is None:
        return
    if not _pid_alive(pid):
        _ICED_BUNDLE_PID = None
        return
    # macOS recycles pids; a Roost-Iced pid can go dead and be reassigned to
    # an unrelated process between launch and teardown. Confirm the pid
    # still names a Roost-Iced process before signalling it — if not, treat
    # it as already dead rather than risk killing whatever's there now.
    command = _process_command(pid)
    name = Path(command).name if command else None
    if name != ICED_BUNDLE_EXECUTABLE_NAME:
        _ICED_BUNDLE_PID = None
        return
    subprocess.run(["kill", str(pid)], check=False)  # SIGTERM
    if _wait_pid_gone(pid, graceful):
        _ICED_BUNDLE_PID = None
        return
    subprocess.run(["kill", "-9", str(pid)], check=False)  # SIGKILL
    if not _wait_pid_gone(pid, 5.0):
        raise RuntimeError(
            f"Roost-Iced (pid {pid}) survived SIGKILL — refusing to unlink its "
            "locks or delete its state dir (would risk a second instance "
            "against fresh lock inodes)"
        )
    _ICED_BUNDLE_PID = None


def quit(target: str) -> None:
    if target == "mac":
        # Escalate rather than ask: `end_session` deletes the state dir (and
        # its `state.lock`) right after this returns, and a live app holding
        # that lock is exactly what must not be deleted out from under. This
        # also reaches a wedged app that no longer answers `identify`.
        #
        # The graceful window matches the budget the old polite-quit poll
        # gave a healthy app (and scales on slow CI runners), so a mid-test
        # relaunch is never signalled just for being slow to exit.
        _quit_mac_process(graceful=scaled_timeout(10.0))
        return
    if target == "iced" and _ICED_BUNDLE_PID is not None:
        _quit_iced_bundle(graceful=scaled_timeout(10.0))
        return
    pid = _answering_pid(target)
    if pid is None:
        return
    owned = _ICED_PROC if target == "iced" else None
    if owned is not None:
        if owned.poll() is not None:
            raise RuntimeError(
                f"harness-owned {target} child already exited but pid {pid} answers; "
                "refusing to terminate a replacement"
            )
        if pid != owned.pid:
            raise RuntimeError(
                f"{target} socket owner changed from harness pid {owned.pid} to {pid}; "
                "refusing to terminate it"
            )
    subprocess.run(["kill", str(pid)], check=False)
    # The Rust side's proof of death is `_cleanup_owned_rust_runtime`'s
    # `process.wait()`; this bounded poll just avoids returning while the UI
    # is visibly still answering.
    deadline = time.monotonic() + 10
    while is_alive(target) and time.monotonic() < deadline:
        time.sleep(0.25)
