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

# How a failure is asserted: `host.status`, never the log

Every claim this module makes about connection state is read off the
`host.status` op (plan 042 W1) — the classified reason, the band's own
rollup, and the ssh ladder's armed rung with its `attempt`/`budget`.
Until that op existed the only surface carrying a reason was the UI's
`tracing` output, which this module scraped; a log line is an operator
convenience a refactor is free to reword, so scraping it passed just as
happily against a feature that had stopped working.

Two properties of the op are what make the waits here honest, and both
are worth knowing before reading a case:

* **`generation` is the edge.** It counts attempts *started*, bumping
  when an ssh establish begins rather than when it succeeds, and it
  survives a disconnect. Two consecutive attempts can fail with
  byte-identical reasons, so "disconnected with a reason" cannot tell
  attempt N from N−1 — reading `generation` first and waiting for it to
  advance can. It is what the old log cursor's "one more line" was.
* **`retry` is present only while a rung is armed**, and disappears the
  moment the timer fires. So `retry.attempt` climbs in visible steps but
  a poll can miss one when the ladder's base is short: the waits below
  assert `attempt >= n` and monotonic non-decrease, never equality with
  a rung.

What is still not an op, and stays an OS fact, is the fake `ssh`
invocation log: how many establishes ran, and which `ssh` processes are
still alive.

Condition waits only.
"""

from __future__ import annotations

import atexit
import contextlib
import os
import shutil
import signal
import stat
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

import pytest
import session as sessionlib
import ui
from client import Roost, scaled_timeout
from util import runs_alone

# The host lane's vocabulary, unchanged: same fixtures' shape, same
# palette reads, same incarnation probe. Only the transport differs.
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
from test_host_client import host_key as _host_key

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

# The band's own two ladder lines (`host_conn.rs`'s `retry_line` and
# `gave_up_copy`, plan 040 §3.8), as `host.status` reports them in
# `rollup` — the sidebar reducer's output, `"{word} — {reason}"`.
# Restated here for the same reason the failure copy above is: this is
# what a user reads.
DISCONNECTED_BAND = "disconnected — "
RETRY_REASON = "reconnecting in "
GAVE_UP_BAND = DISCONNECTED_BAND + "reconnect gave up after "


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
    # Beside it, and for the same reason: the reconnect ladder reads
    # these once, out of the UI process's own environment, and only under
    # `ROOST_TEST_MODE=1` (`host_conn/reconnect.rs`) — which
    # `make e2e-host-ssh` sets.
    #
    # Shipped, the ladder is ten attempts on a 1s base, and a give-up
    # against a black-holed route costs six to eight minutes (plan 040
    # §3.5): every attempt pays an establish, a teardown and a lease
    # probe on top of its sleep. A lane whose whole subject is *settling*
    # cannot spend that, so it shortens the ladder rather than the
    # assertions. 400ms keeps the rungs long enough that a case which has
    # to get an op onto the wire inside an armed window (the disconnect,
    # removal and quit cases below) is racing a several-hundred-millisecond
    # timer rather than a round trip.
    os.environ["ROOST_SSH_RECONNECT_ATTEMPTS"] = "4"
    os.environ["ROOST_SSH_RECONNECT_BASE_MS"] = "400"


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


def is_teardown(argv: list[str]) -> bool:
    """The mux teardown: `-O exit` against the tunnel's own control
    socket (`teardown_argv`, `ssh.rs`) — no remote command at all."""
    return "-O" in argv and "exit" in argv


def count(predicate) -> int:
    return sum(1 for fields in invocations() if predicate(fields[1:]))


def _descendants(pid: int) -> list[int]:
    """The pid's live descendants, deepest last, via `pgrep -P`."""
    out = subprocess.run(
        ["pgrep", "-P", str(pid)], capture_output=True, text=True, check=False
    ).stdout
    children = [int(line) for line in out.split() if line.strip()]
    tree = []
    for child in children:
        tree.append(child)
        tree.extend(_descendants(child))
    return tree


def kill_bridge_connections() -> list[int]:
    """SIGKILL the live far side of every open connection — the logged
    pid AND its descendants.

    Only *exec* invocations are candidates; a warm-up's pid has long
    since exited and could have been recycled onto anything, this test's
    own session daemon included, so those are never touched. The tree
    matters: on shells that exec-optimize `sh -c` (macOS bash) the
    logged pid IS `roost-session client-bridge`, but Ubuntu's dash forks
    instead, leaving the bridge as a *child* of the logged `sh` — and
    killing only the parent orphans a bridge that keeps the pipes open,
    so the UI never sees the connection die (the exact CI hang this
    guard exists for). Killing the whole tree models the thing being
    simulated either way: every process of the connection dies.
    """
    killed = []
    for fields in invocations():
        if not is_exec(fields[1:]) or not fields[0].startswith("pid="):
            continue
        pid = int(fields[0].removeprefix("pid="))
        if not sessionlib.pid_alive(pid):
            continue
        for target in [pid, *_descendants(pid)]:
            with contextlib.suppress(OSError):
                os.kill(target, 9)
        killed.append(pid)
    return killed


def live_ssh_invocations(under: int | None = None) -> list[int]:
    """Every logged fake-`ssh` invocation still running.

    `under` scopes the answer to one process's descendants, which is what
    closes the one false positive a pid list has: a pid the log named
    long ago can have been recycled onto something else. When the UI is
    still alive that is the read to take — every `ssh` it runs is its own
    child. After it has exited there is nothing left to scope to, so the
    bare liveness read is what there is, and the cases that use it run
    seconds after the pids they care about were logged.
    """
    logged = []
    for fields in invocations():
        if not fields[0].startswith("pid="):
            continue
        pid = int(fields[0].removeprefix("pid="))
        if sessionlib.pid_alive(pid):
            logged.append(pid)
    if under is None:
        return logged
    tree = set(_descendants(under))
    return [pid for pid in logged if pid in tree]


def hold(check, seconds: float = 2.0) -> None:
    """Hold an assertion for a window rather than reading it once.

    "Nothing further happens" has no edge to wait for, so a single read
    the instant a ladder settles proves only "nothing further *yet*" — a
    regression that armed one more timer would fire after it and still
    pass. Same shape as `test_host_bootstrap.py`'s `assert_no_dialog_for`,
    for the same reason.
    """
    deadline = time.monotonic() + scaled_timeout(seconds)
    while True:
        check()
        if time.monotonic() >= deadline:
            return
        time.sleep(0.1)


def dialog_dump(roost: Roost) -> dict:
    return roost.call("app.dialog_dump", {})


# ---------------------------------------------------------------------------
# `host.status` — the surface carrying connection state
# ---------------------------------------------------------------------------


def status(host: HostUnderTest) -> dict:
    """This host's one `host.status` row."""
    return host.roost.host_status(host.saved_id)["hosts"][0]


def saved_ids(roost: Roost) -> set[str]:
    """Every id `host.status`'s all-hosts form answers for.

    The all-hosts form rather than a narrowed one because the question
    it answers here is *absence*: a removed host must be gone from the
    list, and `--id` on a host the registry has forgotten is a
    `not-found` error, which is a weaker claim than the row simply not
    being there.
    """
    return {row["id"] for row in roost.host_status()["hosts"]}


def assert_ladder_quiet(
    host: HostUnderTest, what: str, *, establishes: int, generation: int
) -> dict:
    """No establish ran, no rung is armed, no attempt was started.

    The three reads a "nothing further happened" window is held on
    (`hold`), in one place because four cases make exactly this claim
    and differ only in what `what` names as the thing that must have
    stopped.

    `generation` is the strongest of the three: a timer that fired and
    re-armed between two polls could show no `retry` at either, but
    cannot start an attempt without moving it. The establish count is
    the same claim as an OS fact.

    Returns the row it read, so a caller with a further claim to make
    about the same poll spends one `host.status` call rather than two.
    """
    assert count(is_establish) == establishes, invocations()
    row = status(host)
    assert row.get("retry") is None, f"{what} armed another rung: {row}"
    assert row["generation"] == generation, f"{what} was retried anyway: {row}"
    return row


def assert_band_matches_retry(row: dict) -> None:
    """The band and the schedule in one response say the same thing.

    Plan 042 AC1. `rollup` is the sidebar reducer's own output and
    `retry` is the timer the ladder armed; nothing but this rebuilds one
    from the other, so a formatter change on either side that stopped
    them agreeing would otherwise ship. Every number comes from the
    *same* response — recomputing from a second read would be racing the
    ladder rather than checking a format.

    The seconds round up and floor at one, mirroring
    `retry_line`'s `delay.as_millis().div_ceil(1_000).max(1)` exactly:
    integer arithmetic, because a float `ceil` of 400ms and of 400.0ms
    are not the same expression.
    """
    retry = row["retry"]
    seconds = max(1, -(-retry["delay_ms"] // 1000))
    want = (
        f"{DISCONNECTED_BAND}{RETRY_REASON}{seconds}s "
        f"({retry['attempt']}/{retry['budget']})"
    )
    assert row["rollup"] == want, (row["rollup"], want, retry)


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
        # Both suppressed: the C7 signal-teardown test ends the UI itself
        # mid-test, so a socket write here is a `BrokenPipeError`, not a
        # bug — that test's own assertions already covered what it did.
        with contextlib.suppress(Exception):
            roost.palette_dismiss()
        # Removal disconnects, which is what tears the tunnel (and its
        # `ssh` master) down — before the session it reaches goes away.
        with contextlib.suppress(Exception):
            under_test.remove()


# ---------------------------------------------------------------------------
# Reading a host tab, and starting an attempt
# ---------------------------------------------------------------------------


# `host_probe.host_key` (a 64-wide first pass, each retry inside
# `wait_until` re-scanning from the floor with double the span, floor
# advancing on every hit) is exactly this lane's shape too — same
# `tab.focus` probe, same discovery-is-attach story, same reason a
# narrow first pass beats a wide single-pass scan: over ssh a
# tab reaching the client's mirror a moment *after* it is created is the
# common case, not the exception (the `tab.opened` event crosses two
# extra pumps and a remote process before the mirror has it), so a miss
# has to be cheap enough for `wait_until` to genuinely retry. Only this
# lane's longer default budget is worth keeping local: ssh adds a tunnel
# dial and a remote exec before the incarnation this call is watching
# for even exists.
def host_key(roost: Roost, tab_id: int, timeout: float = 60.0) -> str:
    return _host_key(roost, tab_id, timeout=timeout)


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


def wait_for_a_settled_reason(
    host: HostUnderTest, after: int, timeout: float = 90.0
) -> dict:
    """Wait for an attempt started after generation `after` to settle
    with a verdict of its own, and return its `host.status` row.

    Four conditions, and each closes a window the others leave open. The
    generation must have advanced, or the reason belongs to the previous
    attempt. The state must be `disconnected`: an ssh host with no
    connection yet reads that way from the moment an attempt starts, so
    it is a necessary condition rather than the settle signal. No retry
    may be armed, and the reason must not still be the ladder's own
    `reconnecting in …` line — those two are the settle signal, because
    between a rung firing and its establish failing the band still
    carries the armed rung's text with a fresher generation beside it.
    """

    def settled() -> dict | None:
        row = status(host)
        if row["generation"] <= after:
            return None
        if row["state"] != "disconnected" or row.get("retry") is not None:
            return None
        reason = row.get("reason")
        if not reason or reason.startswith(RETRY_REASON):
            return None
        return row

    return wait_until(settled, timeout, f"an attempt past generation {after} to settle")


def connect_expecting_failure(host: HostUnderTest, mode: str) -> str:
    """Reconfigure the fake, ask to connect, and return the classified
    reason the attempt settled on.

    `generation` read before the op is what makes this an assertion
    about *this* attempt: it counts attempts started, so a reason read
    without it could be the previous one's, still sitting in the band.
    """
    configure_fake_ssh(mode)
    before = status(host)["generation"]
    result = host.connect()
    assert result["state"] in CONNECT_STARTED, result
    row = wait_for_a_settled_reason(host, before)
    # Settled, and settled the right way: still offering Connect, never
    # Disconnect.
    rows = host_row_ids(host.roost)
    assert f"host:connect:{host.saved_id}" in rows, rows
    assert f"host:disconnect:{host.saved_id}" not in rows, rows
    return row["reason"]


# ---------------------------------------------------------------------------
# The auto-reconnect ladder (plan 040)
# ---------------------------------------------------------------------------


def drop_the_link(host: HostUnderTest, mode: str) -> None:
    """Put `mode` in front of the *next* `ssh`, then kill the far side.

    Order matters and is the whole trick this lane turns: the fake is
    read per invocation, so the connection that is already up dies of a
    SIGKILL (an unclassified bare EOF — the ordinary dropped link, and
    the one drop shape the ladder retries), while every attempt the
    ladder makes afterwards runs the mode under test.
    """
    configure_fake_ssh(mode)
    assert kill_bridge_connections(), "no live bridge connection to kill"


def wait_for_a_live_retry(host: HostUnderTest, through: int = 3) -> dict:
    """Wait until the ladder has armed attempt `through`'s timer, and
    return the `host.status` row that says so.

    Not attempt 1: its rung is the shortest there is (half to all of
    `ROOST_SSH_RECONNECT_BASE_MS`), and the cases that follow have to get
    an op onto the wire *inside* an armed window. Waiting a couple of
    rungs in spends a second of test time to buy a window several times
    longer than the round trip.

    `>=` rather than equality, with monotonic non-decrease asserted on
    the way: a 400ms base against a 50ms poll means a rung can come and
    go between two reads, and a test that demanded to *see* attempt 2
    would fail on a ladder that was climbing correctly. What a skipped
    rung cannot do is make the number go backwards, which is the
    property that distinguishes climbing from re-arming in place.
    """
    seen = 0

    def climbed() -> dict | None:
        nonlocal seen
        row = status(host)
        retry = row.get("retry")
        if retry is None:
            return None
        attempt = retry["attempt"]
        assert attempt >= seen, f"the ladder went back from {seen} to {attempt}"
        seen = attempt
        return row if attempt >= through else None

    return wait_until(climbed, 60.0, f"the ladder to arm attempt {through}")


def wait_for_the_give_up(host: HostUnderTest, timeout: float = 60.0) -> dict:
    """Wait for the ladder to spend its budget and stop.

    Both halves matter and neither implies the other: `retry is None`
    says no timer is armed, which is also true in the middle of an
    attempt, and the band's give-up copy is what says the ladder is
    *finished* rather than between rungs.
    """

    def gave_up() -> dict | None:
        row = status(host)
        if row.get("retry") is not None:
            return None
        rollup = row.get("rollup") or ""
        return row if rollup.startswith(GAVE_UP_BAND) else None

    return wait_until(gave_up, timeout, "the ladder to give up")


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


def test_killing_the_bridge_processes_auto_reconnects_and_restores_the_tab(
    ssh_host, roost
):
    """SIGKILL the far side, and touch nothing: the host comes back.

    This is the failure a remote transport actually has: no protocol
    error, no disconnect op, just a pipe that ends. Until plan 040 the
    recovery was a person clicking ↻; now a connection that reached
    `Connected` climbs its own ladder, so the assertion is the absence of
    a `connect()` call. Nothing here asks the UI to reconnect.

    Recast rather than added beside its predecessor, which asked for the
    reconnect by hand: with a sub-second first rung that manual call now
    races the automatic one, and the `wait_not_connected` poll in front
    of it can miss a disconnected window that closes before it looks
    (plan 040 §2.9). Two reconnects for one drop is not a test, it is a
    flake — so this is the same scenario with the manual half deleted.

    The registry has to keep the host, the tunnel has to be rebuilt
    rather than reused (`SshTunnel` has no `reestablish`; `apply_state`
    tears it down on every `Disconnected`), the incarnation has to be
    fresh, and the terminal has to still hold what was written before the
    drop — because the session never went anywhere.
    """
    connect_and_wait(ssh_host)
    with ssh_host.client() as session:
        tab = quiet_tab(session, first_project(session), ssh_host.env.launch_cwd)
        key = host_key(roost, tab)
        line = marker("SSH-SURVIVES")
        session.tab_feed_pty_bytes(tab, f"{line}\r\n".encode())
        wait_dump_contains(roost, key, line)

        establishes_before = count(is_establish)
        execs_before = count(is_exec)
        assert kill_bridge_connections(), "no live bridge connection to kill"

        # The ladder's own work, watched on the invocation log rather than
        # on the palette: an ssh host with no connection reads as
        # `disconnected` from the start of any attempt, so the palette
        # cannot say whether the recovery below was this drop's or a
        # window that never opened.
        wait_until(
            lambda: count(is_establish) > establishes_before,
            60.0,
            "the ladder to rebuild the tunnel",
        )
        ssh_host.wait_connected(60.0)
        assert count(is_exec) > execs_before, "the reconnect reused a dead connection"
        assert ssh_host.saved_id in {
            row["id"] for row in roost.call("host.list", {})["hosts"]
        }, "a dropped transport must not unsave the host"

        again = host_key(roost, tab)
        assert again != key, "a reconnect must mint a fresh incarnation"
        wait_dump_contains(roost, again, line)


def test_a_host_that_stays_down_climbs_the_ladder_and_then_settles(ssh_host, roost):
    """AC: the ladder advances, gives up, and stops — the off-switch.

    A capped ladder is what makes auto-reconnect safe to have at all: the
    thing being designed against is a client that hammers an `sshd` for
    the rest of the afternoon. So this asserts the shape, not just the
    end — attempt 1, then 2, then 3, then 4 (the band's `(3/4)` reads off
    the same numbers), then the give-up naming how many it spent, and
    then *nothing*.

    `unreachable` is the fixture mode this test needed and none of the
    older ones could supply: every other establish-failing mode names a
    family the ladder refuses to spend an attempt on, and `drop-after`
    sits in the fixture's second dispatch block where an establish never
    reaches it. This one exits non-zero with stderr the classifier has no
    rule for, so it lands on `Transport` — retryable, four times over.

    ↻ Reconnect never left the screen, which is what settling is allowed
    to rely on: the palette still offers Connect at the end.

    It is also where plan 042's AC1 lives: an armed rung is the one
    moment `rollup` and `retry` describe the same event, so this is
    where the band's formatter and the schedule the op reports are
    checked to agree.
    """
    connect_and_wait(ssh_host)
    before = status(ssh_host)["generation"]
    drop_the_link(ssh_host, "unreachable")

    armed = wait_for_a_live_retry(ssh_host, through=4)
    assert_band_matches_retry(armed)
    settled = wait_for_the_give_up(ssh_host)
    assert settled["rollup"].startswith(f"{GAVE_UP_BAND}4 tries"), settled
    # Every rung is one attempt started, and `generation` counts exactly
    # those — so "gave up after 4" is checked against four real attempts,
    # not against the number the copy happens to print. Rungs can be
    # missed by a poll; the counter cannot.
    assert settled["generation"] == before + armed["retry"]["budget"], (
        before,
        settled,
    )

    establishes = count(is_establish)
    generation = settled["generation"]

    hold(
        lambda: assert_ladder_quiet(
            ssh_host, "a settled ladder", establishes=establishes, generation=generation
        ),
        3.0,
    )

    rows = host_row_ids(roost)
    assert f"host:connect:{ssh_host.saved_id}" in rows, rows
    assert f"host:disconnect:{ssh_host.saved_id}" not in rows, rows


def test_a_changed_host_key_ends_the_ladder_instead_of_advancing_it(ssh_host, roost):
    """AC: a changed host key is never retried.

    The security case, and the reason §3.3's table is an exhaustive
    `match`: retrying a possible machine-in-the-middle on a loop is a
    misfeature, not a convenience. The drop that starts the outage is an
    ordinary bare EOF, so one attempt is made and is allowed to be — what
    must never happen is a *second*. The copy has to stay the wary one
    too: the changed-key warning, never the unknown-key remedy, which is
    to review and accept the key.

    Deviation from plan 040 C5(c), stated rather than hidden: the plan
    asks for no retry at all. There is exactly one, and it is not
    reachable to remove from here — the changed key is what the
    *retry's* establish discovers, and the drop that armed that retry is
    a SIGKILLed pipe with no stderr to classify. "Never retried" is
    therefore asserted where it lives: the ladder never advances past
    the changed key.

    Exactly one, without having to catch a rung mid-air: `generation`
    read before the drop must advance (a retry ran) and then hold flat
    (no second one did). Waiting to *see* rung 1 armed would be a race —
    a 400ms base against a 50ms poll usually shows it, and "usually" is
    a flake.
    """
    connect_and_wait(ssh_host)
    before = status(ssh_host)["generation"]
    drop_the_link(ssh_host, "hostkey-changed")

    row = wait_for_a_settled_reason(ssh_host, before)
    reason = row["reason"]
    assert CHANGED_KEY_COPY in reason, reason
    assert CHANGED_KEY_WARNING in reason, reason
    assert UNKNOWN_KEY_REMEDY not in reason, reason

    establishes = count(is_establish)
    generation = row["generation"]

    assert generation == before + 1, f"the ladder spent more than one rung: {row}"

    hold(
        lambda: assert_ladder_quiet(
            ssh_host,
            "a changed host key",
            establishes=establishes,
            generation=generation,
        ),
        3.0,
    )

    rows = host_row_ids(roost)
    assert f"host:connect:{ssh_host.saved_id}" in rows, rows


def test_a_missing_remote_binary_mid_ladder_settles_without_raising_a_card(
    ssh_host, roost
):
    """AC: an offer-able family reached by a ladder raises no modal.

    `NotFound` is one of the two families a bootstrap offer *is* keyed on
    (`offer_for`), so this is the case where the consent gate has to earn
    its keep: an attempt nobody asked for must never put a card on the
    screen, however installable the far side looks. The gate is the
    origin — every ladder attempt re-enters as `Ipc` — and the property
    could only be unit-tested one layer down, at `HostConnSet`, because
    the real path runs through `host_tunnel_ready` →
    `maybe_offer_bootstrap` in the app.

    It settles rather than retrying for the same reason the copy names:
    a retry cannot install anything, and auto-install is forbidden.

    One rung runs — the drop that starts the outage is a SIGKILLed pipe
    with nothing to classify, so the family is only discovered by the
    retry's own establish — and `generation` is how that is counted:
    advanced once, then flat. Waiting to see the rung armed instead
    would be racing a 400ms timer with a 50ms poll.
    """
    connect_and_wait(ssh_host)
    before = status(ssh_host)["generation"]
    drop_the_link(ssh_host, "exit-127")

    row = wait_for_a_settled_reason(ssh_host, before)
    reason = row["reason"]
    assert NOT_FOUND_COPY in reason, reason
    assert "roost-session" in reason, reason

    establishes = count(is_establish)
    generation = row["generation"]
    assert generation == before + 1, f"the ladder spent more than one rung: {row}"

    def settled_and_silent() -> None:
        assert_ladder_quiet(
            ssh_host,
            "an offer-able family",
            establishes=establishes,
            generation=generation,
        )
        dialog = dialog_dump(roost)
        assert dialog.get("dialog") is None, dialog

    hold(settled_and_silent, 3.0)


def test_an_explicit_disconnect_during_a_scheduled_retry_stays_disconnected(
    ssh_host, roost
):
    """AC: asking to disconnect ends the ladder.

    Being reconnected eight seconds after asking to disconnect is the one
    outcome nobody wants, and it is the outcome a timer nobody cancelled
    produces. The armed rung read just before the op is what keeps this
    honest: `attempt` short of `budget` proves the ladder still had
    rungs left when the disconnect landed, so a pass means the
    disconnect ended it rather than exhaustion getting there first.
    """
    connect_and_wait(ssh_host)
    drop_the_link(ssh_host, "unreachable")

    armed = wait_for_a_live_retry(ssh_host)
    assert armed["retry"]["attempt"] < armed["retry"]["budget"], armed
    ssh_host.disconnect()
    establishes = count(is_establish)
    generation = status(ssh_host)["generation"]

    def stays_down() -> None:
        row = assert_ladder_quiet(
            ssh_host,
            "a disconnected host",
            establishes=establishes,
            generation=generation,
        )
        assert not (row.get("rollup") or "").startswith(GAVE_UP_BAND), (
            f"a cancelled ladder still ran to its give-up: {row}"
        )
        rows = host_row_ids(roost)
        assert f"host:disconnect:{ssh_host.saved_id}" not in rows, rows

    hold(stays_down, 4.0)


def test_removing_a_host_during_a_scheduled_retry_leaves_no_ssh_children(
    ssh_host, roost, target
):
    """AC: removal cancels the ladder, processes and all.

    `remove` goes through `disconnect`, so this is the second of §3.4's
    two cancellation paths that leak processes when they are missed — a
    timer that fires against a host the registry no longer has would dial
    a machine nobody asked about and leave an `ssh` master behind it.
    """
    process = ui.owned_process(target)
    if process is None:
        pytest.skip("needs the harness's own UI process handle to walk its children")

    connect_and_wait(ssh_host)
    drop_the_link(ssh_host, "unreachable")

    armed = wait_for_a_live_retry(ssh_host)
    assert armed["retry"]["attempt"] < armed["retry"]["budget"], armed
    ssh_host.remove()
    establishes = count(is_establish)

    # The all-hosts form: a removed host has no row to read state off,
    # and its absence from the list is the state. What "nothing further"
    # means for it is therefore an OS fact — no further establish — plus
    # the ssh children waited for below.
    def nothing_further() -> None:
        assert count(is_establish) == establishes, invocations()
        assert ssh_host.saved_id not in saved_ids(roost), "a removed host came back"

    hold(nothing_further, 4.0)
    # Waited for rather than asserted flat through the window above: the
    # removal itself runs the tunnel's `-O exit`, so a sample taken in the
    # first moments would catch the teardown doing its job.
    wait_until(
        lambda: live_ssh_invocations(under=process.pid) == [],
        15.0,
        "every ssh the removed host owned to be gone",
    )
    assert ssh_host.saved_id not in saved_ids(roost)


def test_quitting_during_a_scheduled_retry_leaves_no_ssh_children(
    ssh_host, roost, target
):
    """AC: a quit outruns an armed timer.

    The window `EngineFeed::Quit` does not close by itself: it latches
    the exit but the feed keeps draining, so a `ReconnectDue` queued
    behind it would re-enter `connect_saved_host` after the user asked to
    leave — spawning an `ssh` master with `ControlPersist=60s` that
    outlives the app that made it. Two mechanisms answer that (the exit
    teardown aborts every armed handle and any in-flight establish; the
    due message is gated on the exit state), and this is where they are
    exercised together against a real quit.

    Relaunches the UI it ended rather than being the module's last test:
    the SIGTERM case below owns that slot and asserts a different thing
    (the *tunnel's* `-O exit` on the way out, which needs a live tunnel
    at signal time — and a live ladder means there is none). Same
    quit → launch cycle `test_sidebar_collapse_persistence.py` uses.
    """
    process = ui.owned_process(target)
    if process is None:
        pytest.skip("needs the harness's own UI process handle to quit and relaunch")

    connect_and_wait(ssh_host)
    drop_the_link(ssh_host, "unreachable")

    armed = wait_for_a_live_retry(ssh_host)
    assert armed["retry"]["attempt"] < armed["retry"]["budget"], armed
    establishes = count(is_establish)

    ui.quit(target)
    exit_code = process.wait(timeout=scaled_timeout(30.0))
    assert exit_code == 0, (
        f"the UI must end its run loop normally (exit {exit_code}); a "
        "non-zero status means it was killed rather than dropping App"
    )
    assert count(is_establish) == establishes, (
        "a timer fired after the quit was asked for"
    )
    # The exit path runs the tunnel's own `-O exit` on its way out, so
    # this is a settle rather than an instant: what must not survive is
    # an `ssh` still running once the app that spawned it is gone.
    wait_until(
        lambda: live_ssh_invocations() == [],
        15.0,
        "every ssh the quit UI owned to be gone",
    )

    # The module is not finished with this UI; every case after this one
    # needs one alive, and `ROOST_SSH_BIN` reaches the replacement out of
    # the same environment the first launch read it from.
    ui.launch(target)


# ---------------------------------------------------------------------------
# 3-5. The classified failures
# ---------------------------------------------------------------------------


def test_an_auth_failure_settles_disconnected_with_the_auth_copy(ssh_host):
    """`Permission denied (publickey).` — the most common real failure.

    What matters is that it is reported as itself: an auth problem is the
    user's to fix, and the copy names the remedy (`ssh <target>` in a
    terminal), so a classifier that fell through to the generic transport
    message would strand them.
    """
    reason = connect_expecting_failure(ssh_host, "auth-fail")
    assert AUTH_COPY in reason, reason


def test_a_changed_host_key_settles_disconnected_and_never_offers_to_accept(ssh_host):
    """The wary case. A changed host key is what a machine-in-the-middle
    looks like from here, so the copy must warn rather than offer a
    remedy — and must never carry the *unknown*-key remedy, which is to
    review and accept the key.

    ssh's changed-key blob contains the unknown-key blob's own sentence
    (`Host key verification failed.`), so this case is also the fence on
    classification order.
    """
    reason = connect_expecting_failure(ssh_host, "hostkey-changed")
    assert CHANGED_KEY_COPY in reason, reason
    assert CHANGED_KEY_WARNING in reason, reason
    assert UNKNOWN_KEY_REMEDY not in reason, reason


def test_a_missing_remote_binary_settles_disconnected_and_names_roost_session(
    ssh_host,
):
    """Exit 127: the host is reachable, the binary is not there.

    The remedy is about `roost-session`, not about ssh, so the copy has
    to say so — this is the one failure where the user's next step is an
    install on the far machine.
    """
    reason = connect_expecting_failure(ssh_host, "exit-127")
    assert NOT_FOUND_COPY in reason, reason
    assert "roost-session" in reason, reason


# ---------------------------------------------------------------------------
# C7: a signal is a graceful quit, not a crash (plan 039 §3.9)
# ---------------------------------------------------------------------------


def test_sigterm_during_teardown_flushes_state_and_tears_the_tunnel_down(
    ssh_host, target, request
):
    """AC: SIGTERM reaches the same path a menu Quit does.

    Before this commit the iced UI installed no signal handler, so a
    SIGTERM behaved exactly like a crash — no `Drop for App`, so no
    workspace flush and no tunnel `-O exit` (an ssh ControlMaster would
    strand until its own `ControlPersist` window expired; plan 038's known
    gap). Both halves are asserted through what a process leaves behind:
    the exit status is 0, which only the graceful quit path produces
    (plan 039 §3.9), and the fake `ssh` invocation log gets a teardown
    call against the tunnel's own control socket.

    This module, not `test_host_bootstrap.py`: both read the invocation
    log, but that module's `-O exit` is a **bootstrap job's** own
    throwaway control socket (§3.8's job-scoped master) — a different
    socket from the one this assertion is pinned to. This module is where
    the *tunnel's* `-O exit` (`SshTunnel`'s `Drop`) lives.

    SIGINT is not repeated here: it shares every line of the path this
    exercises from the signal handler onward (`observe_quit_signal`,
    `EngineFeed::Quit`, `ExitState::request`), and that shared path is
    what the unit test in `app.rs` covers.

    Necessarily the last thing this module does — SIGTERM ends the UI
    process it drives, and every test after it needs one alive. Skips
    loudly (`runs_alone`) rather than risk running that way: a
    whole-directory `pytest tools/roosttest` would otherwise collect this
    beside every other module and strand them all.
    """
    if not runs_alone(request):
        pytest.skip(
            "SIGTERMs the UI it drives, so it must be this module's own "
            "pytest invocation (`make e2e-host-ssh`) — collected beside "
            "other modules it would strand every test after it"
        )
    process = ui.owned_process(target)
    if process is None:
        pytest.skip("needs the harness's own UI process handle to signal")

    connect_and_wait(ssh_host)
    # The mux is up, so the tunnel's control socket exists for `-O exit`
    # to address once the signal lands.
    assert count(is_establish) >= 1, invocations()

    process.send_signal(signal.SIGTERM)
    # The only exit proof there is, and the only one there needs to be:
    # a status of 0 is reachable only through the run loop's own end
    # (`Drop for App`, the workspace flush, the tunnel teardown), so
    # waiting for a log line first would have been the same claim read
    # off a weaker surface.
    exit_code = process.wait(timeout=scaled_timeout(30.0))
    assert exit_code == 0, (
        f"the UI must end its run loop normally (exit {exit_code}); a "
        "non-zero status means it was killed rather than dropping App"
    )
    assert count(is_teardown) >= 1, invocations()
