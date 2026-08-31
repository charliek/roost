"""The bootstrap offer, through the real UI (host-sessions HS-3, plan 039).

`test_host_ssh.py` proves the SSH transport with one thing faked — the
`ssh` binary itself, in `ok` mode, where the remote command is ignored
and a fixed program stands in for the far side. This module needs the
opposite: the remote command *is* the thing under test — a generated
`/bin/sh -s` script, a `tee` the binary streams into, an `exec` of
whatever the probe found — so it drives the fixture's `run-remote` mode,
which really runs the argv it is handed (`fixtures/fake-ssh.sh`'s header
is the contract).

# Hermeticity — the same three things plan 039 §3.8 pins

`run-remote`'s scripts execute on **this** machine, so a
[`BootstrapJail`] makes this machine invisible to them, exactly the way
`crates/roost-ipc/tests/bootstrap_test.rs`'s own harness does:

* `$HOME` is a tempdir, so `~/.local/bin/roost-session` — the install
  destination and rung 1 of the candidate ladder — lands inside it.
* `$PATH` is one directory this module builds: a fake `uname` that
  answers `Linux` + whatever architecture the test wants, symlinks to
  the handful of real coreutils the generated scripts run, and **no**
  `roost-session` — so the ladder's `command -v` rung can never find a
  real one, and (not incidentally) the PATH-warning scenario is just
  what every install in this jail already does.
* `ROOST_BOOTSTRAP_FS_ROOT` is a jail directory the candidate ladder
  prefixes onto its absolute rungs (`ROOST_TEST_MODE=1`, which this
  lane's Makefile target sets, is what turns that prefixing on at all),
  so a `/usr/bin/roost-session` probe reads
  `<jail>/usr/bin/roost-session` rather than this machine's real one.

A binary the bootstrap job *starts* inside the jail is a real
`roost-session` daemon — `roost-session identify`/`start`/`client-bridge`
answer for real, and a project + a tab come up for real
(`roost-session/src/hydrate.rs`'s first-ever-start seed). Its socket is
computed the same way `paths.rs` computes one from a bare `$HOME` (no
`XDG_*`, because `run-remote`'s `env -i` only ever hands the remote four
names): `~/Library/Caches/RoostSessionDev/roost.sock` on macOS,
`/tmp/roost-session-dev-<uid>/roost.sock` on Linux. Reading a jailed
tab's content is therefore done the way `test_host_ssh.py` does it too —
a direct connection to the session's own socket — but *writing* to one
uses a real shell's real `echo` rather than `tab.feed_pty_bytes`,
because a daemon the *job* starts never carries `ROOST_TEST_MODE=1`
(that name is not one of `run-remote`'s four either) and so has none of
the test-mode ops enabled. The one daemon this module seeds *directly*
(`start_daemon_in_jail`, scenario 5) does carry it — an override handed
past `run-remote` is exactly what that entry point exists for, and
`ROOST_SESSION_FAKE_BUILD` is gated on it.

# The seam this module needed that did not exist

Every dialog in this flow — the bootstrap offer and the pre-existing
upgrade prompt alike — is gated on `RequestOrigin::User`
(`host_notice::connect_route`, `bootstrap::offer_for`): only a real
sidebar/palette click carries it, and **both** `host.connect` and
`palette.activate` arriving over this IPC socket are `RequestOrigin::Ipc`
on purpose (plan 039 §3.5's never-a-modal-at-a-machine rule — `app/
servicing.rs`'s `IPC_CONNECT_ORIGIN` / `IPC_ACTIVATION_ORIGIN`). That
rule is exactly right for `roostctl`, and exactly what makes this an
IPC-only harness unable to reach the offer at all: unlike the
pre-existing restart prompt (which `test_host_client.py:726` sidesteps
by composing the ops its button runs — an escape hatch bootstrap
deliberately lacks, precisely because composing the ops is bypassing
the consent this whole plan is about), there was no way to raise the
*first* dialog to answer with `app.dialog_answer` at all.

So `HostConnectParams` grew one more `ROOST_TEST_MODE=1`-gated field,
`test_user_origin` (`crates/roost-ipc/src/messages.rs`,
`crates/roost-iced/src/app/servicing.rs::host_connect_op` — this
module's one deviation from its assigned file map, and called out in
the landing report). Set, it routes `host.connect` through
`host_connect_requested` — the same NeedsRestart-aware entry a click
uses — instead of the plain dial the op otherwise gives a machine.
Outside test mode the field is inert, so `roostctl`/production IPC
callers can never raise a modal by setting it (they have no reason to,
and the field is not documented anywhere they would read it). Scenario
10 below is the fence on the *original* rule: an ordinary `host.connect`
— what `roostctl` actually sends — must still never prompt.

A second seam closes an unrelated gap: `success.path_warning`
(`app.rs::host_bootstrap_finished`) used to reach only the ephemeral
status banner, with no op and no log line — unlike a *failed* job's
`report_bootstrap_failure`, which already logs `%message`. The
completion line now carries `%message` too, symmetrically, which is
what scenario 9 reads.

Condition waits only — with one deliberate exception. A claim of the
form "no dialog ever appears" has no condition to wait for, so
[`assert_no_dialog_for`] holds it for a bounded window instead; reading
it once would only prove "not yet".
"""

from __future__ import annotations

import atexit
import contextlib
import functools
import hashlib
import http.server
import os
import platform
import shutil
import subprocess
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

import pytest
import session as sessionlib
import ui
from client import Roost, RoostError, scaled_timeout

from eventstream import EventStream
from test_host_client import (
    SUBTITLE_NEEDS_RESTART,
    SUBTITLE_TAKEN_OVER,
    HostUnderTest,
    first_project,
    host_row,
    host_row_ids,
    marker,
    quiet_tab,
    saved_host,
    start_session,
    wait_dump_contains,
    wait_until,
)
from test_host_ssh import (
    FAKE_SSH_SESSION_ENV,
    NOT_FOUND_COPY,
    TUNNEL_FAILED,
    UiLog,
    _harness_owned_ui,  # noqa: F401  (autouse: this lane needs a harness-owned UI too)
    configure_fake_ssh,
    host_key,
    invocations,
    session_env,  # noqa: F401  (ssh_host's own dependency; pytest resolves it in *this* module)
    sh_quote,
    ssh_host,  # noqa: F401  (scenario 1 reuses it verbatim)
)

pytestmark = pytest.mark.host_client


@pytest.fixture(autouse=True)
def _requires_test_mode():
    """Unjailed, this module is not hermetic — it is destructive.

    `ROOST_TEST_MODE=1` is what turns the `ROOST_BOOTSTRAP_FS_ROOT`
    prefixing on at all (`bootstrap.rs::test_mode_env`). Without it the
    ladder's absolute rungs resolve against the *real* filesystem, so on
    a deb-installed box the probe answers about this developer's own
    `/usr/bin/roost-session` — and the failure mode is not an assertion
    but a wall of 90-second timeouts that says nothing about why. Skip
    up front instead; `make e2e-host-bootstrap` sets the variable.
    """
    if os.environ.get("ROOST_TEST_MODE") != "1":
        pytest.skip(
            "this lane needs ROOST_TEST_MODE=1 to jail the candidate ladder's absolute "
            "rungs; run it via `make e2e-host-bootstrap`"
        )


# ---------------------------------------------------------------------------
# Architecture — the same map `roost_ipc::bootstrap::map_arch` uses
# ---------------------------------------------------------------------------


def remote_arch() -> str:
    machine = platform.machine().lower()
    if machine in ("x86_64", "amd64"):
        return "amd64"
    if machine in ("aarch64", "arm64"):
        return "arm64"
    raise RuntimeError(f"the bootstrap jail doesn't know this machine's arch: {machine!r}")


def asset_name(version: str, arch: str) -> str:
    """`roost_ipc::bootstrap::asset_name` — restated deliberately, like
    `test_host_ssh.py`'s classified-copy constants: a change to the wire
    naming should have to be made twice, on purpose."""
    return f"roost-session-{version}-linux-{arch}"


# ---------------------------------------------------------------------------
# The jail: a fake $HOME + $PATH the fixture's `run-remote` mode executes
# real bootstrap scripts against.
# ---------------------------------------------------------------------------

#: The coreutils the generated scripts actually run
#: (`bootstrap_test.rs::TOOLS`, restated). `cat` and `sleep` are not
#: needed by any script this module runs, but keeping the same set the
#: Rust harness symlinks means a future scenario here never discovers a
#: missing tool as a test failure.
_JAIL_TOOLS = ("sh", "printf", "mkdir", "tee", "chmod", "mv", "rm", "cat", "sleep")

#: Where those symlinks are resolved from — not `$PATH`, for the same
#: reason the Rust harness's `TOOL_DIRS` isn't: this jail is about not
#: inheriting the developer's own environment.
_TOOL_DIRS = ("/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin")


def _find_tool(name: str) -> Path:
    for directory in _TOOL_DIRS:
        candidate = Path(directory) / name
        if os.access(candidate, os.X_OK):
            return candidate
    found = shutil.which(name)
    if found:
        return Path(found)
    raise FileNotFoundError(f"the bootstrap jail needs {name!r} on this machine")


def _write_executable(path: Path, content: str) -> None:
    path.write_text(content)
    path.chmod(0o755)


class BootstrapJail:
    """A throwaway `$HOME` + `$PATH`, mirroring `bootstrap_test.rs`'s
    `Harness` — same three env vars, same hermeticity contract, so the
    same scripts that pass there run here."""

    def __init__(self) -> None:
        # `/tmp`, not the default `$TMPDIR`, and canonicalized — the same
        # fix `session.py::make_env` already carries: macOS's per-user
        # `$TMPDIR` reaches through `/var`, itself a symlink to
        # `/private/var`, and `validate_runtime_dir` (rightly) refuses to
        # bind a socket under a redirectable ancestor. `/tmp` is sticky
        # and `.resolve()` collapses `/tmp` -> `/private/tmp` up front so
        # nothing built from `self.root` ever carries the symlink.
        self.root = Path(tempfile.mkdtemp(prefix="roost-bootstrap-jail-", dir="/tmp")).resolve()
        #: `ROOST_BOOTSTRAP_FS_ROOT`: what the ladder's absolute rungs
        #: resolve against.
        self.fs_root = self.root / "jail"
        #: The far side's `$HOME`.
        self.home = self.fs_root / "home" / "fixture"
        #: The far side's entire `$PATH`.
        self.stub_bin = self.root / "stub-bin"
        for directory in (self.fs_root, self.home, self.stub_bin):
            directory.mkdir(parents=True, exist_ok=True)
        if platform.system() == "Darwin":
            # `paths.rs`'s macOS resolver only creates the socket's own
            # leaf directory (`validate_runtime_dir`) — it does not
            # fabricate `~/Library/Caches` the way a real macOS user
            # account already has it. `session.py::make_env` carries the
            # identical `socket.parent.parent.mkdir(...)` for the same
            # reason: a jailed `$HOME` starts empty, so this jail has to
            # provide what the OS normally would.
            (self.home / "Library" / "Caches").mkdir(parents=True, exist_ok=True)
        for tool in _JAIL_TOOLS:
            os.symlink(_find_tool(tool), self.stub_bin / tool)
        self.set_uname("Linux", platform.machine())
        #: Daemons this module started *directly* (`start_daemon_in_jail`),
        #: by the pid their readiness verdict reported. The ones the
        #: bootstrap job starts over `run-remote` are double-fork orphans
        #: nothing here has a pid for — those are reached through
        #: [`stop_session`] alone.
        self.pids: list[int] = []

    def set_uname(self, os_name: str, machine: str) -> None:
        """Rewrite the fake `uname` — the OS a Darwin-remote scenario
        would need, or an arch a mismatch scenario wants to probe."""
        script = (
            "#!/bin/sh\n"
            'case "${1:-}" in\n'
            f"-s) printf '%s\\n' {sh_quote(os_name)} ;;\n"
            f"-m) printf '%s\\n' {sh_quote(machine)} ;;\n"
            f"*) printf '%s\\n' {sh_quote(os_name)} ;;\n"
            "esac\n"
        )
        _write_executable(self.stub_bin / "uname", script)

    def local(self, remote: str) -> Path:
        """A remote path as this side can open it. `$HOME/…` lands in
        the fake home; an absolute path lands in the jail — exactly as
        `ROOST_BOOTSTRAP_FS_ROOT` makes it (`bootstrap_test.rs::local`,
        restated)."""
        if remote.startswith("$HOME/"):
            return self.home / remote[len("$HOME/") :]
        return self.fs_root / remote.lstrip("/")

    def plant(self, remote: str, source: Path) -> Path:
        """Put a `roost-session` (or a stub standing in for one) at a
        remote path."""
        dest = self.local(remote)
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, dest)
        dest.chmod(0o755)
        return dest

    def plant_stub(self, remote: str, script: str) -> Path:
        dest = self.local(remote)
        dest.parent.mkdir(parents=True, exist_ok=True)
        _write_executable(dest, script)
        return dest

    def dest(self) -> Path:
        """Where an install lands — rung 1 of the candidate ladder."""
        return self.local("$HOME/.local/bin/roost-session")

    def session_env_text(self) -> str:
        """The far side's environment, sourced by the fixture before it
        runs a remote command (`FAKE_SSH_SESSION_ENV`) — the hermeticity
        contract, spelled the way `bootstrap_test.rs::write_session_env`
        spells it."""
        return "\n".join(
            [
                f"HOME={sh_quote(str(self.home))}",
                f"PATH={sh_quote(str(self.stub_bin))}",
                "USER=fixture",
                f"ROOST_BOOTSTRAP_FS_ROOT={sh_quote(str(self.fs_root))}",
                "export HOME PATH USER ROOST_BOOTSTRAP_FS_ROOT",
                "",
            ]
        )

    def daemon_env(self, **overrides: str) -> dict[str, str]:
        """The same four names `run-remote`'s `env -i` would hand a
        remote exec — for starting a daemon *directly* (not through the
        fixture), which is how this module pre-seeds a running session
        `run-remote`'s hardcoded four-name allowlist has no room for
        (e.g. `ROOST_SESSION_FAKE_BUILD`).

        Plus `ROOST_TEST_MODE`, forwarded from the harness's own
        environment: every override worth passing here is itself gated on
        it (`roost-session/src/consts.rs::FAKE_BUILD_ENV` — "read **only**
        when `ROOST_TEST_MODE=1`"), so a daemon started without it
        silently reports its *real* identity and a mismatch scenario
        connects cleanly instead. `session.py::make_env` gets this for
        free by inheriting `os.environ`; this dict is built from bare
        names, so it has to say so.
        """
        env = {
            "HOME": str(self.home),
            "PATH": str(self.stub_bin),
            "USER": "fixture",
            "ROOST_BOOTSTRAP_FS_ROOT": str(self.fs_root),
        }
        test_mode = os.environ.get("ROOST_TEST_MODE")
        if test_mode is not None:
            env["ROOST_TEST_MODE"] = test_mode
        env.update(overrides)
        return env

    def socket(self) -> Path:
        """Where a `roost-session` daemon started under `daemon_env()`
        binds, replicating `paths.rs`'s bare-`$HOME` default (no
        `XDG_*`) the same way `session.py`'s `_dir_names`/`_is_debug_build`
        already do for `make_env()`'s fuller environment.

        **On Linux this path is outside the jail and shared.**
        `paths.rs`'s Linux fallback is `/tmp/<namespace>-<uid>/roost.sock`
        — no `$HOME` in it — so every scenario in this module, and the
        X11 and Wayland steps of the same CI job, compute the *same*
        socket. That is why [`stop_session`] is run on the way in as
        well as on the way out: a daemon left holding it would make the
        next scenario's "it connected" vacuous.
        """
        debug = sessionlib._is_debug_build(sessionlib.session_binary())
        label, namespace = sessionlib._dir_names(debug)
        if platform.system() == "Darwin":
            return self.home / "Library" / "Caches" / label / "roost.sock"
        return Path(f"/tmp/{namespace}-{os.getuid()}") / "roost.sock"

    def stop_session(self, timeout: float = 30.0) -> None:
        """Stop whatever is serving on this jail's socket, and **wait for
        the socket itself to go**.

        `session.stop` replies before the daemon finalizes and unlinks —
        the exact race `session.py::wait_socket_gone` exists for — so
        returning on the reply hands the next scenario a socket a dying
        daemon still owns, under a `$HOME` about to be deleted.
        """
        socket_path = self.socket()
        if session_answering(socket_path) is not None:
            with contextlib.suppress(Exception):
                with Roost(str(socket_path), timeout=scaled_timeout(5.0)) as client:
                    client.call("session.stop")
        wait_until(
            lambda: not socket_path.exists() and session_answering(socket_path) is None,
            timeout,
            f"the jailed session socket {socket_path} to go away",
        )

    def track_pid(self, pid: int) -> int:
        self.pids.append(pid)
        return pid

    def kill_tracked(self) -> None:
        """The backstop `SessionEnv._kill_everything` is: SIGTERM then
        SIGKILL, guarded by the same process-name check, so a recycled
        pid number is never signalled."""
        for pid in self.pids:
            with contextlib.suppress(OSError, ValueError):
                sessionlib._terminate_pid(pid)
        self.pids.clear()

    def cleanup(self) -> None:
        """Stop, then kill, then delete — never the other way round.

        Nested `finally`s for `SessionEnv.teardown`'s reason: a polite
        stop that raises (including the "it never went away" timeout,
        which is a finding and not something to swallow) must not skip
        the kill or the removal.
        """
        try:
            self.stop_session()
        finally:
            try:
                self.kill_tracked()
            finally:
                shutil.rmtree(self.root, ignore_errors=True)


def assert_jail_never_installed(jail: BootstrapJail) -> None:
    """The sharp form of "the far side was left alone".

    `not jail.dest().exists()` is the weak form: the prepare script
    `mkdir -p "${dest%/*}"` and stages `<dest>.tmp.<pid>` *before*
    anything reaches `dest`, so a flow that prepared or streamed and
    then stopped leaves `dest` absent and the jail dirty. `~/.local` is
    the first thing prepare creates, and the session socket is what a
    start would bind — neither may exist.
    """
    local = jail.home / ".local"
    assert not local.exists(), (
        f"nothing may have run the prepare step, but {local} exists: "
        f"{sorted(path.name for path in jail.home.iterdir())}"
    )
    assert not jail.socket().exists(), "nothing may have started a session over there"


def session_answering(socket_path: Path) -> dict | None:
    """`session.identify` on a fresh connection, or `None` if nothing is
    there."""
    try:
        with Roost(str(socket_path), timeout=scaled_timeout(5.0)) as client:
            return client.call("session.identify")
    except (OSError, RoostError):
        return None


def start_daemon_in_jail(jail: BootstrapJail, binary: Path, **env_overrides: str) -> Path:
    """Start a real `roost-session` daemon directly (not through the
    fixture), with the jail's env plus whatever this call adds. Returns
    once it is answering.

    This is how a scenario gets a session *already running* inside the
    jail before the UI ever dials it — the bootstrap job's own `start`
    step (run through `run-remote`) can only ever hand a fresh daemon
    the jail's bare four names, so a running mismatch
    (`ROOST_SESSION_FAKE_BUILD`) has to be seeded this way instead.

    The pid the verdict reports is tracked on the jail, so teardown has
    something to signal when the polite `session.stop` does not take.
    """
    result = subprocess.run(
        [str(binary), "start"],
        cwd=str(jail.home),
        env=jail.daemon_env(**env_overrides),
        capture_output=True,
        text=True,
        timeout=scaled_timeout(60),
    )
    verdict = sessionlib.Verdict.parse((result.stdout or "").strip())
    assert verdict.kind == "ready", (result.returncode, result.stdout, result.stderr)
    if verdict.pid is not None:
        jail.track_pid(verdict.pid)
    socket_path = jail.socket()
    wait_until(
        lambda: session_answering(socket_path),
        30.0,
        f"a jailed session answering at {socket_path}",
    )
    return socket_path


# ---------------------------------------------------------------------------
# The asset server — the download rung's fixture (plan 039 §3.8)
# ---------------------------------------------------------------------------


class _AssetServer:
    """A loopback `http.server` standing in for a GitHub release, in the
    shape `test_sparkle.py::_FeedServer` already established: a tempdir
    whose contents this module rewrites between scenarios, started at
    import time so its URL is in the launched UI's environment.

    Like `_FeedServer` it keeps a per-request log, and for the reason
    that one does: without it, "the bytes came over the wire" is not
    observable anywhere. Every happy-path scenario here serves a binary
    byte-identical to what the *sibling* rung would install, so if
    `ROOST_BOOTSTRAP_SOURCE=asset` ever stopped being honored they would
    install the sibling and pass unchanged. The log is what makes them
    fail instead.
    """

    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory(prefix="roost-bootstrap-asset-")
        self.root = Path(self._dir.name)
        #: Every request path served since the last `_clear()`. Appended
        #: from `ThreadingHTTPServer` worker threads; `list.append` is
        #: what makes that safe with no lock.
        self.requests: list[str] = []
        served = self.requests

        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, fmt: str, *args) -> None:  # noqa: A002
                pass

            def do_GET(self) -> None:
                served.append(self.path)
                super().do_GET()

            def do_HEAD(self) -> None:
                served.append(self.path)
                super().do_HEAD()

        self._server = http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), functools.partial(Handler, directory=str(self.root))
        )
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self._server.server_address[1]}"

    def _clear(self) -> None:
        for existing in self.root.iterdir():
            existing.unlink()
        self.requests.clear()

    def fetched(self, name: str) -> bool:
        return f"/{name}" in self.requests

    def serve_valid(self, name: str, binary_bytes: bytes) -> None:
        """A real, correctly checksummed asset — the happy-path
        download."""
        self._clear()
        asset_path = self.root / name
        asset_path.write_bytes(binary_bytes)
        asset_path.chmod(0o755)
        digest = hashlib.sha256(binary_bytes).hexdigest()
        (self.root / f"{name}.sha256").write_text(f"{digest}  {name}\n")

    def serve_corrupted(self, name: str, binary_bytes: bytes) -> None:
        """A well-formed `.sha256` record that names the wrong hash —
        the checksum-failure lane. A parse failure would prove nothing
        about the *comparison* the trust chain actually makes."""
        self._clear()
        asset_path = self.root / name
        asset_path.write_bytes(binary_bytes)
        asset_path.chmod(0o755)
        digest = hashlib.sha256(binary_bytes + b"corrupt").hexdigest()
        (self.root / f"{name}.sha256").write_text(f"{digest}  {name}\n")

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5.0)
        self._dir.cleanup()


# At import time, before the session-scoped UI fixture launches anything
# — `ROOST_SESSION_ASSET_BASE`/`ROOST_BOOTSTRAP_SOURCE` have to be in the
# launched process's own environment, the same reason `ROOST_SSH_BIN`
# is set at `test_host_ssh` import time. Forcing the asset rung
# (`ROOST_BOOTSTRAP_SOURCE=asset`, §3.8) is what lets this module reach
# the download+checksum path on any machine — an unforced ladder would
# let a compatible sibling win locally and never touch the wire.
_ASSET_SERVER = _AssetServer()
os.environ["ROOST_SESSION_ASSET_BASE"] = _ASSET_SERVER.base
os.environ["ROOST_BOOTSTRAP_SOURCE"] = "asset"
# At interpreter exit rather than in a fixture, matching `test_host_ssh`'s
# own `atexit`: the server's URL is baked into the launched UI's
# environment, so it has to outlive every test *and* that UI.
atexit.register(_ASSET_SERVER.stop)

# Same place, same reason, one more name: a bootstrap job's *success* line
# is `tracing::info!` (`app.rs::host_bootstrap_finished`), and it is the
# only surface carrying the PATH warning scenario 9 asserts on — a failed
# job's `warn!` needs nothing here, which is why scenario 6 does not. The
# bare-binary launch path defaults `RUST_LOG` to `warn`
# (`ui.py::_launch`), and CI's e2e steps set exactly that, so without this
# floor the line the assertion waits for is never written. Spelled the way
# `ui.py`'s bundle launch already spells its own floor: an operator filter
# that already names `roost_iced` is left alone.
_rust_log = os.environ.get("RUST_LOG")
if _rust_log is None:
    os.environ["RUST_LOG"] = "warn,roost_iced=info"
elif "roost_iced" not in _rust_log:
    os.environ["RUST_LOG"] = f"{_rust_log},roost_iced=info"


# ---------------------------------------------------------------------------
# Dialog helpers — the `app.dialog_dump` / `app.dialog_answer` test seam
# ---------------------------------------------------------------------------


def dialog_dump(roost: Roost) -> dict:
    return roost.call("app.dialog_dump", {})


def wait_dialog(roost: Roost, kind: str, variant: str | None = None, timeout: float = 90.0) -> dict:
    def probe():
        dump = dialog_dump(roost)
        if dump.get("dialog") == kind and (variant is None or dump.get("variant") == variant):
            return dump
        return None

    return wait_until(probe, timeout, f"the {kind!r} dialog (variant {variant!r})")


def wait_no_dialog(roost: Roost, timeout: float = 15.0) -> None:
    wait_until(lambda: dialog_dump(roost).get("dialog") is None, timeout, "no host dialog to be open")


def assert_no_dialog_for(roost: Roost, seconds: float = 3.0) -> None:
    """Hold "no dialog" for a window, rather than reading it once.

    A single read the instant a failure is logged proves only "no dialog
    *yet*": the offer is raised off an async probe round trip, so a
    regression that dropped the origin gate would raise the card seconds
    later and a one-shot assertion would still pass.
    """
    deadline = time.monotonic() + scaled_timeout(seconds)
    while True:
        dump = dialog_dump(roost)
        assert dump.get("dialog") is None, dump
        if time.monotonic() >= deadline:
            return
        time.sleep(0.1)


# ---------------------------------------------------------------------------
# Reading the fake `ssh` invocation log
# ---------------------------------------------------------------------------


def is_probe_exec(argv: list[str]) -> bool:
    """A bootstrap script exec: its remote command is the `/bin/sh -s`
    that reads a generated script off stdin
    (`roost_ipc::bootstrap::SH_STDIN`)."""
    return len(argv) > 1 and argv[-1] == "/bin/sh -s"


def is_job_master_exit(argv: list[str]) -> bool:
    """`ssh -O exit` against a **bootstrap job's** own control socket.

    The job's scratch directory is `roost-ssh-bootstrap-…`
    (`ssh::one_shot_dir_name(JOB_DIR_KIND)`), which is what tells its
    master apart from a tunnel's.
    """
    if not any(argv[i] == "-O" and argv[i + 1] == "exit" for i in range(len(argv) - 1)):
        return False
    return any(
        argv[i] == "-S" and "/roost-ssh-bootstrap-" in argv[i + 1] for i in range(len(argv) - 1)
    )


def ssh_argvs() -> list[list[str]]:
    """Every fake-`ssh` invocation so far, argv only (the fixture's
    leading `pid=` field dropped)."""
    return [fields[1:] for fields in invocations()]


def answer(roost: Roost, action: str) -> dict:
    return roost.call("app.dialog_answer", {"action": action})


def connect_as_user(roost: Roost, saved_id: str) -> dict:
    """`host.connect`, but carrying the test-only seam this module added
    (`HostConnectParams::test_user_origin`) so the connect is treated as
    a person's click — the only way anything in this flow ever opens a
    dialog. See the module docstring for why that seam had to exist."""
    return roost.call("host.connect", {"id": saved_id, "test_user_origin": True})


CONNECT_STARTED = ("disconnected", "connecting", "connected")


# ---------------------------------------------------------------------------
# The saved ssh host under test
# ---------------------------------------------------------------------------


@dataclass
class BootstrapHost:
    roost: Roost
    saved_id: str
    label: str
    jail: BootstrapJail

    def connect(self) -> dict:
        return connect_as_user(self.roost, self.saved_id)

    def connect_started(self) -> dict:
        """`connect()`, plus the "did it even begin" assertion every
        scenario below opens with — same boilerplate, one call."""
        result = self.connect()
        assert result["state"] in CONNECT_STARTED, result
        return result

    def remove(self) -> None:
        self.roost.call("host.remove", {"id": self.saved_id})

    def wait_connected(self, timeout: float = 60.0) -> None:
        wait_until(
            lambda: f"host:disconnect:{self.saved_id}" in host_row_ids(self.roost),
            timeout,
            f"{self.label} to reach connected",
        )

    def wait_not_connected(self, timeout: float = 30.0) -> None:
        wait_until(
            lambda: f"host:connect:{self.saved_id}" in host_row_ids(self.roost),
            timeout,
            f"{self.label} to stay disconnected",
        )

    def wait_connect_subtitle(self, subtitle: str, timeout: float = 60.0) -> None:
        def settled() -> bool:
            row = host_row(self.roost, f"host:connect:{self.saved_id}")
            return row is not None and row.get("subtitle") == subtitle

        wait_until(settled, timeout, f"{self.label} to offer Connect: {subtitle!r}")


@pytest.fixture
def jail():
    made = BootstrapJail()
    # On the way in as well as on the way out. The session socket a
    # jailed daemon binds is derived from a bare `$HOME`, which on Linux
    # means a `/tmp` path with no `$HOME` in it: identical for every
    # scenario here and for both the X11 and Wayland bootstrap steps of
    # one CI job. A survivor holding it would let the next scenario's
    # "it connected" mean nothing, so anything still there is stopped
    # before this test begins — and if it will not go, that is the
    # finding, not a mystery timeout three scenarios later.
    made.stop_session()
    try:
        yield made
    finally:
        made.cleanup()


@pytest.fixture
def bootstrap_host(roost: Roost, jail: BootstrapJail):
    """A saved ssh host reachable only through `run-remote` against
    `jail` — not yet connected, nothing planted."""
    configure_fake_ssh("run-remote")
    FAKE_SSH_SESSION_ENV.write_text(jail.session_env_text())
    label = f"bs-{uuid.uuid4().hex[:8]}"
    added = roost.call("host.add", {"label": label, "target": f"ssh://{label}.invalid"})["host"]
    host = BootstrapHost(roost=roost, saved_id=added["id"], label=label, jail=jail)
    # The incarnation probe's guard, restated from `test_host_ssh.ssh_host`:
    # two connected hosts can share a tab number.
    #
    # `fail`, not `skip`, and not read once. This lane only ever drives a
    # UI it launched itself (`_harness_owned_ui`), so unlike
    # `test_host_ssh` — which can legitimately meet a developer's own
    # instance — a connected host in here is *this module's* leftover,
    # i.e. a bug. Skipping it turned scenarios 2-9 into silent green and
    # CI reported success. The short wait is for a disconnect that has
    # not settled yet, which is a race rather than a leak.
    def others() -> set[str]:
        return {
            row.removeprefix("host:disconnect:")
            for row in host_row_ids(roost)
            if row.startswith("host:disconnect:")
        } - {host.saved_id}

    try:
        wait_until(lambda: not others(), 15.0, "no other host to be connected")
    except TimeoutError:
        pytest.fail(
            f"another host is still connected ({sorted(others())}); the incarnation "
            "probe cannot tell two connected hosts' tabs apart, so this lane's "
            "scenarios would be vacuous"
        )
    try:
        yield host
    finally:
        roost.palette_dismiss()
        # Not suppressed, and awaited. A `remove()` that throws — or a
        # disconnect that has not settled — leaves a connected host
        # behind, and the guard above is only as good as this is.
        host.remove()
        rows = (f"host:connect:{host.saved_id}", f"host:disconnect:{host.saved_id}")
        wait_until(
            lambda: not set(rows) & set(host_row_ids(roost)),
            30.0,
            f"{host.label} to leave the palette entirely",
        )


@pytest.fixture
def valid_asset(roost: Roost) -> str:
    """Serve this run's own `roost-session` binary as the release asset
    — the source every install-happy-path scenario below resolves to."""
    version = roost.identify()["ui_version"]
    name = asset_name(version, remote_arch())
    _ASSET_SERVER.serve_valid(name, sessionlib.session_binary().read_bytes())
    return name


@pytest.fixture
def corrupted_asset(roost: Roost) -> str:
    version = roost.identify()["ui_version"]
    name = asset_name(version, remote_arch())
    _ASSET_SERVER.serve_corrupted(name, sessionlib.session_binary().read_bytes())
    return name


#: A `roost-session` stub old/wrong enough to answer `identify` with a
#: triple that never matches, but real enough to answer `client-bridge`
#: honestly — it is only ever probed or installed over, never started.
def _mismatched_stub() -> str:
    return (
        "#!/bin/sh\n"
        'case "${1:-}" in\n'
        "identify)\n"
        "  printf '%s\\n' "
        + sh_quote('{"app_version":"0.0.1","session_protocol":1,"libghostty_build":"stale"}')
        + "\n"
        "  exit 0\n"
        "  ;;\n"
        "client-bridge)\n"
        # No `read` here: the real bridge (`roost-session/src/bridge.rs`)
        # answers "no session" from a *startup*-time check, before it
        # ever reads a byte from stdin — a client that sent nothing yet
        # (the common case; the caller sends its first frame only after
        # dialing) would otherwise leave this stub blocked on `read`
        # forever, past any of this suite's own timeouts.
        "  printf '%s\\n' 'client-bridge: no session' >&2\n"
        "  exit 1\n"
        "  ;;\n"
        "*)\n"
        "  printf 'stub: unknown subcommand\\n' >&2\n"
        "  exit 2\n"
        "  ;;\n"
        "esac\n"
    )


# ---------------------------------------------------------------------------
# 1. Preinstalled + compatible: connects, no offer (AC8's baseline)
# ---------------------------------------------------------------------------


def test_preinstalled_compatible_connects_with_no_dialog(ssh_host, roost):
    """A host that already works must never see a bootstrap card — the
    zero-regression baseline plan 038's own transport already proved,
    restated here as this module's first fence. `ssh_host` (fake `ssh`
    in `ok` mode, bridging straight to an already-running local session)
    is exactly plan 038's happy path; nothing about it should route
    anywhere near the offer."""
    assert dialog_dump(roost).get("dialog") is None
    result = ssh_host.connect()
    assert result["state"] in CONNECT_STARTED, result
    ssh_host.wait_connected()
    assert dialog_dump(roost).get("dialog") is None


# ---------------------------------------------------------------------------
# 2. Missing -> install -> start -> attach works
# ---------------------------------------------------------------------------


def test_missing_binary_offers_install_and_attaches_after_confirm(
    bootstrap_host: BootstrapHost, roost: Roost, valid_asset: str
):
    """AC: the primary path. Nothing is over there, the card says so
    honestly (what/where/from-where), confirming installs for real and
    starts it, and the reconnect lands on a tab that really renders —
    proven the same way `test_host_ssh`'s happy path proves it: bytes a
    real shell echoed, read back through the UI's own dump.
    """
    result = bootstrap_host.connect_started()

    dump = wait_dialog(roost, "bootstrap", "install")
    assert dump["host"] == bootstrap_host.saved_id
    # What.
    assert "roost-session" in dump["body"], dump
    # Where.
    assert "~/.local/bin/roost-session" in dump["body"], dump
    assert bootstrap_host.label in dump["title"] or bootstrap_host.label in dump["body"], dump
    # From where — named honestly, never masked as github.com, since the
    # asset base is overridden to the loopback fixture server.
    assert "downloaded from" in dump["body"], dump
    assert "checksum-verified" in dump["body"], dump
    assert "127.0.0.1" in dump["body"], dump
    assert dump["buttons"] == ["Cancel", "Install"], dump

    answer(roost, "confirm")
    wait_no_dialog(roost)
    bootstrap_host.wait_connected()

    dest = bootstrap_host.jail.dest()
    assert dest.is_file(), "the install must have landed at ~/.local/bin/roost-session"
    assert os.access(dest, os.X_OK), "the installed binary must be executable"
    # Strictly stronger than a size check: the installed file *is* the
    # source, byte for byte.
    assert dest.read_bytes() == sessionlib.session_binary().read_bytes()

    # …and it came over the loopback server rather than off the sibling
    # rung, which serves identical bytes. Only the request log can tell
    # those two apart (`ROOST_BOOTSTRAP_SOURCE=asset` is what forces the
    # download rung; if it stopped being honored this scenario would
    # otherwise pass unchanged).
    assert _ASSET_SERVER.fetched(valid_asset), _ASSET_SERVER.requests
    assert _ASSET_SERVER.fetched(f"{valid_asset}.sha256"), _ASSET_SERVER.requests

    socket_path = bootstrap_host.jail.socket()
    with Roost(str(socket_path), timeout=scaled_timeout(30.0)) as session:
        project = first_project(session)
        line = marker("BOOTSTRAPPED")
        tab = session.open_tab(
            project,
            cwd=str(bootstrap_host.jail.home),
            cols=80,
            rows=24,
            argv=["/bin/sh", "-c", f"echo {line}; exec sleep 300"],
        )
        key = host_key(roost, tab)
        wait_dump_contains(roost, key, line)


# ---------------------------------------------------------------------------
# 3. NoSession + compatible -> Start variant -> running
# ---------------------------------------------------------------------------


def test_nosession_compatible_offers_start_only(bootstrap_host: BootstrapHost, roost: Roost):
    """A deb-shaped host: the binary is already right, but nothing is
    serving, and it lives at `/usr/bin` — NOT `~/.local/bin` — so this
    also fences the case a start-only flow must exec the probe-resolved
    path rather than a `~/.local/bin` that was never written.
    """
    bootstrap_host.jail.plant("/usr/bin/roost-session", sessionlib.session_binary())

    result = bootstrap_host.connect_started()

    dump = wait_dialog(roost, "bootstrap", "start")
    assert "/usr/bin/roost-session" in dump["body"], dump
    assert "will be installed" not in dump["body"], dump
    assert dump["buttons"] == ["Cancel", "Start"], dump

    answer(roost, "confirm")
    wait_no_dialog(roost)
    bootstrap_host.wait_connected()

    # Nothing was ever written to the install destination — a start-only
    # plan installs nothing.
    assert not bootstrap_host.jail.dest().exists()


# ---------------------------------------------------------------------------
# 4. NoSession + mismatched on-disk -> installs and starts, no stop
# ---------------------------------------------------------------------------


def test_nosession_mismatch_installs_over_the_stale_binary(
    bootstrap_host: BootstrapHost, roost: Roost, valid_asset: str
):
    """Row 2 of the action matrix: a stale binary with nothing running.
    The card must not warn about shells ending — there is no session to
    end — and the job installs + starts with no stop step at all (the
    matrix's own reason: there is nothing over there to stop).
    """
    bootstrap_host.jail.plant_stub("$HOME/.local/bin/roost-session", _mismatched_stub())

    result = bootstrap_host.connect_started()

    dump = wait_dialog(roost, "bootstrap", "update")
    # The stale binary is at rung 1 — the very file the install
    # overwrites and backs up — so the card owes the precise wording,
    # not the "goes ahead of a copy left where it is" phrasing that
    # belongs to a *different* rung. Getting this right needs the
    # remote's own `$HOME`, which the probe now carries back
    # (`bootstrap.rs::Probe::home` → `app/bootstrap.rs::dest_on_disk`):
    # `plan.found` is shell-expanded and absolute, and comparing it to
    # `card_dest`'s reader-facing `~/…` never matched, which made this
    # arm dead in production.
    assert "will replace what is at ~/.local/bin/roost-session" in dump["body"], dump
    assert "goes ahead of" not in dump["body"], dump
    assert "left where it is" not in dump["body"], dump
    assert "shells running in it end" not in dump["body"], dump

    answer(roost, "confirm")
    wait_no_dialog(roost)
    bootstrap_host.wait_connected()

    dest = bootstrap_host.jail.dest()
    assert dest.is_file() and os.access(dest, os.X_OK)
    assert dest.read_bytes() == sessionlib.session_binary().read_bytes()


# ---------------------------------------------------------------------------
# 5. Running mismatch -> NeedsRestart -> Update -> stop/await/start/reconnect
# ---------------------------------------------------------------------------


FAKE_BUILD = "ghostty-0000000000000000+fake.plan039"


def test_running_mismatch_offers_remote_update_and_reconnects(
    bootstrap_host: BootstrapHost, roost: Roost
):
    """The remote branch of the pre-existing upgrade prompt
    (`RestartAction::OfferRemoteUpdate`): a real session is up, started
    under `ROOST_SESSION_FAKE_BUILD` so this client cannot talk to it.
    Confirming "Update roost-session on <label>" runs the probe (the
    on-disk binary really is this build — only the *process* is stale),
    which is the Compatible+Running matrix row: stop, await-gone, start,
    reconnect, with no install at all.
    """
    binary = bootstrap_host.jail.plant("$HOME/.local/bin/roost-session", sessionlib.session_binary())
    start_daemon_in_jail(bootstrap_host.jail, binary, ROOST_SESSION_FAKE_BUILD=FAKE_BUILD)

    # First connect: the compat check that lands the host in
    # `NeedsRestart` is not itself origin-gated (only *raising the
    # prompt* is) — `host_connect_requested` only prompts when it is
    # already there, so a first-ever connect always just dials.
    result = bootstrap_host.connect_started()
    bootstrap_host.wait_connect_subtitle(SUBTITLE_NEEDS_RESTART)

    # Second connect: now the same "click" reaches `ConnectRoute::Prompt`.
    bootstrap_host.connect()
    restart = wait_dialog(roost, "confirm_restart")
    assert f"Update roost-session on {bootstrap_host.label}" in restart["buttons"], restart

    answer(roost, "confirm")
    update = wait_dialog(roost, "bootstrap", "update")
    assert "Nothing will be installed" in update["body"], update

    answer(roost, "confirm")
    wait_no_dialog(roost)
    bootstrap_host.wait_connected()


# ---------------------------------------------------------------------------
# 6. Checksum failure -> classified, host untouched
# ---------------------------------------------------------------------------


def test_checksum_failure_leaves_the_jail_untouched(
    bootstrap_host: BootstrapHost, roost: Roost, corrupted_asset: str, target
):
    """A hash that does not match must stop the install *before* the
    bytes go anywhere, so the destination is not the only thing to
    assert on: prepare `mkdir -p`s the destination's parent and stages
    `<dest>.tmp.<pid>` before anything reaches `dest` at all, so a
    regression that streamed first and verified after would leave the
    jail dirty and a bare `not dest.exists()` would still pass.
    """
    result = bootstrap_host.connect_started()

    wait_dialog(roost, "bootstrap", "install")
    log = UiLog(target, "bootstrap failed")
    answer(roost, "confirm")
    line = log.wait_next()
    assert "checksum" in line.lower(), line

    assert not bootstrap_host.jail.dest().exists(), "a checksum failure must install nothing"
    assert_jail_never_installed(bootstrap_host.jail)
    bootstrap_host.wait_not_connected()


# ---------------------------------------------------------------------------
# 7. Cancel -> nothing mutated
# ---------------------------------------------------------------------------


def test_cancel_mutates_nothing(bootstrap_host: BootstrapHost, roost: Roost, valid_asset: str):
    """Cancel means the far side is exactly as it was — and the probe's
    own ssh master is gone with it.

    `not dest.exists()` alone would not say that: prepare creates the
    destination's parent and stages a `<dest>.tmp.<pid>` before anything
    reaches `dest`, so a regression that ran prepare/stream *before*
    asking would leave the jail dirty and still pass. And the read-only
    probe holds a `ControlPersist` master of its own, which a user
    taking a minute over the card must not be left paying for.
    """
    before = len(ssh_argvs())
    result = bootstrap_host.connect_started()

    wait_dialog(roost, "bootstrap", "install")
    answer(roost, "cancel")
    wait_no_dialog(roost)

    assert not bootstrap_host.jail.dest().exists(), "cancel must install nothing"
    assert_jail_never_installed(bootstrap_host.jail)
    assert not _ASSET_SERVER.requests, (
        "a card nobody confirmed must not have fetched anything",
        _ASSET_SERVER.requests,
    )
    assert any(is_job_master_exit(argv) for argv in ssh_argvs()[before:]), ssh_argvs()[before:]
    bootstrap_host.wait_not_connected()
    assert dialog_dump(roost).get("dialog") is None


# ---------------------------------------------------------------------------
# 8. Unix-socket remote target -> NeedsRestart keeps the docs-pointer
# ---------------------------------------------------------------------------


def test_unix_socket_remote_keeps_the_docs_pointer_with_no_update_button(roost: Roost):
    """`RestartAction::None`: a saved host whose target is a bare socket
    path is nobody's ssh session to update, so the prompt has to say so
    and offer nothing — the one card in this whole flow with no primary
    button at all.
    """
    session_env = sessionlib.make_env()
    try:
        start_session(session_env, ROOST_SESSION_FAKE_BUILD=FAKE_BUILD)
        with saved_host(roost, session_env) as host:
            # First connect settles NeedsRestart (the compat check is
            # not origin-gated); the second, from a "click", reaches
            # `ConnectRoute::Prompt` and raises the dialog.
            result = connect_as_user(roost, host.saved_id)
            assert result["state"] in CONNECT_STARTED, result
            host.wait_connect_subtitle(SUBTITLE_NEEDS_RESTART)
            connect_as_user(roost, host.saved_id)

            dump = wait_dialog(roost, "confirm_restart")
            assert dump["buttons"] == ["Close"], dump
            assert "Only the machine running it can restart it" in dump["body"], dump
            assert "roostctl session stop" in dump["body"], dump

            with pytest.raises(RoostError):
                answer(roost, "confirm")

            answer(roost, "cancel")
            wait_no_dialog(roost)
    finally:
        session_env.teardown()


# ---------------------------------------------------------------------------
# 9. PATH-warning suffix
# ---------------------------------------------------------------------------


def test_install_warns_when_the_non_interactive_path_misses_local_bin(
    bootstrap_host: BootstrapHost, roost: Roost, valid_asset: str, target
):
    """This module's jail never puts `~/.local/bin` on the far side's
    `$PATH` (§ module docstring), so every successful install here is
    already this scenario — the completion line the app now logs
    symmetrically with a failed job's (`%message` on both,
    `app.rs::host_bootstrap_finished` / `report_bootstrap_failure`) is
    what makes it observable at all.
    """
    result = bootstrap_host.connect_started()

    wait_dialog(roost, "bootstrap", "install")
    log = UiLog(target, "roost-session is set up; reconnecting")
    answer(roost, "confirm")
    line = log.wait_next()
    assert "isn't on" in line and "PATH" in line, line


# ---------------------------------------------------------------------------
# 10. Non-interactive refusal: roostctl never prompts, never mutates
# ---------------------------------------------------------------------------


def test_roostctl_never_prompts_on_a_not_found_target(roost: Roost, target):
    """The rule scenarios 2-9 exist to exercise the *other* side of:
    `host.connect` and `host add --verify`, as `roostctl` actually sends
    them, must still classify a `NotFound` target as a failure and raise
    nothing — this module's own `test_user_origin` seam is opt-in and
    `roostctl` never sets it.

    Two claims, and the weaker one alone will not do. "No card" is held
    for a window rather than read once: the offer is raised only after an
    async probe round trip, so a single read the instant the failure is
    logged proves "no dialog *yet*", not "no dialog" — a regression that
    dropped the origin gate would raise the card seconds later and pass.
    The stronger claim is that no probe ran **at all**, which is what the
    fake-`ssh` invocation log says: an IPC connect must never exec the
    discovery script (`/bin/sh -s`) in the first place.
    """
    label = f"bs-cli-{uuid.uuid4().hex[:8]}"
    added = roost.call("host.add", {"label": label, "target": f"ssh://{label}.invalid"})["host"]
    saved_id = added["id"]
    try:
        configure_fake_ssh("exit-127")
        log = UiLog(target, TUNNEL_FAILED)
        before = len(ssh_argvs())
        result = subprocess.run(
            [
                sessionlib.roostctl_binary(),
                "--socket",
                str(ui.socket_path(target)),
                "host",
                "connect",
                "--id",
                saved_id,
            ],
            capture_output=True,
            text=True,
            timeout=scaled_timeout(60),
        )
        assert result.returncode == 0, result
        line = log.wait_next()
        assert NOT_FOUND_COPY in line, line
        assert_no_dialog_for(roost)
        rows = host_row_ids(roost)
        assert f"host:connect:{saved_id}" in rows
        assert f"host:disconnect:{saved_id}" not in rows

        verify_label = f"bs-cli-verify-{uuid.uuid4().hex[:8]}"
        add_result = subprocess.run(
            [
                sessionlib.roostctl_binary(),
                "--socket",
                str(ui.socket_path(target)),
                "host",
                "add",
                "--label",
                verify_label,
                "--target",
                f"ssh://{verify_label}.invalid",
                "--verify",
            ],
            capture_output=True,
            text=True,
            timeout=scaled_timeout(60),
        )
        assert add_result.returncode != 0, add_result
        combined = add_result.stdout + add_result.stderr
        assert NOT_FOUND_COPY in combined, combined
        assert "connect from the Roost app to install it" in combined, combined
        assert_no_dialog_for(roost)
        labels = {h["label"] for h in roost.call("host.list", {})["hosts"]}
        assert verify_label not in labels, "a failed --verify must never save the host"

        # Neither op may have probed. `--verify` does run one `ssh` of
        # its own, but its remote command is the exec chain — the
        # discovery script's `/bin/sh -s` belongs to the bootstrap job
        # alone, and no machine-driven op is allowed to reach it.
        probed = [argv for argv in ssh_argvs()[before:] if is_probe_exec(argv)]
        assert not probed, probed
    finally:
        with contextlib.suppress(Exception):
            roost.call("host.remove", {"id": saved_id})


# ---------------------------------------------------------------------------
# 11. Issue #376 regression: refuse paste into a frozen host frame
# ---------------------------------------------------------------------------


@pytest.mark.xfail(
    strict=True,
    reason=(
        "plan 039 C9 (§3.11) has not landed: paste_into_active "
        "(crates/roost-iced/src/app/interactions.rs:2021-2028) still returns a bare "
        "UiTask with no frozen-frame guard, and no test-mode op exists yet to drive "
        "the paste keybind from this IPC-only harness. C9 lands both; remove this "
        "xfail once it does."
    ),
)
def test_paste_into_a_frozen_host_frame_is_refused(roost: Roost, target):
    """The regression test for plan 039 C9 (§3.11), landing here per the
    plan's own instruction ("whichever lands second wires it").

    C9 has NOT landed as of this commit, in two ways: `paste_into_active`
    still returns a bare `UiTask` rather than `Result<UiTask, String>`
    and never checks `frozen_frame` at all, and — the deeper gap —
    nothing in this IPC-only harness can *drive* the paste keybind
    today. `KeybindAction::Paste` (`app.rs:2894`/`:2964`) is reachable
    only from a real key event; every other otherwise-input-only
    behavior this plan touched got a `ROOST_TEST_MODE=1` test-mode op
    seam (`app.dialog_answer`, `tab.feed_pty_bytes`, …) and this one
    still has none. `app.keybind_dispatch` below is this test's guess at
    what that seam will be named — C9's implementer is expected to wire
    it up (or rename this call to match whatever it actually adds) —
    and the assertion is the pinned observable from §3.11 itself: the
    host-client lane drives the paste keybind against a frozen frame and
    reads the refusal line off `UiLog`. Today `app.keybind_dispatch`
    does not exist, so the very first call below raises `unknown-op` and
    the test fails outright — which is what `xfail(strict=True)` is for.
    """
    session_env = sessionlib.make_env()
    try:
        start_session(session_env)
        with saved_host(roost, session_env) as host:
            host.connect_and_wait()
            with host.client() as session:
                tab = quiet_tab(session, first_project(session), host.env.launch_cwd)
                host_key(roost, tab)

            with host.client() as interloper:
                lease = HostUnderTest.lease(interloper, takeover=True)
                with EventStream(host.env.socket, lease=lease):
                    host.wait_connect_subtitle(SUBTITLE_TAKEN_OVER)

                    log = UiLog(target, "reconnect to paste")
                    roost.call("app.keybind_dispatch", {"action": "paste"})
                    line = log.wait_next(timeout=10.0)
                    assert "reconnect to paste" in line, line
    finally:
        session_env.teardown()
