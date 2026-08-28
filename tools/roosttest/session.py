"""Spawn, isolate, and drive a headless `roost-session` daemon.

The UI harness (`ui.py`) is deliberately not involved. A host session is
not a UI: it has no window, it is never a `roostctl --target`, and it
owns its own bundle profile (`BundleProfileKind::Session`). So this
module is a parallel, much smaller launcher — it builds the daemon's
*environment*, spawns the binary, and reads the one readiness line the
launch contract promises (`roost_ipc::session_launch`).

# Isolation

Every test gets its own throwaway profile root, because `roost-session`
resolves its socket / state / log from the environment and a shared
profile would let two tests (or a test and a developer's real session)
contend on one socket, one `state.json`, and one lock pair.

The env differs per OS because the resolver does
(`crates/roost-ipc/src/paths.rs`):

* **macOS** — everything hangs off `$HOME`:
  `~/Library/Caches/<label>/roost.sock`,
  `~/Library/Application Support/<label>`, `~/Library/Logs/<label>`.
  So `HOME` alone is the whole isolation knob.
* **Linux** — XDG: `XDG_RUNTIME_DIR/<ns>/roost.sock`,
  `XDG_DATA_HOME/<ns>`, `XDG_STATE_HOME/<ns>`. `HOME` is set too, as the
  fallback the resolver uses when an XDG dir is missing, and
  `XDG_CACHE_HOME` because `roost-engine`'s shell-integration cache
  writes there.

The root is **canonicalized** (`Path.resolve()`) before anything is
derived from it. `validate_runtime_dir` rejects a socket directory with
a symlinked component, and both `/tmp` (Linux) and `$TMPDIR`
(`/var/folders/...` on macOS) reach the real path through one.

The runtime directory's *parent* is pre-created here rather than left to
the daemon: `validate_runtime_dir` creates only the leaf (non-recursive,
`0700`), which is right for a real machine where `$XDG_RUNTIME_DIR` and
`~/Library/Caches` are provided by the OS. The fixture plays the OS.

# Why `SHELL=/bin/sh`

Restored tabs re-open the user's `$SHELL`; a developer's zsh/bash with
Roost's shell integration emits OSC 0/2/7 marks that would rewrite the
titles and cwds the layout tests assert on. `/bin/sh` in an empty `HOME`
emits nothing, and `ROOST_SHELL_FEATURES=""` disables the integration's
emitters even if a shell did load them. Tests that *want* OSC drive it
explicitly through `argv`.
"""

from __future__ import annotations

import contextlib
import json
import os
import platform
import shutil
import signal
import subprocess
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

from client import Roost, RoostError, scaled_timeout

REPO_ROOT = Path(__file__).resolve().parents[2]

# The daemon's executable name, as `ps -o comm=` reports it. Used as the
# pid-reuse fence in `_terminate_pid`.
BIN_NAME = "roost-session"

# Env this harness owns outright: either it selects a profile / state
# location (a leaked value would send the daemon at the developer's real
# session) or it is something Roost injects per tab (a leaked value rides
# into every shell the session spawns). Removed from the inherited
# environment, then re-set explicitly where the fixture has an opinion.
_SANITIZE = (
    "HOME",
    "SHELL",
    "ZDOTDIR",
    "ENV",
    "HISTFILE",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "ROOST_STATE_DIR",
    "ROOST_BUNDLE_PROFILE",
    "ROOST_CONFIG",
    "ROOST_RESOURCES_DIR",
    "ROOST_SHELL_FEATURES",
    "ROOST_SHELL_INTEGRATION",
    "ROOST_SOCKET",
    "ROOST_TAB_ID",
    "ROOST_SESSION_BIN",
    "ROOST_SESSION_LAUNCH_CWD",
)


# ---------------------------------------------------------------------------
# Binaries
# ---------------------------------------------------------------------------


def session_binary() -> Path:
    """The `roost-session` binary, built on demand.

    `ROOST_SESSION_BIN` first (the same override `roostctl session start`
    consults, so a shed / cross-target run points both at one path), then
    the cargo debug build the `e2e-session` Makefile target produces.
    """
    if override := os.environ.get("ROOST_SESSION_BIN"):
        path = Path(override).expanduser()
        if not path.is_absolute():
            path = REPO_ROOT / path
        if not os.access(path, os.X_OK):
            raise FileNotFoundError(f"ROOST_SESSION_BIN is not executable: {path}")
        return path
    built = REPO_ROOT / "target/debug" / BIN_NAME
    if not os.access(built, os.X_OK):
        subprocess.run(
            ["cargo", "build", "-p", "roost-session"], cwd=REPO_ROOT, check=True
        )
    return built


def _is_debug_build(binary: Path) -> bool:
    """Which pair of directory names the daemon will resolve.

    `BundleProfile::session` splits its paths on `cfg!(debug_assertions)`
    so a dev session can never land in a shipped one's directories, and
    this harness has to predict the same answer to find the socket. The
    build profile is not observable from outside the binary, so read it
    off cargo's own layout — `target/<profile>/<bin>`.
    """
    return binary.parent.name != "release"


def _dir_names(debug: bool) -> tuple[str, str]:
    """`(mac_label, linux_namespace)` — mirrors `paths.rs`'s
    `session_dir_names`."""
    if debug:
        return "RoostSessionDev", "roost-session-dev"
    return "RoostSession", "roost-session"


# ---------------------------------------------------------------------------
# The launch verdict
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Verdict:
    """One parsed readiness line. Mirrors `Verdict::parse` in
    `roost_ipc::session_launch` — deliberately a re-implementation, so a
    change to the wire format fails here instead of being absorbed."""

    kind: str  # "ready" | "already-running" | "error"
    pid: int | None = None
    reason: str = ""
    raw: str = ""

    @staticmethod
    def parse(line: str) -> "Verdict":
        raw = line
        line = line.strip()
        if line.startswith("ready pid="):
            tail = line.removeprefix("ready pid=").strip()
            if tail.isdigit():
                return Verdict("ready", int(tail), raw=raw)
        if line == "already-running":
            return Verdict("already-running", None, raw=raw)
        if line.startswith("already-running pid="):
            tail = line.removeprefix("already-running pid=").strip()
            return Verdict(
                "already-running", int(tail) if tail.lstrip("-").isdigit() else None, raw=raw
            )
        if line.startswith("error: "):
            return Verdict("error", None, line.removeprefix("error: "), raw=raw)
        return Verdict("error", None, f"unrecognized readiness verdict: {line!r}", raw=raw)


# ---------------------------------------------------------------------------
# Waiting
# ---------------------------------------------------------------------------


def wait_until(pred, timeout: float, what: str, interval: float = 0.05):
    """Poll `pred` until it returns something truthy. Scaled by
    `ROOST_TEST_TIMEOUT_SCALE` like every other wait in this harness, so
    a loaded runner widens the daemon's budgets and the driver's
    together."""
    eff = scaled_timeout(timeout)
    deadline = time.monotonic() + eff
    while True:
        value = pred()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(f"timed out after {eff:.1f}s waiting for {what}")
        time.sleep(interval)


def read_pidfile(path: Path) -> int | None:
    """The pid a racer child published, or None if it hasn't yet.

    `> file` truncates before the write lands, so existence alone is not
    content: an empty or half-written file reads as "not yet", not as a
    parse error.
    """
    try:
        raw = path.read_text().strip()
    except OSError:
        return None
    return int(raw) if raw.isdigit() else None


def wait_for_pidfile(path: Path, timeout: float) -> int | None:
    """Give a child a bounded window to publish its pid; None if it never
    does. A `None` is a real answer — the child was killed before it
    could `exec` — not a failure, which is why this returns rather than
    raising."""
    try:
        return wait_until(lambda: read_pidfile(path), timeout, f"{path} to be written")
    except TimeoutError:
        return None


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


# ---------------------------------------------------------------------------
# One isolated session profile
# ---------------------------------------------------------------------------


@dataclass
class SessionEnv:
    """A throwaway session profile: its paths, its environment, and the
    processes this test started against it."""

    root: Path
    env: dict[str, str]
    socket: Path
    state_dir: Path
    log_dir: Path
    launch_cwd: Path
    binary: Path
    _pids: list[int] = field(default_factory=list)
    _procs: list[subprocess.Popen] = field(default_factory=list)

    # -- paths ------------------------------------------------------------
    @property
    def state_json(self) -> Path:
        return self.state_dir / "state.json"

    @property
    def log_file(self) -> Path:
        return self.log_dir / "roost.log"

    @property
    def runtime_dir(self) -> Path:
        return self.socket.parent

    def state(self) -> dict | None:
        """`state.json`'s CONTENT, re-read from disk. Every "the layout
        was flushed" assertion goes through this rather than through an
        op's return value — the op can be right while the write is
        not."""
        try:
            return json.loads(self.state_json.read_text())
        except FileNotFoundError:
            return None

    # -- clients ----------------------------------------------------------
    def client(self, timeout: float = 30.0) -> Roost:
        """A `client.Roost` on this session's socket. Always with a
        socket timeout: a wedged session must fail the test, not hang
        it."""
        return Roost(self.socket, timeout=scaled_timeout(timeout))

    def identify(self) -> dict:
        with self.client() as c:
            return c.call("session.identify")

    def answering(self) -> dict | None:
        try:
            return self.identify()
        except (OSError, RoostError):
            return None

    def wait_answering(self, timeout: float = 30.0) -> dict:
        return wait_until(self.answering, timeout, f"a session answering at {self.socket}")

    def wait_socket_gone(self, timeout: float = 30.0) -> None:
        wait_until(
            lambda: not self.socket.exists(), timeout, f"the socket {self.socket} to go away"
        )

    def wait_pid_gone(self, pid: int, timeout: float = 30.0) -> None:
        """Wait for `pid` to exit, then stop tracking it.

        Untracking on confirmed death is the pid-reuse fence: the OS
        recycles pid numbers, so a number we keep after the process is
        gone is a number teardown might signal at whatever took it over.
        Every test that kills or stops a daemon comes through here, which
        is what keeps `_pids` a list of processes we still plausibly own.
        """
        wait_until(lambda: not pid_alive(pid), timeout, f"pid {pid} to exit")
        self.untrack_pid(pid)

    def stop_over_the_wire(self) -> dict:
        """`session.stop` on a fresh connection; returns the reap
        report."""
        with self.client(timeout=90.0) as c:
            return c.call("session.stop")

    # -- process bookkeeping ---------------------------------------------
    #
    # Two kinds of process, two mechanisms. A `--foreground` session is a
    # direct child, so it is tracked by its `Popen` handle and torn down
    # through it — `wait`/`poll` on a handle cannot be fooled by pid
    # reuse. A daemonized session is a double-fork orphan: there is no
    # handle to hold, only the pid it reported, so that one is tracked as
    # a bare number and every signal to it is guarded by a name check.
    def track_pid(self, pid: int) -> int:
        self._pids.append(pid)
        return pid

    def untrack_pid(self, pid: int) -> None:
        while pid in self._pids:
            self._pids.remove(pid)

    def track_proc(self, proc: subprocess.Popen) -> subprocess.Popen:
        self._procs.append(proc)
        return proc

    # -- launching --------------------------------------------------------
    def command_env(self, **overrides: str) -> dict[str, str]:
        env = dict(self.env)
        env.update(overrides)
        return env

    def start_daemonized(self, *, timeout: float = 90.0, **overrides: str) -> "Launch":
        """`roost-session start` — fork, setsid, verdict down the pipe,
        parent relays it to stdout and exits."""
        proc = subprocess.run(
            [str(self.binary), "start"],
            cwd=str(self.launch_cwd),
            env=self.command_env(**overrides),
            capture_output=True,
            text=True,
            timeout=scaled_timeout(timeout),
        )
        # The launch contract is "exactly one line on stdout, whatever
        # happens" — that is what lets a caller read it without a parser.
        # The daemonized parent has exited by now, so its whole stdout is
        # in hand and the assertion is total, not a sample. It also
        # catches the subtler regression: the forked child shares this
        # stdout until it redirects to /dev/null, so a stray print in
        # that window would show up here as a second line.
        assert len(proc.stdout.splitlines()) == 1, (
            f"roost-session start must print exactly one stdout line, got "
            f"{proc.stdout!r} (stderr={proc.stderr!r})"
        )
        launch = Launch(
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=proc.stderr,
            verdict=Verdict.parse(_first_line(proc.stdout)),
        )
        if launch.verdict.kind == "ready" and launch.verdict.pid:
            self.track_pid(launch.verdict.pid)
        return launch

    def start_foreground(self, **overrides: str) -> "Foreground":
        """`roost-session start --foreground` — this process *is* the
        session; the verdict goes to its own stdout and it keeps
        serving.

        stderr goes to a file rather than a pipe: the console log tee
        writes there for as long as the session runs, and nobody is
        draining a pipe while a test waits on the socket.
        """
        # uuid, not a counter: the two-concurrent-starts case launches
        # from two threads, and a length-derived name would collide.
        errlog = self.root / f"foreground-{uuid.uuid4().hex[:8]}.stderr"
        with open(errlog, "wb") as handle:
            proc = subprocess.Popen(
                [str(self.binary), "start", "--foreground"],
                cwd=str(self.launch_cwd),
                env=self.command_env(**overrides),
                stdout=subprocess.PIPE,
                stderr=handle,
                text=True,
            )
        self.track_proc(proc)
        return Foreground(proc, errlog)

    def roostctl(self, *args: str, timeout: float = 120.0, **overrides: str):
        """Run `roostctl <args>` against this profile. `ROOST_SESSION_BIN`
        is already in the env, so `session start` launches *this* daemon
        rather than whatever is on PATH."""
        result = subprocess.run(
            [roostctl_binary(), *args],
            cwd=str(self.launch_cwd),
            env=self.command_env(**overrides),
            capture_output=True,
            text=True,
            timeout=scaled_timeout(timeout),
        )
        # `session start` prints the launcher's pid. Adopt it so a test
        # that fails between start and stop still has its daemon reaped
        # rather than leaked past the temp root's deletion.
        for line in result.stdout.splitlines():
            if line.startswith("launcher_reported_pid="):
                tail = line.removeprefix("launcher_reported_pid=").strip()
                if tail.isdigit():
                    self.track_pid(int(tail))
        return result

    # -- teardown ---------------------------------------------------------
    def teardown(self) -> None:
        """Ask any surviving session to stop, then prove every process
        this test started is gone, then delete the root.

        Signals before deletion, never the other way round: both instance
        locks are inodes, and removing them out from under a live daemon
        is exactly the double-instance the locks exist to prevent.

        Structured as nested `finally`s because the polite stop talks to
        a process the test may have left in any state at all. Anything it
        raises — a transport error, a malformed reply, an assertion from
        deep in the client — must not be able to skip the kill or the
        cleanup: a teardown that aborts halfway leaks a daemon *and* a
        `/tmp` root into whatever runs next.
        """
        try:
            self._polite_stop()
        finally:
            try:
                self._kill_everything()
            finally:
                shutil.rmtree(self.root, ignore_errors=True)

    def _polite_stop(self) -> None:
        """Give a still-serving session the chance to flush and reap.

        Best-effort by design, so the catch is deliberately broad: this
        is cleanup for a test that has already reached its verdict, and
        every failure mode here ends in the same place — the kill below.
        """
        try:
            if self.socket.exists() and self.answering() is not None:
                self.stop_over_the_wire()
        except Exception:  # noqa: BLE001 — cleanup, see the docstring
            pass

    def _kill_everything(self) -> None:
        """Terminate every process this test started, one bad handle at a
        time: a failure on one must not strand the rest, which is the
        whole reason the loops suppress rather than propagate."""
        for proc in self._procs:
            with contextlib.suppress(OSError, ValueError):
                _terminate_proc(proc)
        self._procs.clear()
        for pid in list(self._pids):
            with contextlib.suppress(OSError, ValueError):
                _terminate_pid(pid)
        self._pids.clear()


@dataclass(frozen=True)
class Launch:
    """The outcome of a daemonizing `roost-session start`."""

    returncode: int
    stdout: str
    stderr: str
    verdict: Verdict


class Foreground:
    """A `--foreground` session and the one line it says on stdout.

    The reader thread takes the readiness line and then keeps reading to
    EOF, so [`assert_single_stdout_line`] can hold the process to the
    same "exactly one line" contract the daemonized parent is held to.
    Reading to EOF also means nothing can wedge on a full stdout pipe if
    a regression ever made the session chattier.
    """

    def __init__(self, proc: subprocess.Popen, errlog: Path):
        self.proc = proc
        self.errlog = errlog
        self._line: str | None = None
        self._rest: str | None = None
        self._lock = threading.Lock()
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()

    def _read(self) -> None:
        line = self.proc.stdout.readline() if self.proc.stdout else ""
        with self._lock:
            self._line = line
        rest = self.proc.stdout.read() if self.proc.stdout else ""
        with self._lock:
            self._rest = rest

    def verdict(self, timeout: float = 60.0) -> Verdict:
        """Block until the readiness line lands. An empty read means the
        process exited without saying anything, which is itself a
        verdict-shaped failure."""

        def ready():
            with self._lock:
                return self._line

        wait_until(
            lambda: ready() is not None,
            timeout,
            f"a readiness line from {self.proc.pid}",
        )
        with self._lock:
            raw = self._line or ""
        if raw.strip() == "":
            return Verdict(
                "error",
                None,
                f"no readiness line; exit={self.proc.poll()} stderr={self.stderr_text()!r}",
            )
        return Verdict.parse(raw)

    def wait(self, timeout: float = 60.0) -> int:
        return self.proc.wait(timeout=scaled_timeout(timeout))

    def assert_single_stdout_line(self, timeout: float = 10.0) -> None:
        """Assert the session said its verdict and nothing else.

        Call after [`wait`]: the process has exited, so the reader thread
        is at EOF and what it holds is the complete stdout rather than a
        snapshot that more could still arrive behind.
        """
        self._reader.join(scaled_timeout(timeout))
        assert not self._reader.is_alive(), (
            f"stdout of foreground session {self.proc.pid} never reached EOF"
        )
        with self._lock:
            line, rest = self._line or "", self._rest or ""
        assert line.endswith("\n"), f"readiness line was not newline-terminated: {line!r}"
        assert rest == "", (
            "roost-session --foreground must print exactly one stdout line; it also "
            f"printed {rest!r}"
        )

    def stderr_text(self) -> str:
        try:
            return self.errlog.read_text(errors="replace")
        except OSError:
            return ""


def _first_line(text: str) -> str:
    return text.splitlines()[0] if text.strip() else ""


def _terminate_proc(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=scaled_timeout(10.0))
        return
    except subprocess.TimeoutExpired:
        pass
    proc.kill()
    try:
        proc.wait(timeout=scaled_timeout(10.0))
    except subprocess.TimeoutExpired:
        pass


def _process_name(pid: int) -> str | None:
    """The executable name behind `pid`, or None when it isn't ours to
    see. `ps -o comm=` prints the full path on macOS and the (15-char
    capped) comm on Linux; `roost-session` is 13 characters, so the
    basename compares equal on both."""
    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "comm="],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    command = result.stdout.strip()
    return Path(command).name if command else None


def _terminate_pid(pid: int) -> None:
    """SIGTERM then SIGKILL a *daemonized* session by pid.

    Bare pids are the one thing a double-fork leaves us: the launcher we
    ran is not the session's parent, so there is no `Popen` handle and no
    `waitpid`. Pid numbers are reused, so the name check is the fence —
    the same discipline `ui.py::_quit_iced_bundle` applies before
    signalling a bundle pid. A pid that no longer names a `roost-session`
    is treated as already dead rather than signalled.

    Residual window: the check and the signal are not atomic, so a pid
    recycled onto another `roost-session` in between would still be hit.
    Nothing short of a pidfd closes that, and the only `roost-session`
    processes on the box during a run are this test's own.
    """
    if _process_name(pid) != BIN_NAME:
        return
    with contextlib.suppress(ProcessLookupError, PermissionError):
        os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + scaled_timeout(10.0)
    while pid_alive(pid) and time.monotonic() < deadline:
        time.sleep(0.05)
    if _process_name(pid) == BIN_NAME:
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.kill(pid, signal.SIGKILL)


def roostctl_binary() -> str:
    """`roostctl`, built on demand — shared with the UI harness so a
    single build serves both lanes."""
    import util  # local: `util` imports pytest, which this module does not need

    return util.roostctl_path()


# ---------------------------------------------------------------------------
# Construction
# ---------------------------------------------------------------------------


def make_env(*, launch_cwd_name: str = "launch") -> SessionEnv:
    """Build a fresh, isolated session profile rooted in a temp dir."""
    binary = session_binary()
    label, namespace = _dir_names(_is_debug_build(binary))
    # `/tmp`, not `$TMPDIR`: a Unix socket path is capped at ~104 bytes
    # (`SUN_LEN`), and macOS's per-user `$TMPDIR`
    # (`/var/folders/xx/yyy…/T/`) spends most of that before the profile's
    # own `home/Library/Caches/<label>/roost.sock` is appended. `/tmp` is
    # sticky on both platforms, which is what keeps it legal as a
    # runtime-dir ancestor.
    #
    # Canonicalized: `validate_runtime_dir` refuses a socket directory
    # with a symlinked component, and `/tmp` reaches `/private/tmp`
    # through one on macOS.
    root = Path(tempfile.mkdtemp(prefix="roost-hs-", dir="/tmp")).resolve()

    home = root / "home"
    launch_cwd = root / launch_cwd_name
    env = {k: v for k, v in os.environ.items() if k not in _SANITIZE}
    env["HOME"] = str(home)
    # The one shell every restored tab gets. See the module docstring.
    env["SHELL"] = "/bin/sh"
    env["ROOST_SHELL_FEATURES"] = ""
    env["ROOST_SESSION_BIN"] = str(binary)
    env.setdefault("RUST_LOG", "info")

    if platform.system() == "Darwin":
        socket = home / "Library/Caches" / label / "roost.sock"
        state_dir = home / "Library/Application Support" / label
        log_dir = home / "Library/Logs" / label
        # The OS provides `~/Library/Caches`; `validate_runtime_dir`
        # creates only the leaf.
        socket.parent.parent.mkdir(parents=True, exist_ok=True)
    else:
        runtime = root / "run"
        data = root / "data"
        state = root / "state"
        cache = root / "cache"
        for directory in (runtime, data, state, cache):
            directory.mkdir(parents=True, exist_ok=True)
        env["XDG_RUNTIME_DIR"] = str(runtime)
        env["XDG_DATA_HOME"] = str(data)
        env["XDG_STATE_HOME"] = str(state)
        env["XDG_CACHE_HOME"] = str(cache)
        socket = runtime / namespace / "roost.sock"
        state_dir = data / namespace
        log_dir = state / namespace

    home.mkdir(parents=True, exist_ok=True)
    launch_cwd.mkdir(parents=True, exist_ok=True)

    return SessionEnv(
        root=root,
        env=env,
        socket=socket,
        state_dir=state_dir,
        log_dir=log_dir,
        launch_cwd=launch_cwd,
        binary=binary,
    )
