"""Plan 058 §3.2 (#460): the `localhost` sentinel, reaching a daemon this
lane controls.

# What this proves

`localhost` is the target the seeded Connect row saves and the one a
person types, and `roost_ipc::ssh::classify` maps that exact string —
and no path that happens to name the same file — to this build's session
socket (`BundleProfile::session()`). Every other host lane registers a
socket **path** instead, which classifies as `UnixSocket`, so everything
gated on `HostTransportKind::Localhost` — the ↻ Restart verb standing
where ssh's ⬆ Update does, and the stop-and-relaunch behind it — had no
end-to-end cover at all.

# Owning a machine-wide sentinel

That socket is one path per build per user, so a lane that dials it
dials whatever is already there: a developer's own session, or the
daemon another host lane spawned a minute ago. This module moves the
path rather than the daemon. It mints a private root and points
`XDG_RUNTIME_DIR` (with its `XDG_*` siblings) at it **at import**,
before `conftest.py`'s session-scoped fixture launches the UI, so the
UI's own socket and the sentinel it resolves for `localhost` move
together. From there the fixture's daemon is the only thing that can
answer that path, a `roostctl` run from a test resolves the same one,
and the developer's instance is not merely left alone but unreachable —
which is also why `--roost-fresh` has nothing here to force-quit.

`HOME` is deliberately not redirected: the UI's own local shells are not
under test, and a private home would move font and config lookups for
nothing.

# Why its own pytest invocation

Two reasons, either sufficient. The import-time environment poisons
every module imported after it in the same process — the missing-daemon
lane's own reason — and the incarnation probe every host lane shares
cannot tell two connected hosts' tabs apart.

`pytestmark = pytest.mark.host_client` for the reason the other host
lanes carry it: a whole-directory run — `make e2e-mac` in particular,
where the Swift app answers `unknown-op` to every `host.*` op —
deselects it rather than failing it.

Condition waits only.
"""

from __future__ import annotations

import atexit
import contextlib
import functools
import json
import os
import platform
import shutil
import subprocess
import tempfile
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import pytest

if platform.system() == "Darwin":
    # Before any environment is touched. macOS resolves the session
    # profile out of `$HOME`, so moving the sentinel there drags
    # `~/Library` and the Mac app's own launch path along with it — a
    # different lane's problem (#390).
    pytest.skip(
        "the localhost sentinel is redirected through XDG_RUNTIME_DIR, which "
        "is Linux only; the macOS variant is #390",
        allow_module_level=True,
    )

import agent_jail  # noqa: E402
import session as sessionlib  # noqa: E402

# `/tmp` and a resolved path, for the reasons `session.make_env`
# documents for a root a caller lends it.
_ROOT = Path(tempfile.mkdtemp(prefix="roost-hs-", dir="/tmp")).resolve()
_RUN = _ROOT / "run"
# Makes it 0700 — under umask 0002 a plain `mkdir` leaves it
# group-writable and `validate_runtime_dir` refuses it.
agent_jail.make_private_runtime_dir(_RUN)
for _sibling in ("data", "state", "cache"):
    (_ROOT / _sibling).mkdir()

os.environ["XDG_RUNTIME_DIR"] = str(_RUN)
os.environ["XDG_DATA_HOME"] = str(_ROOT / "data")
os.environ["XDG_STATE_HOME"] = str(_ROOT / "state")
os.environ["XDG_CACHE_HOME"] = str(_ROOT / "cache")

# The one shell the daemon a restart relaunches gets. `make_env` sets
# this for the daemon it starts itself, but a relaunch comes out of the
# UI's environment instead, and a developer's zsh with Roost's shell
# integration would rewrite the titles and cwds a layout assertion reads
# (`session.py`'s header has the long form). `ROOST_SHELL_FEATURES` is
# not set beside it: `ui.py` sanitizes that name out of the UI's launch
# and `make_env` sets it for its own daemon, so exporting it here would
# reach neither.
os.environ["SHELL"] = "/bin/sh"
# The same binary for the fixture's daemon and for the one the UI
# relaunches, so a restart cannot come back as a different build for a
# reason no case chose. `ROOST_SESSION_FAKE_BUILD` is deliberately NOT
# here: the one case that wants a reduced-fidelity session passes it to
# its own daemon, and a relaunch that inherited it would come back
# reduced too, with nothing left to prove.
os.environ["ROOST_SESSION_BIN"] = str(sessionlib.session_binary())

# The UI is stood down by `ui.end_session`, a session-scoped fixture
# teardown, so by the time this runs nothing is left holding the root.
atexit.register(shutil.rmtree, _ROOT, ignore_errors=True)

import ui  # noqa: E402
from client import Roost, scaled_timeout  # noqa: E402
from host_probe import host_key  # noqa: E402
from test_host_client import (  # noqa: E402
    FAKE_BUILD,
    HostUnderTest,
    first_project,
    host_row_ids,
    host_status_row,
    quiet_tab,
    require_test_mode,
    start_session,
    takeback_in_place,
    wait_live_connect,
    wait_until,
)

# Borrowed rather than copied: both address a session by its profile's
# socket out of the inherited environment, which here is already the
# redirected one — so they reach the fixture's daemon and can reach no
# other. `test_host_local_spawn` sets nothing at import, so taking them
# costs this lane none of its own isolation.
from test_host_local_spawn import roostctl_session, running_session_id  # noqa: E402

pytestmark = pytest.mark.host_client

# What `roostctl session status` exits when nothing is listening
# (`STATUS_NOT_RUNNING_EXIT`, `crates/roost-cli/src/session.rs`).
NOT_RUNNING_EXIT = 3

# `host_state::TAKEN_OVER` on the wire, restated like this lane's other
# wire constants.
TAKEN_OVER = "taken-over"


@functools.cache
def client_libghostty_build() -> str:
    """The libghostty build this client pins, as a string.

    `roost-session identify` is compile-time identity — no socket, no
    profile — and this tree builds the daemon and the UI against one pin.
    Read from a **clean** environment because this lane runs with
    `ROOST_TEST_MODE=1`, the very gate that would otherwise let a
    developer's exported `ROOST_SESSION_FAKE_BUILD` answer here and make
    the card assertion tautological.
    """
    result = subprocess.run(
        [str(sessionlib.session_binary()), "identify"],
        env={"PATH": os.environ.get("PATH", "")},
        capture_output=True,
        text=True,
        timeout=scaled_timeout(30),
    )
    assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
    return json.loads(result.stdout)["libghostty_build"]


# ---------------------------------------------------------------------------
# The host, its daemon, and what the teardown is answerable for
# ---------------------------------------------------------------------------


@dataclass
class Ground:
    """This lane's `localhost` host, plus every session id the test has
    put on the sentinel socket or connected to there."""

    host: HostUnderTest
    caused: set[str] = field(default_factory=set)

    def start_daemon(self, **overrides: str) -> str:
        """Daemonize this lane's session and claim what it turned out to
        be.

        `Launch.verdict` carries a kind and a pid and nothing else, so
        the identity comes from `session.identify` — and it is recorded
        before anything else can fail, because it is what lets the
        teardown stop this daemon and refuse to stop any other.
        """
        start_session(self.host.env, **overrides)
        return self.claim(self.host.env.identify()["session_id"])

    def claim(self, session_id: str) -> str:
        self.caused.add(session_id)
        return session_id


@pytest.fixture
def ground(roost: Roost):
    """A saved `localhost` host and the profile its daemon will land in.

    Three things are settled before the daemon starts:

    * **The UI is the harness's own.** Its throwaway `ROOST_STATE_DIR`
      is where the daemon's state has to go, and a UI the harness did
      not launch has none this side can name. Under the redirected
      runtime dir there is never a running instance to reuse, so this is
      an assertion rather than the `precondition` its siblings use.
    * **Nothing already answers the sentinel.** A hard assertion in
      every mode, unlike `test_host_local_spawn`'s preconditioned twin:
      that lane shares the machine's real session socket with whoever
      else is on the box, and this one's lives in a directory minted
      seconds ago — so a listener there is this lane's own leak and
      never somebody's session.
    * **The saved layout is empty.** All three cases share
      `<UI state>/session`, the state dir `spawn_session` derives for a
      UI-spawned child (#397) and therefore the one a restart hydrates
      from, so a layout left behind by the case before would be restored
      into the case after. Nothing holds its lock at this moment: the
      only daemon that ever takes it is the one about to start.

    The teardown stops **an identity, not a socket** — whatever answers
    the sentinel is stopped only while its id is one this test brought
    up or connected to, and anything else is named rather than stopped
    (the `spawn_ground` rule). `SessionEnv.teardown` reaps behind that
    assertion, which is sound for the same reason the precondition is:
    nothing but this lane can bind that path.

    Function-scoped against a session-scoped UI, which is what orders
    this teardown ahead of the UI's own: `ui._remove_session_state`
    refuses a live daemon's nested `state.lock`, and by then there is no
    daemon left to hold one.
    """
    require_test_mode(roost)
    state_dir = ui.session_state_dir()
    assert state_dir is not None, (
        "the harness did not launch the UI, so where a session it spawns keeps "
        "state is unknowable — under this lane's redirected XDG_RUNTIME_DIR "
        "there should never have been an instance to reuse"
    )
    status = roostctl_session("status")
    assert status.returncode == NOT_RUNNING_EXIT, (
        f"a roost-session already answers this lane's own session socket "
        f"(`roostctl session status` exited {status.returncode}: "
        f"{status.stdout.strip()!r}). That directory was minted for this run, "
        "so it can only be a daemon an earlier case leaked"
    )

    session_state = state_dir / ui.DERIVED_SESSION_SUBDIR
    shutil.rmtree(session_state, ignore_errors=True)
    env = sessionlib.make_env(root=_ROOT, state_dir=session_state)

    label = f"localhost-{uuid.uuid4().hex[:8]}"
    added = roost.call("host.add", {"label": label, "target": "localhost"})["host"]
    made = Ground(
        HostUnderTest(roost=roost, env=env, saved_id=added["id"], label=label)
    )
    try:
        yield made
    finally:
        # The host goes before the session does, so the client is never
        # left dialing a socket the teardown is about to delete.
        with contextlib.suppress(Exception):
            roost.palette_dismiss()
        with contextlib.suppress(Exception):
            made.host.disconnect()
        with contextlib.suppress(Exception):
            made.host.remove()
        try:
            stop_what_this_lane_started(made)
        finally:
            env.teardown()


def stop_what_this_lane_started(ground: Ground) -> None:
    running = running_session_id()
    if running is None:
        return
    assert running in ground.caused, (
        f"the session on {ground.host.env.socket} is {running!r}, which this "
        f"lane never started (it caused {sorted(ground.caused)})"
    )
    stopped = roostctl_session("stop")
    assert stopped.returncode == 0, (
        f"stopping the session this lane started failed ({stopped.returncode}): "
        f"{stopped.stdout!r} / {stopped.stderr!r}"
    )


# ---------------------------------------------------------------------------
# The host modal, through `app.dialog_dump` / `app.dialog_answer`
# ---------------------------------------------------------------------------


def dialog_dump(roost: Roost) -> dict:
    return roost.call("app.dialog_dump", {})


def wait_dialog(roost: Roost, kind: str, timeout: float = 60.0) -> dict:
    def probe() -> dict | None:
        dump = dialog_dump(roost)
        return dump if dump.get("dialog") == kind else None

    return wait_until(probe, timeout, f"the {kind!r} dialog")


def answer(roost: Roost, action: str) -> dict:
    return roost.call("app.dialog_answer", {"action": action})


def activate(roost: Roost, item_id: str) -> None:
    """Press a palette row, the way a person reaches these verbs."""
    roost.palette_open("commands")
    try:
        roost.palette_activate(item_id)
    finally:
        roost.palette_dismiss()


# ---------------------------------------------------------------------------
# 1. The sentinel resolves to this lane's daemon
# ---------------------------------------------------------------------------


def test_a_localhost_host_reaches_this_lanes_daemon(ground: Ground, roost: Roost):
    """`host.add {"target": "localhost"}`, connected, and both halves of
    "this daemon" asserted.

    The session id is what a socket-path host can never claim: the UI
    resolved the path itself, out of the same profile the fixture put its
    daemon in. The tab is what makes it a statement about the
    *connection* rather than about a string — opened through the window
    (⌘T's own command, on the host project focusing selected) and read
    back on the daemon's own socket.

    `host.status` carries no transport field, so nothing here asserts
    one; the verb set the case below reads is where the transport shows.
    """
    session_id = ground.start_daemon()
    ground.host.connect_and_wait()

    row = wait_live_connect(ground.host)
    assert row["target"] == "localhost", row
    assert row["connect"]["session_id"] == session_id, (
        "the UI connected to some session other than the one this lane put on "
        f"the sentinel socket ({ground.host.env.socket}): {row}"
    )

    with ground.host.client() as session:
        project = first_project(session)
        seed = quiet_tab(session, project, ground.host.env.launch_cwd)
        # Focusing is the attach (§3.4), and it is also what selects the
        # host project a new tab then lands on.
        host_key(roost, seed)
        before = set(session.project_tab_ids(project))

        activate(roost, "new_tab")

        wait_until(
            lambda: set(session.project_tab_ids(project)) - before,
            30.0,
            "the tab the window opened to appear on this lane's daemon",
        )


# ---------------------------------------------------------------------------
# 2. The Localhost transport's own verb, and the restart behind it
# ---------------------------------------------------------------------------


def watch_the_restart(ground: Ground, timeout: float = 180.0) -> tuple[dict, list[str]]:
    """Poll until the host is back **connected at full fidelity**, and
    hand back every state seen on the way. Same shape, and the same
    reasoning for settling on `reduced_fidelity` rather than on
    `connected`, as `test_host_bootstrap`'s `watch_the_update_job`.

    Every session id seen on the way is claimed, so a case that fails
    mid-restart still leaves the teardown able to stop what the relaunch
    started.
    """
    states: list[str] = []

    def landed() -> dict | None:
        row = host_status_row(ground.host.roost, ground.host.saved_id)
        states.append(row["state"])
        connect = row.get("connect")
        if connect is not None:
            ground.claim(connect["session_id"])
        if row["state"] != "connected" or connect is None or connect["reduced_fidelity"]:
            return None
        return row

    row = wait_until(landed, timeout, "the restart to land the host at full fidelity", 0.01)
    return row, states


def test_a_reduced_fidelity_localhost_host_restarts_from_the_palette_and_comes_back_whole(
    ground: Ground, roost: Roost
):
    """R14's route on the transport R14 could not reach: this machine's
    own session, restarted from inside the window.

    The ssh sibling
    (`test_host_bootstrap.test_a_reduced_fidelity_ssh_host_updates_from_the_palette_and_comes_back_whole`)
    offers ⬆ Update, because a remote binary can be replaced. Here there
    is nothing to install — the binary is already this build, only the
    *process* is stale — so `fidelity_action` answers `Restart` and the
    verb set is the transport's own signature: `host:restart:` present,
    `host:update:` absent. That pair is the proof this connection really
    was classified `Localhost`, which is the one thing `host.status` will
    not say.

    Confirming has to let go of the session **before** the job owns it,
    and that is read once with no wait in front of it:
    `app.dialog_answer` runs the confirm handler to completion before it
    replies, and the disconnect inside it is synchronous, so this is a
    fence rather than a race. `taken-over` is sampled for the whole job
    for the reason the ssh sibling names — with a stream still up when
    the job stops the session, the band can end up reading "taken over"
    by nobody.

    The layout is compared by **cwd**, never by title: a restored shell
    is a fresh one and is free to rewrite what it is called.
    """
    started = ground.start_daemon(ROOST_SESSION_FAKE_BUILD=FAKE_BUILD)
    identity = ground.host.env.identify()
    assert identity["libghostty_build"] == FAKE_BUILD, identity

    with ground.host.client() as session:
        quiet_tab(session, first_project(session), ground.host.env.launch_cwd)
        before = [row["cwd"] for row in session.tabs()]

    ground.host.connect_and_wait()
    row = wait_live_connect(ground.host)
    assert row["connect"]["reduced_fidelity"] is True, row
    assert row["connect"]["session_id"] == started, row

    restart_id = f"host:restart:{ground.host.saved_id}"
    ids = host_row_ids(roost)
    assert restart_id in ids, sorted(ids)
    assert f"host:update:{ground.host.saved_id}" not in ids, sorted(ids)

    activate(roost, restart_id)

    card = wait_dialog(roost, "confirm_restart")
    assert FAKE_BUILD in card["body"], card
    assert client_libghostty_build() in card["body"], (client_libghostty_build(), card)

    answer(roost, "confirm")
    handed_over = host_status_row(roost, ground.host.saved_id)
    assert handed_over["state"] == "disconnected", (
        "confirming has to let go of the session *before* the restart owns it "
        f"— the host was still {handed_over['state']!r}: {handed_over}"
    )
    assert handed_over.get("connect") is None, handed_over

    landed, states = watch_the_restart(ground)
    assert landed["connect"]["reduced_fidelity"] is False, landed
    assert TAKEN_OVER not in states, (
        f"the host read {TAKEN_OVER!r} during the restart (states "
        f"{sorted(set(states))} over {len(states)} samples) — confirming has to "
        "disconnect before the job owns the session"
    )
    assert landed["connect"]["session_id"] != started, (
        "the host came back on the session the restart was supposed to have "
        f"stopped: {landed}"
    )

    with ground.host.client() as session:
        restored = session.tabs()
    assert [tab["cwd"] for tab in restored] == before, restored


# ---------------------------------------------------------------------------
# 3. R15's takeback, on this transport
# ---------------------------------------------------------------------------


def test_a_takeover_on_a_localhost_host_is_taken_back_in_place(
    ground: Ground, roost: Roost
):
    """R15's sequence, called rather than copied, against a session
    reached through the sentinel: the foreground moves to a second
    client, the frame stays live, and `host.connect` takes it back on the
    connection that was already there."""
    ground.start_daemon()
    takeback_in_place(ground.host, roost)
