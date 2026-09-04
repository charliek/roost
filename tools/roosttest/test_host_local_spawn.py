"""Plan 044 W2 (#397): a UI running under `ROOST_STATE_DIR` can spawn a
`localhost` session, and that session gets a state dir of its own.

# What this proves

`test_host_local_missing_daemon.py` is the twin that pins the local
spawn ladder's *failure* rung. This is the success rung — the one no
lane could reach at all before #397. The harness always launches its UI
under an absolute `ROOST_STATE_DIR` (`ui.py`'s throwaway dir); the
daemon inherited that variable, resolved the *UI's* state dir, found the
`state.lock` the UI already held, and refused to start. The refusal was
honest and useless: the isolation seam collided with itself, and the UI
reported it as `reason = "roost-session failed to start"` with the
daemon's own "is using this state directory" sentence in `detail`.

`spawn_and_read_verdict` now hands the child `<the launcher's state
dir>/session`, so the isolation is inherited rather than collided with.
Both halves of AC6 are asserted rather than inferred: the daemon's state
really lands under the UI's throwaway dir (`state.lock`, then
`state.json`), and the daemon is still *findable*, because the seam
moves state and nothing else — `roostctl session status` reaches it by
socket exactly as before.

# Why its own pytest invocation

Not import-time env, which is the missing-daemon twin's reason; this
module sets none. What is shared here is the **daemon**. The session the
UI spawns binds the default Session socket for this build — the one
every other host lane's UI would dial (finding a stranger's session
instead of spawning or settling one), and the one `roostctl session
status|stop` address. So the lane demands that socket free before it
runs, records the `session_id` of the daemon it brings up, and stops
that id and no other; none of which is sound with another host lane
driving a UI beside it. Own list, own
target, own CI steps, ordered after the other host steps.

`pytestmark = pytest.mark.host_client` for the reason the other host
lanes carry it: a whole-directory run — `make e2e-mac` in particular,
where the Swift app answers `unknown-op` to every `host.*` op —
deselects it rather than failing it.

Condition waits only.
"""

from __future__ import annotations

import contextlib
import subprocess
import uuid
from dataclasses import dataclass
from pathlib import Path

import pytest
import ui
from client import Roost, scaled_timeout
from test_host_client import wait_until
from util import precondition, roostctl_path

pytestmark = pytest.mark.host_client

# What `roostctl session status` exits when nothing is listening
# (`crates/roost-cli/src/session.rs`'s `STATUS_NOT_RUNNING_EXIT`, chosen
# for `systemctl status`'s reason: a script should be able to branch on
# the code alone). Restated rather than imported, like the other host
# lanes' copy constants — this is a shell-facing contract, and changing
# it should have to be done twice, on purpose.
NOT_RUNNING_EXIT = 3

# The first line `roostctl session status` prints for a live session
# (`print_identity`). It is the only handle a shell gets on *which*
# session is answering — `status` never prints a state dir — and this
# lane needs one, because "the session I started" and "a session" are
# not the same claim at teardown.
SESSION_ID_FIELD = "session_id="

# The daemon's own words when it refuses a state dir another process is
# holding (`crates/roost-session/src/start.rs`; `ui.py` needles the same
# sentence). Nothing here asserts it — this lane is the positive
# control — but the #397 negative control reads it out of `detail`, so
# the failure message points at it by name.
STATE_LOCK_REFUSAL = "is using this state directory"


def roostctl_session(*args: str, timeout: float = 60.0) -> subprocess.CompletedProcess:
    """Run `roostctl session <verb>` the way a user would.

    The environment is inherited untouched on purpose. These verbs
    address the session by its profile's **socket**, and
    `ROOST_STATE_DIR` moves state and nothing else — so even a developer
    with the seam exported in their own shell reaches the very daemon
    this lane's UI spawned under it. That is the second half of AC6, and
    running the CLI bare is what asserts it instead of assuming it.

    A timeout is re-raised as an `AssertionError` rather than left as
    `TimeoutExpired`, which out of a teardown would be an opaque fixture
    error — and, worse, would abandon a daemon on the shared socket for
    whichever lane runs next in the same CI job. Naming it is the only
    way that gets noticed.
    """
    budget = scaled_timeout(timeout)
    try:
        return subprocess.run(
            [roostctl_path(), "session", *args],
            capture_output=True,
            text=True,
            timeout=budget,
        )
    except subprocess.TimeoutExpired as error:
        raise AssertionError(
            f"`roostctl session {' '.join(args)}` did not return within "
            f"{budget:.0f}s; a roost-session may still be listening on this "
            "build's default session socket"
        ) from error


def running_session_id() -> str | None:
    """The id of the session answering the default socket, or `None` when
    nothing answers there.

    Doubles as AC6's discoverability assertion: a state dir the launcher
    derived is still reachable by socket, so an exit-0 `status` with an
    identity on it is exactly the claim being made. Any other exit is a
    fault, not an answer, and says so.
    """
    result = roostctl_session("status")
    if result.returncode == NOT_RUNNING_EXIT:
        return None
    assert result.returncode == 0, (
        f"`roostctl session status` failed ({result.returncode}): "
        f"{result.stdout!r} / {result.stderr!r}"
    )
    for line in result.stdout.splitlines():
        if line.startswith(SESSION_ID_FIELD):
            return line[len(SESSION_ID_FIELD) :].strip()
    raise AssertionError(
        f"`roostctl session status` answered without a {SESSION_ID_FIELD} line: "
        f"{result.stdout!r}"
    )


@dataclass
class SpawnGround:
    """The UI's state dir, plus the identity of the session this run put on
    the default socket once it has one."""

    state_dir: Path
    session_id: str | None = None


@pytest.fixture
def spawn_ground(roost: Roost):
    """The UI's own state dir, once this run is cleared to spawn a
    session into it — and the reaper for whatever it spawns.

    Both preconditions go through `precondition` rather than
    `pytest.skip`, because the two environments they separate want
    opposite answers:

    * **The UI's state dir must be known**, which it is only when the
      harness launched the UI. A non-fresh run reusing a developer's own
      instance cannot say where that instance's spawned session would
      keep state, so there is nothing to assert — a skip, by name.
    * **Nothing may already be listening** on this build's default
      Session socket. In fresh mode (and therefore CI) a live daemon is
      a hard failure: the UI would dial it instead of spawning one, and
      a leaked daemon that made this lane green would hide the very
      regression it exists to catch, forever. On a developer's box it is
      their own session — one this lane must never stop — so there it is
      a skip.

    `precondition` is exactly that split.

    **The teardown stops an identity, not a socket.** "Nothing was
    listening at setup" is a time-of-check fact, and the stop happens
    much later: a developer who starts their own session while the lane
    runs would otherwise have it stopped, and every PTY under it reaped.
    So the test claims the `session_id` of the daemon it brought up
    (`SpawnGround.session_id`), and the teardown stops only while that
    same id is still the one answering. Anything else — nothing running,
    or a stranger — is left alone.

    Running before the UI's own teardown is also what keeps
    `ui._remove_session_state` from having to refuse a live daemon's
    nested `state.lock`.
    """
    state_dir = ui.session_state_dir()
    precondition(
        state_dir is not None,
        "the UI's state dir is known only when the harness launched the UI; a "
        "non-fresh run reusing a developer's instance cannot tell where a "
        "session it spawned would keep its state",
    )
    status = roostctl_session("status")
    precondition(
        status.returncode == NOT_RUNNING_EXIT,
        "a roost-session is already listening on this build's default session "
        f"socket (`roostctl session status` exited {status.returncode}: "
        f"{status.stdout.strip()!r}) — the UI would dial it rather than spawn "
        "one, and stopping someone else's session is not this lane's to do",
    )

    ground = SpawnGround(state_dir=state_dir)
    yield ground

    # Disowned before any I/O, so the fixture ends in a defined state
    # whatever the probe below does.
    owned, ground.session_id = ground.session_id, None
    if owned is None:
        # The test never got far enough to identify a session. If one is
        # listening now it is not provably this lane's, so it is left
        # alone — and if it *is* one this run leaked, `ui.py`'s state
        # sweep refuses loudly rather than deleting its state.
        return
    running = running_session_id()
    if running is None:
        return
    assert running == owned, (
        f"the session on the default socket is {running!r}, not the {owned!r} "
        "this lane started; refusing to stop somebody else's session"
    )
    stopped = roostctl_session("stop")
    assert stopped.returncode == 0, (
        f"stopping the session this lane started failed ({stopped.returncode}): "
        f"{stopped.stdout!r} / {stopped.stderr!r}"
    )


@pytest.fixture
def localhost_host(roost: Roost):
    """A `localhost` host, saved but not yet connected."""
    label = f"local-spawn-{uuid.uuid4().hex[:8]}"
    added = roost.call("host.add", {"label": label, "target": "localhost"})["host"]
    host_id = added["id"]
    try:
        yield host_id
    finally:
        with contextlib.suppress(Exception):
            roost.palette_dismiss()
        with contextlib.suppress(Exception):
            roost.call("host.disconnect", {"id": host_id})
        with contextlib.suppress(Exception):
            roost.call("host.remove", {"id": host_id})


def status(roost: Roost, host_id: str) -> dict:
    """This host's one `host.status` row."""
    return roost.host_status(host_id)["hosts"][0]


def settled_message(row: dict) -> str:
    """Why a settled row is a verdict worth naming, not a slow success."""
    message = f"the localhost spawn settled instead of connecting: {row}"
    if STATE_LOCK_REFUSAL in (row.get("detail") or ""):
        message += (
            "\n\nThat detail is #397 itself: the spawned roost-session resolved "
            "the UI's own state dir and refused the lock the UI holds, so the "
            "launcher is not deriving <ROOST_STATE_DIR>/session for the child."
        )
    return message


def wait_connected(roost: Roost, host_id: str, timeout: float = 30.0) -> dict:
    """Wait for the spawn ladder to reach `connected`, or fail with the row.

    A localhost attempt never retries (plan 042 W2): it settles
    `disconnected` after exactly one attempt. A settle is therefore a
    verdict rather than a slow success, and waiting the whole budget out
    on one would throw away the `reason` and `detail` that say why —
    which is precisely what a #397 regression puts there.
    """
    last: dict = {}

    def connected() -> dict | None:
        nonlocal last
        last = status(roost, host_id)
        if last["state"] == "connected":
            return last
        # `generation` counts attempts *started*, so `disconnected` past
        # the first one is a settle and not the moment before the ladder
        # got going.
        if last["state"] == "disconnected" and last["generation"] >= 1 and "retry" not in last:
            raise AssertionError(settled_message(last))
        return None

    try:
        return wait_until(connected, timeout, "the localhost session to connect")
    except TimeoutError as error:
        raise AssertionError(
            f"the localhost host never connected; last host.status row: {last}"
        ) from error


def test_a_localhost_connect_under_the_seam_spawns_a_session_of_its_own(
    roost: Roost, spawn_ground, localhost_host
):
    """AC6: Connect on a `localhost` host from a UI running under
    `ROOST_STATE_DIR` reaches `connected`, the session it started keeps
    its state at `<seam>/session`, and `roostctl` still finds it by
    socket."""
    result = roost.call("host.connect", {"id": localhost_host})
    # The op reports what was asked for, not the far end's verdict.
    assert result["state"] in ("disconnected", "connecting", "connected"), result

    row = wait_connected(roost, localhost_host)
    # A clean connect says nothing further: `detail` is the long form of
    # a `reason` there is none of, and `retry` appears only with a rung
    # armed.
    assert "detail" not in row, row
    assert "retry" not in row, row

    # Identity first, before any later assertion can fail: this both makes
    # AC6's other half — a derived state dir stays discoverable, because
    # the seam never moves the socket — and hands the teardown the one
    # thing that lets it stop *this* session rather than whatever happens
    # to be on the socket by then.
    spawn_ground.session_id = running_session_id()
    assert spawn_ground.session_id is not None, (
        "`roostctl session status` found nothing on the default session socket, "
        "though the UI just connected to a session it started there"
    )

    session_state = spawn_ground.state_dir / ui.DERIVED_SESSION_SUBDIR
    lock = session_state / "state.lock"
    found = (
        sorted(p.name for p in session_state.iterdir())
        if session_state.is_dir()
        else "<no such directory>"
    )
    assert lock.is_file(), (
        f"{lock} is missing — the spawned session did not take the derived "
        f"state dir. That directory holds {found}"
    )

    # Both of these are already true by the time `connected` is: the daemon
    # takes its lock before anything else, and `serve()` hydrates — seeding
    # a first project at the launch cwd, a write-through mutation — before
    # it binds the socket, so `state.json` is on disk before the session can
    # answer at all. The poll is a belt against a slow filesystem, not a
    # claim that the file trails the connect.
    state_json = session_state / "state.json"
    wait_until(state_json.is_file, 15.0, f"{state_json} to be written")
