"""The whole SSH transport, through the real UI (host-sessions HS-3).

`test_host_client.py` connects the UI to a session over a Unix socket.
This module connects it the way a remote host is really reached — the
app's own `SshTunnel`, a real `ssh` invocation per connection, and
`roost-session client-bridge` on the far side — with exactly one thing
faked: the `ssh` binary itself.

# The chain, and where the fake sits

    the UI  →  SshTunnel (real, in the UI process)
            →  $ROOST_SSH_BIN  →  fixtures/fake-ssh.sh   ← the only fake
            →  roost-session client-bridge (real)
            →  a real roost-session daemon on a throwaway profile

`fake-ssh.sh` (its header is the contract) is a *faithful* stand-in
rather than a stub: it honors `-O exit` as a recorded no-op, treats the
mux warm-up's literal `true` as `true`, records every invocation's argv,
and — in the failure modes — writes the real thing's stderr and exit
codes, which is what the client classifies on. What it does not do is
cross a network, which is the one part of the chain a test cannot own.

# The two moving pieces of the harness

* **`$ROOST_SSH_BIN`** is set at *import* time, because it has to be in
  the environment of the UI the session-scoped fixture launches — the
  same reason `test_sparkle.py` binds its loopback port at import. It
  points at a per-run wrapper script that sources
  [`FAKE_SSH_CONFIG`] and execs the fixture. The config file is
  rewritten between phases, and the UI execs `ssh` afresh per
  connection, so a test can change what the far side does mid-run
  without relaunching anything.
* **`FAKE_SSH_SESSION_ENV`** is how the far side finds *this test's*
  session. The bridge child inherits the UI's environment, whose `HOME`
  (and `XDG_RUNTIME_DIR`) belong to the developer — so without an
  override every bridge would dial the developer's own session socket.
  The file this module writes reproduces `session.make_env`'s
  derivations, and unsets the profile selectors the UI carries, so
  `BundleProfile::session()` on the far side resolves the throwaway
  profile the fixture started.

# What can be asserted about a failure, and what cannot

A classified failure's copy reaches the *sidebar band*
(`disconnected — <reason>`) and the UI's log. Neither `host.list` (a
registry row: id, label, target) nor `app.sidebar_dump` (agent rows)
carries it, so there is no op that returns the reason. The failure cases
below therefore assert the strongest pair that is actually available:
the connection state, read off the palette's host verbs the way
`test_host_client.py` reads it, and the classified copy itself, read
from the UI's own log — which also serves as the settle signal, because
an ssh host with no connection yet reads as `disconnected` from the very
start of an attempt (`refresh_host_views`), so the palette alone cannot
say when the attempt finished.

Condition waits only.
"""

from __future__ import annotations

import atexit
import contextlib
import os
import shutil
import stat
import tempfile
import uuid
from pathlib import Path

import pytest
import session as sessionlib
import ui
from client import Roost

# The host lane's vocabulary, unchanged: same fixtures' shape, same
# palette reads, same incarnation probe. Only the transport differs.
from client import RoostError
from test_host_client import (
    HostUnderTest,
    first_project,
    host_row_ids,
    marker,
    quiet_tab,
    start_session,
    wait_dump_contains,
    wait_until,
)

pytestmark = pytest.mark.host_client


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "fake-ssh.sh"

# One directory per pytest process, not per test: the wrapper's path has
# to be stable for the whole run, because it is baked into the launched
# UI's environment and the UI outlives every test here.
_RUN_ROOT = Path(tempfile.mkdtemp(prefix="roost-ssh-e2e-", dir="/tmp"))
SSH_WRAPPER = _RUN_ROOT / "ssh"
#: Sourced by the wrapper on every invocation, so rewriting it between
#: phases changes what the *next* `ssh` does with nothing to relaunch.
FAKE_SSH_CONFIG = _RUN_ROOT / "fake-ssh.env"
#: Where the far side is told to find this test's session.
FAKE_SSH_SESSION_ENV = _RUN_ROOT / "session.env"
#: One tab-separated line per invocation; see the fixture's header.
FAKE_SSH_LOG = _RUN_ROOT / "invocations.log"

# The env keys `session.make_env` sets, per platform. Exported to the far
# side when the fixture set them and unset when it did not, so the bridge
# resolves the same profile the daemon bound.
_SESSION_ENV_KEYS = (
    "HOME",
    "SHELL",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "ROOST_SHELL_FEATURES",
    "ROOST_SESSION_BIN",
    "RUST_LOG",
)

# Carried by the UI, meaningless (or actively misleading) on the far
# side: the bridge must resolve the *session* profile from the
# fixture's paths alone.
_SESSION_ENV_UNSET = (
    "ROOST_BUNDLE_PROFILE",
    "ROOST_STATE_DIR",
    "ROOST_CONFIG",
    "ROOST_SOCKET",
    "ROOST_TAB_ID",
    "XDG_CONFIG_HOME",
)

# The classified copy each failure mode must produce
# (`roost_ipc::ssh::SshFailure::message`). Restated rather than imported
# — this is user-facing wording, and a change to it should have to be
# made twice, on purpose.
AUTH_COPY = "refused authentication"
CHANGED_KEY_COPY = "has CHANGED since it was last seen"
CHANGED_KEY_WARNING = "Do not accept the new key"
# The *unknown*-key remedy. It must never appear for a changed key: that
# is the one case where accepting is exactly the wrong advice.
UNKNOWN_KEY_REMEDY = "review and accept it"
NOT_FOUND_COPY = "roost-session isn't installed on"

# What the UI logs when an establish fails (`host_conn.rs`). Also the
# settle signal — see the module docstring.
TUNNEL_FAILED = "ssh tunnel could not be established"


def sh_quote(raw: str) -> str:
    return "'" + raw.replace("'", "'\\''") + "'"


def configure_fake_ssh(mode: str = "ok", *, remote: str | None = None) -> None:
    """Point the next `ssh` invocation at `mode`.

    Rewritten rather than re-exported: the UI holds the wrapper's path,
    not its contents, and execs it afresh per connection.
    """
    if remote is None:
        remote = f"{sh_quote(str(sessionlib.session_binary()))} client-bridge"
    FAKE_SSH_CONFIG.write_text(
        "\n".join(
            [
                f"FAKE_SSH_LOG={sh_quote(str(FAKE_SSH_LOG))}",
                f"FAKE_SSH_MODE={sh_quote(mode)}",
                f"FAKE_SSH_EXEC={sh_quote(remote)}",
                f"FAKE_SSH_SESSION_ENV={sh_quote(str(FAKE_SSH_SESSION_ENV))}",
                "",
            ]
        )
    )


def write_session_env(env: sessionlib.SessionEnv) -> None:
    """Reproduce `session.make_env`'s derivations for the far side."""
    lines = [f"unset {' '.join(_SESSION_ENV_UNSET)}"]
    for key in _SESSION_ENV_KEYS:
        value = env.env.get(key)
        if value is None:
            lines.append(f"unset {key}")
        else:
            lines.append(f"{key}={sh_quote(value)}")
            lines.append(f"export {key}")
    FAKE_SSH_SESSION_ENV.write_text("\n".join(lines) + "\n")


def _install_wrapper() -> None:
    """Write the `ssh` the UI will exec, and point the UI at it.

    At import time, before the session-scoped UI fixture launches
    anything: `ROOST_SSH_BIN` is read once per tunnel, out of the UI
    process's own environment.
    """
    FAKE_SSH_LOG.write_text("")
    FAKE_SSH_SESSION_ENV.write_text("")
    # An inert placeholder: the fixture writes the real one per test.
    # `true` rather than the bridge command so importing this module can
    # never trigger a `cargo build` at collection time.
    configure_fake_ssh("ok", remote="true")
    SSH_WRAPPER.write_text(
        "#!/bin/sh\n"
        "# Written by tools/roosttest/test_host_ssh.py.\n"
        f". {sh_quote(str(FAKE_SSH_CONFIG))}\n"
        "export FAKE_SSH_LOG FAKE_SSH_MODE FAKE_SSH_EXEC FAKE_SSH_SESSION_ENV\n"
        f'exec {sh_quote(str(FIXTURE))} "$@"\n'
    )
    SSH_WRAPPER.chmod(SSH_WRAPPER.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    os.environ["ROOST_SSH_BIN"] = str(SSH_WRAPPER)


_install_wrapper()
# At interpreter exit rather than in a fixture: the wrapper has to outlive
# every test *and* the UI that holds its path, which the session fixture
# quits during its own teardown.
atexit.register(shutil.rmtree, _RUN_ROOT, True)


# ---------------------------------------------------------------------------
# The invocation log
# ---------------------------------------------------------------------------


def invocations() -> list[list[str]]:
    """Every `ssh` invocation so far as `[pid, *argv]`."""
    try:
        raw = FAKE_SSH_LOG.read_text(errors="replace")
    except OSError:
        return []
    return [line.split("\t") for line in raw.splitlines() if line]


def is_establish(argv: list[str]) -> bool:
    """The mux warm-up: its remote command is the literal `true`."""
    return len(argv) > 1 and argv[-1] == "true"


def is_exec(argv: list[str]) -> bool:
    """A per-connection exec: its remote command runs the bridge."""
    return len(argv) > 1 and "client-bridge" in argv[-1]


def count(predicate) -> int:
    return sum(1 for fields in invocations() if predicate(fields[1:]))


def kill_bridge_connections() -> list[int]:
    """SIGKILL the live far side of every open connection.

    Only *exec* invocations are candidates. The wrapper and the fixture
    both `exec`, so a logged pid IS the `roost-session client-bridge`
    process — and an exec's pid lives exactly as long as its connection,
    which is what makes it safe to signal. A warm-up's pid has long since
    exited and could have been recycled onto anything, this test's own
    session daemon included, so those are never touched.
    """
    killed = []
    for fields in invocations():
        if not is_exec(fields[1:]) or not fields[0].startswith("pid="):
            continue
        pid = int(fields[0].removeprefix("pid="))
        if not sessionlib.pid_alive(pid):
            continue
        with contextlib.suppress(OSError):
            os.kill(pid, 9)
            killed.append(pid)
    return killed


# ---------------------------------------------------------------------------
# The UI's log — the only surface carrying a classified reason
# ---------------------------------------------------------------------------


class UiLog:
    """A cursor over the launched UI's captured output.

    Counting matches rather than searching the whole file is what makes
    "this attempt failed" distinguishable from "an earlier attempt in
    this module did".
    """

    def __init__(self, target: str, needle: str):
        self.target = target
        self.needle = needle
        self.seen = len(self._matches())

    def _matches(self) -> list[str]:
        output = ui._launch_output(self.target)
        return [line for line in output.splitlines() if self.needle in line]

    def wait_next(self, timeout: float = 90.0) -> str:
        """Block until one more matching line appears; return it."""

        def fresh() -> str | None:
            lines = self._matches()
            return lines[self.seen] if len(lines) > self.seen else None

        line = wait_until(fresh, timeout, f"the UI to log {self.needle!r}")
        self.seen += 1
        return line


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _harness_owned_ui(target):
    """This lane can only drive a UI it launched itself.

    `ROOST_SSH_BIN` reaches the UI through its launch environment, so a
    developer's already-running instance would exec the *real* `ssh` at a
    host that does not exist — and settle disconnected, which is what
    three of these cases assert. Skipping is the only honest answer.
    """
    if ui.session_state_dir() is None:
        pytest.skip(
            "this lane needs a harness-launched UI (ROOST_SSH_BIN is injected at "
            "launch); run it via `make e2e-host-ssh` or with --roost-fresh"
        )


@pytest.fixture
def session_env():
    made = sessionlib.make_env()
    try:
        yield made
    finally:
        made.teardown()


@pytest.fixture
def ssh_host(roost: Roost, session_env):
    """A running session, reachable only over the fake `ssh`, saved as a
    host, not yet connected."""
    start_session(session_env)
    write_session_env(session_env)
    configure_fake_ssh("ok")

    # The destination is decorative — the fixture ignores it and the
    # session env decides what the far side dials — but it must classify
    # as ssh, which is what puts the tunnel in the path at all.
    label = f"ssh-{uuid.uuid4().hex[:8]}"
    added = roost.call("host.add", {"label": label, "target": f"ssh://{label}.invalid"})["host"]
    under_test = HostUnderTest(
        roost=roost, env=session_env, saved_id=added["id"], label=label
    )
    # Same guard `test_host_client.saved_host` carries: the incarnation
    # probe finds a tab by the number it has, and two connected hosts can
    # both have a tab `4`.
    others = {
        row.removeprefix("host:disconnect:")
        for row in host_row_ids(roost)
        if row.startswith("host:disconnect:")
    } - {under_test.saved_id}
    if others:
        pytest.skip(
            f"another host is already connected ({sorted(others)}); the incarnation "
            "probe cannot tell two connected hosts' tabs apart"
        )
    try:
        yield under_test
    finally:
        roost.palette_dismiss()
        # Removal disconnects, which is what tears the tunnel (and its
        # `ssh` master) down — before the session it reaches goes away.
        with contextlib.suppress(Exception):
            under_test.remove()


# ---------------------------------------------------------------------------
# Reading a host tab, and starting an attempt
# ---------------------------------------------------------------------------


# How many incarnations [`host_key`] scans per pass. Small on purpose,
# and safe here for a reason `test_host_client` cannot rely on: this lane
# only ever drives a UI it launched itself (see `_harness_owned_ui`), so
# the app's minter is a handful of connects in, not hundreds.
#
# Deliberately NOT that module's 4096-wide single-pass scan. A miss there
# costs 4096 IPC round trips — tens of seconds, i.e. the whole wait
# budget — so a tab that reaches the client's mirror a moment *after* it
# is created never gets a second pass. Over ssh that moment is real: the
# `tab.opened` event crosses two extra pumps and a remote process before
# the mirror has it.
HOST_INCARNATION_WINDOW = 64


def host_key(roost: Roost, tab_id: int, timeout: float = 60.0) -> str:
    """Focus a host tab and return the `h<host>.<id>` spelling that
    reached it.

    The incarnation is minted per connect attempt and no op reports it,
    so it is discovered by probing — and focusing is the discovery
    because focusing is also the attach. A stale incarnation names
    nothing, so a wrong guess is a clean `not-found` rather than a wrong
    tab.
    """

    def probe() -> str | None:
        for incarnation in range(1, HOST_INCARNATION_WINDOW + 1):
            try:
                roost.call("tab.focus", {"tab_id": f"h{incarnation}.{tab_id}"})
            except RoostError:
                continue
            return f"h{incarnation}.{tab_id}"
        return None

    return wait_until(probe, timeout, f"the client to list host tab {tab_id}")


#: What `host.connect` may answer for an **ssh** host. The establish is
#: in flight when the op replies, which `host_connection_result` reads as
#: `connecting` (the `HostConn` itself is created an engine-feed hop
#: later). The settled verdict is watched on the palette either way,
#: which is why this stays a tolerance rather than an assertion about
#: which spelling the race lands on.
CONNECT_STARTED = ("disconnected", "connecting", "connected")


def connect_and_wait(host: HostUnderTest, timeout: float = 60.0) -> None:
    """Ask to connect, then wait for the palette to say it happened."""
    result = host.connect()
    assert result["state"] in CONNECT_STARTED, result
    host.wait_connected(timeout)


def connect_expecting_failure(host: HostUnderTest, target: str, mode: str) -> str:
    """Reconfigure the fake, ask to connect, and return the classified
    copy the UI logged.

    The log line is the settle signal as well as the assertion: a host
    whose tunnel never came up has no connection object at all, and
    `refresh_host_views` reads that as `disconnected` from the moment the
    attempt starts — so the palette cannot say when the attempt ended.
    """
    configure_fake_ssh(mode)
    log = UiLog(target, TUNNEL_FAILED)
    result = host.connect()
    assert result["state"] in CONNECT_STARTED, result
    line = log.wait_next()
    # Settled, and settled the right way: still offering Connect, never
    # Disconnect.
    rows = host_row_ids(host.roost)
    assert f"host:connect:{host.saved_id}" in rows, rows
    assert f"host:disconnect:{host.saved_id}" not in rows, rows
    return line


# ---------------------------------------------------------------------------
# 1. The happy path: a host reached over ssh is a host
# ---------------------------------------------------------------------------


def test_an_ssh_host_connects_and_renders_its_session(ssh_host, roost):
    """AC: an ssh target connects, hydrates, and shows a terminal.

    The bytes are the assertion. A marker fed into the session's own
    terminal has to cross the bridge, the `ssh` stdio pipe and the
    tunnel's socket before the client can dump it, so a chain that was
    connected but not *pumping* would fail here rather than pass quietly.

    The invocation log is the second half: it proves the UI really went
    through `$ROOST_SSH_BIN` — a warm-up (`true`) to open the mux, then
    one exec per connection — rather than reaching the session some other
    way.
    """
    connect_and_wait(ssh_host)
    assert count(is_establish) >= 1, invocations()
    assert count(is_exec) >= 1, invocations()

    with ssh_host.client() as session:
        tab = quiet_tab(session, first_project(session), ssh_host.env.launch_cwd)
        key = host_key(roost, tab)
        line = marker("OVER-SSH")
        session.tab_feed_pty_bytes(tab, f"{line}\r\n".encode())
        wait_dump_contains(roost, key, line)


# ---------------------------------------------------------------------------
# 2. The transport dies under the client
# ---------------------------------------------------------------------------


def test_killing_the_bridge_processes_disconnects_and_a_reconnect_restores_the_tab(
    ssh_host, roost
):
    """SIGKILL the far side, then connect again over a fresh tunnel.

    This is the failure a remote transport actually has: no protocol
    error, no disconnect op, just a pipe that ends. The client has to
    land in `disconnected` (the palette offers Connect again), the
    registry has to keep the host, and the reconnect has to find the same
    terminal — because the session never went anywhere.
    """
    connect_and_wait(ssh_host)
    with ssh_host.client() as session:
        tab = quiet_tab(session, first_project(session), ssh_host.env.launch_cwd)
        key = host_key(roost, tab)
        line = marker("SSH-SURVIVES")
        session.tab_feed_pty_bytes(tab, f"{line}\r\n".encode())
        wait_dump_contains(roost, key, line)

        assert kill_bridge_connections(), "no live bridge connection to kill"
        ssh_host.wait_not_connected()
        assert ssh_host.saved_id in {
            row["id"] for row in roost.call("host.list", {})["hosts"]
        }, "a dropped transport must not unsave the host"

        execs_before = count(is_exec)
        connect_and_wait(ssh_host)
        assert count(is_exec) > execs_before, "the reconnect reused a dead connection"
        again = host_key(roost, tab)
        assert again != key, "a reconnect must mint a fresh incarnation"
        wait_dump_contains(roost, again, line)


# ---------------------------------------------------------------------------
# 3-5. The classified failures
# ---------------------------------------------------------------------------


def test_an_auth_failure_settles_disconnected_with_the_auth_copy(ssh_host, target):
    """`Permission denied (publickey).` — the most common real failure.

    What matters is that it is reported as itself: an auth problem is the
    user's to fix, and the copy names the remedy (`ssh <target>` in a
    terminal), so a classifier that fell through to the generic transport
    message would strand them.
    """
    reason = connect_expecting_failure(ssh_host, target, "auth-fail")
    assert AUTH_COPY in reason, reason


def test_a_changed_host_key_settles_disconnected_and_never_offers_to_accept(
    ssh_host, target
):
    """The wary case. A changed host key is what a machine-in-the-middle
    looks like from here, so the copy must warn rather than offer a
    remedy — and must never carry the *unknown*-key remedy, which is to
    review and accept the key.

    ssh's changed-key blob contains the unknown-key blob's own sentence
    (`Host key verification failed.`), so this case is also the fence on
    classification order.
    """
    reason = connect_expecting_failure(ssh_host, target, "hostkey-changed")
    assert CHANGED_KEY_COPY in reason, reason
    assert CHANGED_KEY_WARNING in reason, reason
    assert UNKNOWN_KEY_REMEDY not in reason, reason


def test_a_missing_remote_binary_settles_disconnected_and_names_roost_session(
    ssh_host, target
):
    """Exit 127: the host is reachable, the binary is not there.

    The remedy is about `roost-session`, not about ssh, so the copy has
    to say so — this is the one failure where the user's next step is an
    install on the far machine.
    """
    reason = connect_expecting_failure(ssh_host, target, "exit-127")
    assert NOT_FOUND_COPY in reason, reason
    assert "roost-session" in reason, reason
