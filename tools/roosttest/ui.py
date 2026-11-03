"""Launch / quit a Roost UI for tests, and resolve its socket path.

The driver is one client parameterized by target. There is one target,
`iced` (the Rust UI), run either as a bare cargo binary or, on macOS, as
the app bundle `ROOST_ICED_APP` names; its launch and isolation details
live in the capability table below.
"""

from __future__ import annotations

import atexit
import fcntl
import os
import platform
import plistlib
import shutil
import subprocess
import sys
import tempfile
import time
from collections.abc import MutableMapping
from dataclasses import dataclass
from pathlib import Path
from xml.parsers.expat import ExpatError

from client import Roost, RoostError, scaled_timeout

# The throwaway state dir for a harness-owned session (set in
# `start_session`, removed in `end_session`). `ROOST_STATE_DIR` points the
# launched UI at this so its `state.json` never touches the developer's
# real saved tabs — and is wiped between runs so no stale layout leaks in.
# Only set when the harness *launches* the UI; a reused dev instance keeps
# its own state. None when reusing or before a session starts.
#
# The directory also holds the UI's **state lock** (`state.lock`) — and,
# since #397, a spawned `roost-session`'s lock underneath it — so
# removing it is not a plain `rmtree`; see `_remove_session_state`.
_SESSION_STATE_DIR: Path | None = None

# Where a `roost-session` the UI spawns keeps its own state: nested
# inside its launcher's state dir, so the throwaway `ROOST_STATE_DIR`
# above isolates the daemon too rather than colliding with it (#397).
#
# MIRRORS `roost_ipc::paths::derived_session_state_dir`, frozen on the
# Rust side by
# `paths::tests::a_spawned_session_derives_a_dir_nested_in_an_absolute_seam`.
DERIVED_SESSION_SUBDIR = "session"

# Harness-launched Rust UI process handle + file capturing its
# stdout+stderr (the UI tees its log to stdout). Retained so `wait_alive`
# can tell a UI that crashed on boot (process already exited) from one
# that's merely slow, and surface the captured log instead of an opaque
# "did not boot". `None` until launch; the bundle path launches via
# `open` (no direct child) and leaves these unset.
_ICED_PROC: "subprocess.Popen[bytes] | None" = None
_ICED_LOG: Path | None = None

# The bundle-launched (`ROOST_ICED_APP`) sibling of `_ICED_PROC`. Bundle mode
# launches the bundle via LaunchServices (`open`) — no direct child, so there
# is no stdout to capture and no `Popen` handle to poll/wait on; its startup
# diagnostics only exist in `~/Library/Logs/<label>/roost.log`, which persists
# across launches. `_ICED_BUNDLE_LOG_OFFSET` is that log's size at the moment
# of `open`: everything after it is this launch's, which is what makes "what
# this launch said" separable from the developer's accumulated log. `None`
# means no bundle launch in this process — distinct from 0, which would mean
# "read the whole file" and would let a refusal line from a previous day
# satisfy `_boot_refusal` — so it doubles as the "are we currently in
# bundle-launch mode" flag consulted by `_launch_output`/`_boot_refusal`.
# `_ICED_BUNDLE` is the bundle that launch drove. `_ICED_BUNDLE_PID` is the
# identify-verified pid of its process, recorded only once its identity is
# confirmed (see `_launch_iced_bundle`) — this is the sole handle teardown
# (`_quit_iced_bundle`) signals against, and every signal is re-qualified by
# the executable's path inside that bundle, never by a process name: the
# production bundle's process is named `Roost`, as an installed Roost.app's
# is.
_ICED_BUNDLE_LOG_OFFSET: int | None = None
_ICED_BUNDLE: IcedBundle | None = None
_ICED_BUNDLE_PID: int | None = None

# The two identities `mac/scripts/bundle-iced.sh` assembles
# (ROOST_BUNDLE_IDENTITY). Only the production id resolves the `Mac`
# profile — MIRRORS `mac_bundle_default_kind` in crates/roost-iced/src/main.rs.
PRODUCTION_BUNDLE_ID = "ai.stridelabs.Roost"
DEV_BUNDLE_ID = "ai.stridelabs.Roost.iced"
# The dev bundle's executable, for the lanes that need that bundle itself.
ICED_BUNDLE_EXECUTABLE_NAME = "Roost-Iced"
# Outside CI the harness drives the production identity only when this is
# `1` — see `refuse_production_identity`.
ALLOW_PRODUCTION_IDENTITY_ENV = "ROOST_TEST_ALLOW_PRODUCTION_IDENTITY"

# The UI's own words when it refuses to start because another process holds
# the state lock (`crates/roost-iced/src/main.rs`).
_STATE_LOCK_REFUSAL = "is using this state directory"

# MIRRORS `roost_ipc::paths::{SESSION_SOCKET_DIR_ENV, TEST_PASSWD_HOME_ENV,
# TEST_OLD_SESSION_DIR_ENV}`, frozen on the Rust side by
# `paths::tests::the_session_socket_env_names_are_frozen`. The first names a
# session socket's directory; the other two are debug-only test knobs. A
# test-mode process that would resolve the default session socket with none
# of them is refused by the product itself, so every process this harness
# starts carries the harness's own choice — see `claim_session_isolation`.
SESSION_SOCKET_DIR_ENV = "ROOST_SESSION_SOCKET_DIR"
TEST_PASSWD_HOME_ENV = "ROOST_TEST_PASSWD_HOME"
TEST_OLD_SESSION_DIR_ENV = "ROOST_TEST_OLD_SESSION_DIR"
SESSION_ISOLATION_ENV = (SESSION_SOCKET_DIR_ENV, TEST_PASSWD_HOME_ENV, TEST_OLD_SESSION_DIR_ENV)

# MIRRORS `roost_ipc::paths::TEST_USER_RUNTIME_DIR_ENV` (plan 085 D3): what a
# debug or test-mode build derives a Linux tab's `XDG_RUNTIME_DIR` from in
# place of `/run/user/<uid>`. Set by `pin_user_runtime_dir`.
TEST_USER_RUNTIME_DIR_ENV = "ROOST_TEST_USER_RUNTIME_DIR"
# MIRRORS `roost_ipc::opener::OPENER_SET`: what a tab gets from its opener,
# never from the process serving it.
OPENER_SET = (
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
)


def pin_user_runtime_dir(env: MutableMapping[str, str]) -> MutableMapping[str, str]:
    """Point `env`'s runtime-dir knob at `env`'s own `XDG_RUNTIME_DIR`,
    whatever it inherited, so the tabs of a UI or daemon started under it
    derive that private directory and never the desktop's. With no runtime
    dir the knob is set empty, which derives nothing: an unset knob would
    derive the real `/run/user/<uid>`.

    Every environment this harness starts a UI or daemon under goes
    through here, and so does its own `os.environ` whenever it moves
    `XDG_RUNTIME_DIR`, for the children that inherit it (`roostctl session
    start`)."""
    env[TEST_USER_RUNTIME_DIR_ENV] = env.get("XDG_RUNTIME_DIR", "")
    return env


# Env vars to strip from a harness-launched Rust UI's inherited environment.
# These are either per-tab values Roost injects itself (a stale inherited
# value would leak into every tab — `pty.rs` keeps a pre-set
# ROOST_SHELL_FEATURES instead of injecting the default) or selectors the
# harness sets explicitly (config/profile/state/session socket). Keeps a UI
# launched from inside a Roost tab, or from a shell with these exported,
# hermetic.
_UI_ENV_SANITIZE = (
    "ROOST_SHELL_FEATURES",
    "ROOST_SHELL_INTEGRATION",
    "ROOST_RESOURCES_DIR",
    "ROOST_TAB_ID",
    "ROOST_SOCKET",
    "ROOST_BUNDLE_PROFILE",
    "ROOST_STATE_DIR",
    "ROOST_CONFIG",
    "ROOST_TEST_SESSION_DIR_NAMES",
    *SESSION_ISOLATION_ENV,
    # The runner's SSH identity and agent (plan 085 D9): a UI is a local
    # opener, and an inherited login would make its tabs about the runner's.
    # Its display and bus stay — they are the harness's own xvfb/weston.
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "SSH_AUTH_SOCK",
    # Another multiplexer's marker, which an agent inside a tab reads as
    # "I am in herdr".
    "HERDR_ENV",
)

REPO_ROOT = Path(__file__).resolve().parents[2]


@dataclass(frozen=True)
class TargetSpec:
    """Launch and isolation capabilities for one UI implementation."""

    name: str
    profile: str
    mac_label: str
    linux_namespace: str
    rust_package: str
    binary_name: str


TARGET_SPECS = {
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
# fixtures/launcher.conf; test_newtab_cwd.py and test_shell_integration.py
# launch its commands). Applies only to UIs this
# harness launches; a developer's already-running UI keeps its own config
# (the launcher tests skip when the seed isn't active).
#
# This is the TRACKED template. A harness-launched UI is never pointed at
# it directly: `start_session` copies it into the session's throwaway
# state dir (`_session_config_path()`), and every launch/relaunch of that
# UI uses the copy instead. Otherwise a test that writes back a config key
# (e.g. `show-sidebar-agents`, plan 007) would mutate this repo file and
# race another run in CI.
SEED_CONFIG = Path(__file__).resolve().parent / "fixtures" / "launcher.conf"


def claim_session_isolation() -> tuple[Path, str | None]:
    """Give this harness process a session socket directory of its own, and
    with it every UI, `roostctl` and session it starts.

    Whatever the process inherited is dropped first — the gate's, CI's, or a
    developer's own `ROOST_SESSION_SOCKET_DIR` must not decide where this
    run's sessions live, and a stray passwd-home or old-session knob would
    let a test-mode process past the product's guard. Called once, by
    `conftest.py` at import, so before any test module: a module that lends
    a session root re-exports its own directory afterwards
    (`session.socket_dir`), and the old-session lane trades the override for
    a scratch passwd home. Returns the directory and the value it replaced.

    The runtime-dir knob is pinned here too (`pin_user_runtime_dir`), for
    the children that inherit this process's environment; a module that
    moves `XDG_RUNTIME_DIR` re-pins after it.
    """
    pin_user_runtime_dir(os.environ)
    inherited = os.environ.get(SESSION_SOCKET_DIR_ENV)
    for name in SESSION_ISOLATION_ENV:
        os.environ.pop(name, None)
    # `/tmp` and resolved: short for `sun_path`, and `validate_runtime_dir`
    # refuses a socket directory with a symlinked component.
    owned = Path(tempfile.mkdtemp(prefix="roost-ss-", dir="/tmp")).resolve()
    atexit.register(shutil.rmtree, owned, ignore_errors=True)
    os.environ[SESSION_SOCKET_DIR_ENV] = str(owned)
    return owned, inherited


def session_isolation_env() -> dict[str, str]:
    """The session isolation a UI launch carries: this process's own, as
    `claim_session_isolation` and the modules after it left it. Launch
    builders strip the three names (`_UI_ENV_SANITIZE`) and set them from
    here."""
    return {name: os.environ[name] for name in SESSION_ISOLATION_ENV if name in os.environ}


def needs_shared_ui(marker_sets) -> bool:
    """Whether a run needs the session's shared UI: some collected test is
    neither headless (`session_daemon`) nor the owner of its own UI
    (`owns_ui`). `marker_sets` holds each collected test's marker names."""
    return any(not ({"session_daemon", "owns_ui"} & set(names)) for names in marker_sets)


def socket_path(target: str) -> Path:
    try:
        spec = TARGET_SPECS[target]
    except KeyError as error:
        raise ValueError(
            f"unknown target {target!r} (want {'|'.join(TARGETS)})"
        ) from error
    home = Path.home()
    if platform.system() == "Darwin":
        return home / f"Library/Caches/{_mac_label(target)}/roost.sock"
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
    env_name = f"ROOST_{target.upper()}_BIN"
    if override := os.environ.get(env_name):
        path = Path(override).expanduser()
        if not path.is_absolute():
            path = REPO_ROOT / path
        return path, True
    return REPO_ROOT / "target/debug" / spec.binary_name, False


@dataclass(frozen=True)
class IcedBundle:
    """An app bundle `ROOST_ICED_APP` names, as its own Info.plist declares
    it, with the profile that declared id resolves in the UI."""

    app: Path
    bundle_id: str
    executable: str
    profile: str
    mac_label: str
    # `title_fallback` in crates/roost-iced/src/app.rs for that profile: the
    # app menu's title and the window title's fallback.
    title: str

    @property
    def executable_path(self) -> Path:
        return self.app / "Contents" / "MacOS" / self.executable

    @property
    def is_production(self) -> bool:
        return self.bundle_id == PRODUCTION_BUNDLE_ID

    def runs_inside(self, command: str) -> bool:
        """Whether a process's executable path lies inside this bundle."""
        return command.startswith(f"{self.app}/")


def read_bundle(app: Path) -> IcedBundle:
    """`app`'s identity, from its `Contents/Info.plist`."""
    info = app / "Contents" / "Info.plist"
    try:
        with open(info, "rb") as handle:
            plist = plistlib.load(handle)
    except OSError as error:
        raise FileNotFoundError(f"ROOST_ICED_APP={app} has no readable {info}: {error}") from error
    except (ValueError, ExpatError) as error:
        raise ValueError(f"ROOST_ICED_APP={app}: {info} is not a property list: {error}") from error
    fields = plist if isinstance(plist, dict) else {}
    bundle_id = fields.get("CFBundleIdentifier")
    executable = fields.get("CFBundleExecutable")
    if not (isinstance(bundle_id, str) and bundle_id and isinstance(executable, str) and executable):
        raise ValueError(f"{info} must name a CFBundleIdentifier and a CFBundleExecutable")
    if bundle_id == PRODUCTION_BUNDLE_ID:
        return IcedBundle(app, bundle_id, executable, profile="mac", mac_label="Roost", title="Roost")
    spec = TARGET_SPECS["iced"]
    return IcedBundle(
        app, bundle_id, executable, profile=spec.profile, mac_label=spec.mac_label, title="Roost-Iced"
    )


def refuse_production_identity(bundle: IcedBundle) -> None:
    """Refuse the production identity outside CI unless opted in.

    On a developer's Mac that identity's socket, logs and defaults domain are
    the installed Roost.app's: driving it would talk to that app, and fresh
    mode would quit it."""
    if not bundle.is_production:
        return
    if os.environ.get("CI") or os.environ.get(ALLOW_PRODUCTION_IDENTITY_ENV) == "1":
        return
    raise RuntimeError(
        f"ROOST_ICED_APP={bundle.app} is the production identity ({PRODUCTION_BUNDLE_ID}): "
        "it shares the installed Roost.app's socket (~/Library/Caches/Roost/roost.sock), "
        "logs and defaults domain, and --roost-fresh would quit whatever answers there. "
        f"Quit Roost.app, then set {ALLOW_PRODUCTION_IDENTITY_ENV}=1 (CI needs no opt-in)."
    )


def iced_bundle() -> IcedBundle | None:
    """Resolve + validate `ROOST_ICED_APP`, or `None` when unset.

    Validated eagerly, right here where the override is read — never lazily
    at the point `open` fails — so a bad path or platform raises a clear
    exception at launch time instead of the harness silently falling back
    to `target/debug/roost-iced` (see `rust_binary_path`, which stays
    untouched and unconsulted when this returns non-None).

    The bundle's Info.plist decides which profile it runs, and with it every
    profile-derived path the harness uses (`socket_path`, the log, the
    expected `identify.app_id`), so the production identity is refused here,
    before any of them is consulted (`refuse_production_identity`).
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
    # Resolved, because the ownership checks compare it against `ps -o
    # comm=`, which reports the executable's resolved path: a bundle reached
    # through a symlinked directory would otherwise never match its own
    # process.
    app = app.resolve()
    if not app.is_dir():
        raise FileNotFoundError(f"ROOST_ICED_APP does not exist: {app}")
    bundle = read_bundle(app)
    if not bundle.executable_path.is_file():
        raise FileNotFoundError(
            f"ROOST_ICED_APP={app} is missing its executable: {bundle.executable_path}"
        )
    refuse_production_identity(bundle)
    return bundle


def iced_bundle_app() -> Path | None:
    bundle = iced_bundle()
    return bundle.app if bundle is not None else None


def iced_app_id() -> str:
    """The `identify.app_id` the iced UI under test serves: the id its
    bundle declares, or the dev id for a bare binary."""
    bundle = iced_bundle()
    return bundle.bundle_id if bundle is not None else DEV_BUNDLE_ID


def iced_app_title() -> str:
    """The iced UI's app-menu title: its bundle profile's, or the dev one."""
    bundle = iced_bundle()
    return bundle.title if bundle is not None else "Roost-Iced"


def _mac_label(target: str) -> str:
    if (bundle := iced_bundle()) is not None:
        return bundle.mac_label
    return TARGET_SPECS[target].mac_label


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


def _iced_bundle_ui_log_path() -> Path:
    """The bundle-launched app's own persistent file log, under its
    profile's label: `Roost` for the production identity, `Roost-iced` for
    a dev bundle (the directory a bare `ROOST_BUNDLE_PROFILE=iced` binary
    logs to too). `open` gives no child to capture, so this file is the
    only place its startup diagnostics land."""
    label = _ICED_BUNDLE.mac_label if _ICED_BUNDLE is not None else TARGET_SPECS["iced"].mac_label
    return Path.home() / f"Library/Logs/{label}/roost.log"


def _launch_output(target: str) -> str:
    """What the harness-launched UI has written *since this launch*.

    A bare binary gets a fresh capture file per launch (opened `wb`), so the
    whole file is this launch. A bundle appends to a persistent log, so read
    from the offset `_launch_iced_bundle` recorded. Empty when the harness
    did not launch this UI.
    """
    log: Path | None
    if target == "iced" and _ICED_BUNDLE_LOG_OFFSET is not None:
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

    Only consulted once the UI is gone — an exit code of 0 is the
    activate-the-running-instance path, not a refusal.
    """
    if target == "iced" and _ICED_BUNDLE_LOG_OFFSET is not None:
        # Bundle mode has no direct child to poll (LaunchServices detaches
        # it), so liveness is a read-only probe for a process running the
        # bundle's own executable.
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


def _booted(c: Roost) -> bool | None:
    """Whether this UI's main loop is up, asked the way its **backend**
    can answer (plan 063 §D1).

    `None` means "not yet"; `True` means booted; the caller closes the
    client either way.

    Under `in-process` the answer is the historical one: a tab exists.
    Under `local-backend = session` the UI owns **no** in-process
    projects by design (§D5), so that predicate can never come true — it
    is not slow, it is wrong, and before this it timed out at 30s. The
    session answer is an `app.sidebar_dump` round trip, which is served
    by a `UiRequest` on the main thread: it can only be answered once the
    engine feed is being drained there, which is the very thing stage 1
    exists to establish. `identify` cannot stand in for it — that one is
    answered engine-side, off the socket, before the UI loop runs.
    """
    if c.identify().get("local_backend", "in-process") == "session":
        c.sidebar_dump()
        return True
    return True if c.tabs() else None


def wait_alive(target: str, timeout: float = 30.0) -> None:
    """Block until the UI is *ready to drive*, not merely until the socket
    answers.

    Two startup stages to clear:
      1. The IPC server binds early (so `identify` works the instant the
         process starts), but the workspace + tab machinery come up
         afterward on the UI main loop. Wait until [`_booted`] says so.
      2. The UI's workspace-event subscription comes up at the end of
         bootstrap. Confirm it's live by round-tripping a probe tab —
         open it, require it to materialize (dump succeeds), then close
         it. No fixed sleep.

    A tab opened via IPC *before* the subscription is live no longer
    races permanently: the UI reconciles against a full snapshot as the
    first thing the subscription does (resync-on-subscribe —
    `engine_feed.rs`), so it materializes regardless.
    This probe is therefore a readiness gate (don't make the first test
    absorb boot latency), not a workaround for a dropped event.

    **Stage 2 is skipped under `session`**, and skipping it is the point:
    the probe tab it opens would land in the hidden in-process workspace
    — `tab.open` with a project id of 0 *creates* a project there
    (`ipc.rs`'s `ensure_default_project`) — which is exactly the "a
    project nobody can see" §D5 forbids, and would leave every case in a
    session-mode lane running against a workspace the harness polluted.
    What stage 2 proves is that the *local* workspace's event
    subscription is live, and under `session` there is no local workspace
    to have one; the `UiRequest` round trip in stage 1 already proves the
    feed drain those events land on. A session-mode test waits on the
    slot's own rows itself, which is a condition wait like any other.
    """
    timeout = scaled_timeout(timeout)
    deadline = time.monotonic() + timeout
    # (1) booted, as this UI's backend can answer it.
    session_mode = False
    while True:
        try:
            c = Roost(socket_path(target))
            try:
                if _booted(c):
                    session_mode = (
                        c.identify().get("local_backend", "in-process") == "session"
                    )
                    break
            finally:
                c.close()
        except (OSError, RoostError):
            pass
        # A UI that exited *refusing* to start will never answer; report the
        # refusal now instead of burning the deadline. Not a TimeoutError, on
        # purpose: a caller retrying a slow boot must not retry a refusal.
        if (refusal := _boot_refusal(target)) is not None:
            raise RuntimeError(refusal)
        if time.monotonic() >= deadline:
            raise TimeoutError(
                f"{target} UI did not boot within {timeout}s{_boot_failure_detail(target)}"
            )
        time.sleep(0.25)

    if target == "iced" and platform.system() == "Darwin":
        _wait_menu_installed(target, deadline, timeout)

    if session_mode:
        return

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


def _wait_menu_installed(target: str, deadline: float, timeout: float) -> None:
    """The macOS iced UI installs its native menu bar and Sparkle when its
    window opens, after the socket already answers, so a lane's first
    `app.menu_*` or `app.update_status` call can otherwise beat it (#678).
    Without ROOST_TEST_MODE the dump answers `not-enabled`, and the wait
    has nothing to observe."""
    while True:
        c = Roost(socket_path(target))
        try:
            c.app_menu_dump()
            return
        except RoostError as e:
            if e.code == "not-enabled":
                return
            if "not installed yet" not in e.message:
                raise
        finally:
            c.close()
        if time.monotonic() >= deadline:
            raise TimeoutError(
                f"{target} UI's native menu bar not installed within {timeout}s"
                f"{_boot_failure_detail(target)}"
            )
        time.sleep(0.1)


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
    state dir, so a test that toggles `show-sidebar-agents` writes back
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


def test_mode_active() -> bool:
    """Whether the UI this session is driving can be trusted to have
    `ROOST_TEST_MODE=1` in its own process env — i.e. whether test-mode
    ops like `tab.feed_pty_bytes` will actually work against it.

    `launch()` forwards `ROOST_TEST_MODE` from the harness process into a
    UI it spawns, so that only happens when the harness OWNS this session
    (`session_state_dir()` non-None — see `start_session`, which returns
    `False` and leaves `_SESSION_STATE_DIR` at `None` on the reuse path,
    `:577-593`). A reused developer instance keeps whatever env it was
    started with; the harness has no IPC op to read it back, so it is
    never assumed to be in test mode (#483)."""
    return os.environ.get("ROOST_TEST_MODE") == "1" and _SESSION_STATE_DIR is not None


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
    """Quit a harness-owned UI and remove its throwaway state dir.

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


# How long a state-lock claim keeps re-probing before it answers. Short:
# it covers a handover in flight, not a daemon someone forgot to stop.
_STATE_LOCK_SETTLE_S = 2.0

# The sentinel for "somebody else holds this right now", distinct from
# both an absent lock and a claimed descriptor.
_LOCK_HELD = object()


def _try_claim_state_lock(lock: Path, whose: str):
    """One attempt at `lock`: the held descriptor, `None` if the file is not
    there, or `_LOCK_HELD` if another process has it."""
    flags = os.O_RDWR | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(lock, flags)
    except FileNotFoundError:
        return None
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            return _LOCK_HELD
        locked = os.fstat(fd)
        named = os.stat(lock, follow_symlinks=False)
        if (locked.st_dev, locked.st_ino) != (named.st_dev, named.st_ino):
            raise RuntimeError(f"{whose} state lock was replaced; refusing state cleanup")
    except BaseException:
        os.close(fd)
        raise
    return fd


def _claim_state_lock(lock: Path, whose: str, held_means: str) -> int | None:
    """Take `lock` exclusively and keep holding it, or `None` if nothing
    claims it.

    `None` is "nothing here is alive": the processes that take a state lock
    create it eagerly at startup and never unlink it, so a lock that is
    absent — or present and free — beside a used state dir is not a live
    instance. A lock still *held* raises: the caller is about to delete the
    file it names, and the whole point of the probe is that it must not do
    so behind a live writer's back.

    Both answers come from re-probing for `_STATE_LOCK_SETTLE_S` rather than
    from one look, because a single look is wrong in both directions. A
    `roostctl session stop` that exited 0 proves the daemon's *socket* is
    gone, not its process — `serve()` releases the state flock after the
    socket finalizer has run — so a lock can be held for a moment after the
    stop that removed it succeeded. And a daemon still starting creates its
    lock *after* a probe that found none, so an absent lock is only
    provisional too. Re-probing closes both; the loop below does not care
    which way the answer moves.

    The caller keeps the descriptor and closes it after the delete, so the
    claim spans the unlink: releasing here would leave a window for the
    holder to come back between the proof and the removal.
    """
    if not lock.parent.is_dir():
        # Nothing has even begun here, so there is nothing to wait for: a
        # process creates its state dir before the lock inside it, which
        # rules out the "still starting" case the retry exists for. This is
        # also what keeps the retry off the common path — only the
        # local-spawn lane ever grows a `session/` subdirectory at all.
        return None
    settle = scaled_timeout(_STATE_LOCK_SETTLE_S)
    deadline = time.monotonic() + settle
    while True:
        claim = _try_claim_state_lock(lock, whose)
        if isinstance(claim, int):
            return claim
        if time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    if claim is _LOCK_HELD:
        raise RuntimeError(
            f"{whose} state lock {lock} was still held {settle:.1f}s after the "
            f"sweep began: {held_means}. Refusing to remove the session state dir"
        )
    return None


def _remove_session_state(target: str, state_dir: Path) -> None:
    """Delete the session's throwaway state dir — but only once nothing holds
    a **state lock** inside it.

    `state.lock` is an inode, not a name. Deleting it out from under a live
    UI frees the name, so the next launch creates a *fresh* lock inode, takes
    it happily, and two processes write one `state.json`. That is precisely
    the failure the state lock exists to prevent, reintroduced by the
    cleanup — and an unconditional `rmtree(..., ignore_errors=True)` would do
    it silently, with no liveness proof at all.

    Since #397 there are **two** such locks to answer for, because a
    `roost-session` the UI spawns is handed `<state dir>/session`
    (`DERIVED_SESSION_SUBDIR`) and takes its own `state.lock` there from
    startup. A recursive delete would unlink a live daemon's lock *and* its
    `state.json` while its socket kept answering — the daemon writing to an
    unlinked inode, and the next lane dialing a session whose state dir no
    longer exists. Same failure as the UI's, one directory deeper.

    So claim every lock first, the same shape `_cleanup_owned_rust_runtime`
    uses for the socket lock, and raise rather than delete if one is held: a
    held lock means something is still writing that `state.json`. A lane that
    starts a daemon stops it in its own teardown, so reaching this with the
    session lock held means one leaked — which is worth a loud failure, not a
    silent corruption.
    """
    claims: list[tuple[Path, int]] = []
    try:
        for lock, whose, held_means in (
            (
                state_lock_path(state_dir),
                target,
                "a UI is still writing state.json there",
            ),
            (
                state_lock_path(state_dir / DERIVED_SESSION_SUBDIR),
                "spawned roost-session",
                "a daemon this run started is still writing state.json there. "
                "A `roostctl session stop` releases this lock only after it has "
                "unlinked the socket, so a stop that has just returned is still "
                "expected to hold it briefly; this long means the daemon leaked",
            ),
        ):
            fd = _claim_state_lock(lock, whose, held_means)
            if fd is not None:
                claims.append((lock, fd))
        # Unlink each lock while still holding it, then sweep the rest. Errors
        # on the sweep are ignorable (a throwaway dir the harness owns); the
        # locks are the part that must not go without proof.
        for lock, _ in claims:
            lock.unlink()
        shutil.rmtree(state_dir, ignore_errors=True)
    finally:
        for _, fd in claims:
            os.close(fd)


def _answering_pid(target: str) -> int | None:
    return _answering_pid_at(socket_path(target))


def _answering_pid_at(socket: Path, timeout: float | None = None) -> int | None:
    """The pid of the UI answering `identify` on `socket`, or None — also when
    it does not answer within `timeout`: the connect, and the whole reply (a
    server dripping bytes without ever finishing the line included)."""
    try:
        client = Roost(socket, timeout=timeout, deadline=timeout)
        try:
            return int(client.identify()["pid"])
        finally:
            client.close()
    except (OSError, RoostError, KeyError, TypeError, ValueError):
        return None


def _stop_owned_ui(proc: "subprocess.Popen[bytes]") -> None:
    """Stop and reap a UI this harness spawned whose boot wait failed.

    SIGTERM twice before SIGKILL: a UI wedged before its event loop absorbs
    the first SIGTERM by design, and the second escalates to `exit(1)`.
    Only this `Popen`'s pid is signalled — never a group or a name.
    """
    for _attempt in (1, 2):
        if proc.poll() is not None:
            break
        proc.terminate()
        try:
            proc.wait(timeout=scaled_timeout(3.0))
        except subprocess.TimeoutExpired:
            pass
    if proc.poll() is None:
        proc.kill()
    try:
        proc.wait(timeout=scaled_timeout(5.0))
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"harness-launched UI (pid {proc.pid}) survived SIGKILL") from error


def launch(
    target: str,
    *,
    state_dir: Path | None = None,
    force: bool = False,
    extra_env: dict[str, str] | None = None,
) -> None:
    """Start the UI. Returns once its socket answers `identify`. No-op if
    already running unless `force` (fresh mode, where the caller has
    already asked the running instance to quit). `state_dir`, when given,
    is passed as `ROOST_STATE_DIR` so the UI isolates its `state.json`.

    `extra_env` is applied **after** the harness's own `ROOST_CONFIG` /
    `ROOST_STATE_DIR`, so a lane can point one of them somewhere else —
    which is the only way to test a UI whose config lives in a directory
    it cannot write (plan 063's switch: the key write is a phase, and its
    failure has a rollback behind it).

    **The bare-binary launch only.** The `open`-launched *bundle* (when
    `ROOST_ICED_APP` is set) forwards a hand-maintained `--env` allowlist
    rather than an environment, so an override handed to it would be
    dropped and the lane would run, green, against the config it meant to
    replace. It is refused instead
    of threaded through: `open` does not define which of two `--env` flags
    naming the same variable wins, so "threaded" would trade a silent drop
    for an unverifiable override, and the allowlist is deliberately
    curated (`ROOST_BUNDLE_PROFILE` is withheld from it on purpose).
    """
    if extra_env and iced_bundle() is not None:
        raise ValueError(
            f"extra_env has no route into the `open`-launched {target} app: "
            "only the bare-binary launch carries an environment"
        )
    if is_alive(target) and not force:
        return
    # A mid-test relaunch (e.g. the sidebar-persistence test's quit→launch)
    # calls this bare; reuse the session's throwaway dir so state persists
    # across the relaunch and never falls back to the dev's real state.json.
    if state_dir is None:
        state_dir = _SESSION_STATE_DIR
    if target == "iced" and (bundle := iced_bundle()) is not None:
        _launch_iced_bundle(bundle, state_dir=state_dir)
    elif target == "iced":
        # A prior launch in this same process may have gone through
        # bundle mode (ROOST_ICED_APP was set then, unset now); clear
        # the flag `_launch_output`/`_boot_refusal` key off so this
        # Popen launch isn't mistaken for one still in flight, and clear
        # any pid it recorded — otherwise a stale bundle pid from an
        # earlier (now-dead) bundle would make `quit("iced")` dispatch
        # to bundle teardown instead of terminating this live process.
        global _ICED_BUNDLE_LOG_OFFSET, _ICED_BUNDLE_PID, _ICED_BUNDLE
        _ICED_BUNDLE_LOG_OFFSET = None
        _ICED_BUNDLE_PID = None
        _ICED_BUNDLE = None
        spec = TARGET_SPECS[target]
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
        env.update(session_isolation_env())
        env["ROOST_BUNDLE_PROFILE"] = spec.profile
        env["ROOST_CONFIG"] = str(_session_config_path() if state_dir is not None else SEED_CONFIG)
        if state_dir is not None:
            env["ROOST_STATE_DIR"] = str(state_dir)
        # Last, so a lane can redirect what the harness just set.
        env.update(extra_env or {})
        # After `extra_env`: the knob follows the runtime dir this UI actually
        # gets, a lane's own included.
        pin_user_runtime_dir(env)
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
        try:
            wait_alive(target)
        except Exception:
            # `_ICED_PROC`/`_ICED_LOG` stay set: the log is the failure's
            # diagnostic, and teardown's `wait()` on a reaped child is instant.
            _stop_owned_ui(proc)
            raise
    else:
        raise ValueError(f"unknown target {target!r}")


def _log_size_or_zero(log: Path) -> int:
    """Everything after this size in `log`'s persistent file belongs to the
    launch about to start — `_launch_iced_bundle`'s "offset before open"
    convention (bare-binary launches get a fresh capture file per run
    instead; an `open`-launched bundle has no child to give a clean stdout
    to capture)."""
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
    process has `name` set, else leave `argv` untouched."""
    if name in os.environ:
        argv += ["--env", f"{name}={os.environ[name]}"]


def _launch_iced_bundle(bundle: IcedBundle, *, state_dir: Path | None = None) -> None:
    """Launch an assembled iced bundle via LaunchServices (`open`).

    Every value here keys off the bundle's own declared identity (its
    executable, the profile its id resolves, that profile's socket and log).
    There is no retry: a genuine boot failure surfaces immediately.
    """
    global _ICED_BUNDLE_LOG_OFFSET, _ICED_BUNDLE_PID, _ICED_BUNDLE
    app = bundle.app
    if bundle.is_production and state_dir is None:
        raise RuntimeError(
            f"{app} is the production identity: refusing to launch it without a throwaway "
            "state dir, which would leave it on ~/Library/Application Support/Roost"
        )
    _ICED_BUNDLE = bundle
    _ICED_BUNDLE_PID = None
    log = _iced_bundle_ui_log_path()
    log.parent.mkdir(parents=True, exist_ok=True)
    # Everything after this point in the bundle's persistent log belongs to
    # this launch (`_log_size_or_zero`).
    _ICED_BUNDLE_LOG_OFFSET = _log_size_or_zero(log)

    config_path = _session_config_path() if state_dir is not None else SEED_CONFIG
    argv = ["open", "--env", f"ROOST_CONFIG={config_path}"]
    if state_dir is not None:
        argv += ["--env", f"ROOST_STATE_DIR={state_dir}"]
    # Enumerated allowlist (plan 027 W5), each forwarded only when the
    # session env actually set it. Without `ICED_BACKEND`, a CI cell pinned
    # to tiny-skia would silently launch the bundle under wgpu and the
    # renderer matrix would stop meaning anything.
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
    # `**os.environ` and needs no entry. `ROOST_SESSION_SOCKET_DIR` is the
    # harness's own (`claim_session_isolation`): without it a test-mode
    # bundle on a session backend is refused its session socket.
    for name in (
        "ROOST_TEST_MODE",
        "ROOST_TEST_TIMEOUT_SCALE",
        "ICED_BACKEND",
        "ROOST_SPARKLE_FEED_URL",
        "ROOST_SSH_BIN",
        SESSION_SOCKET_DIR_ENV,
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
    # propagating — pid-based if identity was confirmed, else every process
    # running this bundle's own executable (`_stop_bundle_processes`).
    try:
        wait_alive("iced")

        pid = _answering_pid("iced")
        if pid is None:
            raise RuntimeError(
                "iced bundle: wait_alive succeeded but identify no longer answers"
            )
        command = _process_command(pid)
        name = Path(command).name if command else None
        if name != bundle.executable:
            raise RuntimeError(
                f"iced bundle's identify pid {pid} belongs to process {command!r}, "
                f"expected {bundle.executable!r} — refusing to adopt it "
                "for teardown (would risk signalling the wrong process)"
            )
        # Name alone isn't enough — a same-named process from somewhere else
        # (an installed Roost.app, for the production identity) would pass
        # it. `_process_command` returns the full executable path on macOS
        # (`ps -o comm=`), so confirm it actually lives inside the bundle we
        # launched.
        if not bundle.runs_inside(command):
            raise RuntimeError(
                f"iced bundle's identify pid {pid} executable {command!r} is "
                f"not under the launched bundle {app} — refusing to adopt it "
                "for teardown (would risk signalling the wrong process)"
            )
        # Recorded only now that identity is confirmed — this is the sole handle
        # `_quit_iced_bundle` acts on.
        _ICED_BUNDLE_PID = pid

        _assert_bundle_identity_logged(bundle)
        if os.environ.get("ROOST_TEST_MODE") == "1":
            _assert_test_mode_canary("iced")
    except Exception:
        if _ICED_BUNDLE_PID is not None:
            _quit_iced_bundle(graceful=scaled_timeout(10.0))
        else:
            _stop_bundle_processes(bundle, graceful=scaled_timeout(10.0))
        raise


def _assert_bundle_identity_logged(bundle: IcedBundle) -> None:
    """W3's startup log line (`resolved bundle identity`) is the observable
    proof the bundle-id probe ran and resolved the profile this bundle's id
    maps to. Assert it on every bundle-mode launch (plan 027 W3/W5) rather
    than in one test module, so it can never silently regress while the
    curated e2e modules still pass."""
    output = _launch_output("iced")
    want = (f'bundle_id="{bundle.bundle_id}"', f'profile="{bundle.profile}"')
    if "resolved bundle identity" not in output or not all(f in output for f in want):
        raise RuntimeError(
            "iced bundle launch did not log the W3 bundle-identity line "
            f"(want a line containing `resolved bundle identity`, `{want[0]}` "
            f"and `{want[1]}`); captured log since launch:\n" + output
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


def _roost_iced_bundle_running() -> bool:
    """Read-only liveness probe for the bundle-launched process, used only
    while `_launch_iced_bundle` hasn't yet confirmed a pid via `identify`
    (see `_boot_refusal`)."""
    return _ICED_BUNDLE is not None and bool(_bundle_pids(_ICED_BUNDLE))


def _bundle_pids(bundle: IcedBundle) -> list[int]:
    """Pids running `bundle`'s own executable: named like it (`pgrep -x`, a
    read-only probe) AND executing from inside it. The name alone never
    qualifies a process: the production executable is `Roost`, which is the
    installed Roost.app's process name too."""
    try:
        found = subprocess.run(
            ["pgrep", "-x", bundle.executable],
            capture_output=True, text=True, check=False, timeout=_PS_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return []
    pids = [int(token) for token in found.stdout.split() if token.isdigit()]
    return [pid for pid in pids if _runs_inside(pid, bundle) is True]


def _runs_inside(pid: int, bundle: IcedBundle) -> bool | None:
    """Whether `pid` executes from inside `bundle`; `None` when it is alive
    but `ps` cannot say what it runs."""
    command = _process_command(pid)
    if command is None:
        return None if _pid_alive(pid) else False
    return bundle.runs_inside(command)


def _stop_bundle_processes(bundle: IcedBundle, *, graceful: float) -> None:
    """Stop every process running `bundle`'s executable, for a launch that
    failed before its pid was confirmed. Each is re-qualified before each
    signal (`_terminate_owned_pid`), so a same-named process elsewhere — an
    installed Roost.app — is never touched."""
    for pid in _bundle_pids(bundle):
        _terminate_owned_pid(pid, owned=lambda p: _runs_inside(p, bundle), graceful=graceful)


def _process_command(pid: int) -> str | None:
    """The `comm` (executable path/name) of a running pid, or `None` if it
    isn't running / isn't visible to us. Used to verify `identify`'s
    reported pid actually belongs to the bundle's own process before the
    harness adopts it for teardown."""
    try:
        result = subprocess.run(
            ["ps", "-p", str(pid), "-o", "comm="],
            capture_output=True, text=True, check=False, timeout=_PS_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


_PS_TIMEOUT_S = 5.0


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


def _quit_iced_bundle(graceful: float = 10.0) -> None:
    """Stop a bundle-launched app and *prove* it is gone — pid-based
    (SIGTERM, wait, escalate to SIGKILL, wait again), and before each signal
    the pid must still run the launched bundle's own executable, by path:
    never a process-name kill (plan 027 W5), because the production
    bundle's process name is the installed Roost.app's. `end_session`
    deletes the session state dir (state.lock included) immediately after
    `quit()` returns, so this must not return while the bundle might still
    hold that lock: a polite request is not a proof, and both locks live on
    inodes, not names, so the only safe predicate before touching either is
    a confirmed-dead process.
    """
    global _ICED_BUNDLE_PID
    pid = _ICED_BUNDLE_PID
    if pid is None:
        return
    bundle = _ICED_BUNDLE
    if bundle is None:
        raise OwnershipUnknown(
            f"pid {pid} was recorded with no bundle to qualify it by — not signalled"
        )
    _terminate_owned_pid(pid, owned=lambda p: _runs_inside(p, bundle), graceful=graceful)
    _ICED_BUNDLE_PID = None


class OwnershipUnknown(RuntimeError):
    """A live pid that cannot be shown to be — or not to be — the caller's."""


def _terminate_owned_pid(pid: int, *, owned, graceful: float) -> None:
    """SIGTERM `pid`, SIGKILL it if it outlives `graceful`, and return only once
    it is gone — never by process name, and never while it might still hold
    the state lock its caller is about to delete.

    macOS recycles pids, so the number can belong to an unrelated process —
    an installed Roost.app's included — by teardown time, or by the end of the
    SIGTERM grace period. So `owned(pid)` is asked again immediately before
    *each* signal: False (provably not the caller's process) means the one
    launched is already gone, and None (it cannot tell) raises
    `OwnershipUnknown` without signalling, rather than be taken for dead.
    """
    for argv, grace in ((["kill", str(pid)], graceful), (["kill", "-9", str(pid)], 5.0)):
        if not _pid_alive(pid):
            return
        verdict = owned(pid)
        if verdict is None:
            raise OwnershipUnknown(
                f"cannot tell whether pid {pid} is still the process launched — not "
                "signalled, and its locks and state dir must be kept"
            )
        if not verdict:
            return
        subprocess.run(argv, check=False)
        if _wait_pid_gone(pid, grace):
            return
    raise RuntimeError(
        f"pid {pid} survived SIGKILL — refusing to unlink its locks or "
        "delete its state dir (would risk a second instance against fresh "
        "lock inodes)"
    )


def quit(target: str) -> None:
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
