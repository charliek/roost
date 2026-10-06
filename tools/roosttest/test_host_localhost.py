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
import dataclasses
import json
import os
import platform
import shutil
import signal
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
#
# A copy rather than the tree's binary itself, so the update cases (plan
# 076) can stand a different build in its place: with
# `ROOST_SESSION_BIN` set it is the only restart candidate the UI ever
# identifies (D4). `restore_candidate` puts the copy back after each.
# `_REAL_SESSION` is read before the override moves: from here on
# `session_binary()` answers `_CANDIDATE`, which a case may have replaced.
_REAL_SESSION = sessionlib.session_binary()
_CANDIDATE = _ROOT / "candidate" / "roost-session"
_CANDIDATE.parent.mkdir()
shutil.copy2(_REAL_SESSION, _CANDIDATE)
os.environ["ROOST_SESSION_BIN"] = str(_CANDIDATE)

# The UI is stood down by `ui.end_session`, a session-scoped fixture
# teardown, so by the time this runs nothing is left holding the root.
atexit.register(shutil.rmtree, _ROOT, ignore_errors=True)

import ui  # noqa: E402
from client import Roost, RoostError, scaled_timeout  # noqa: E402
from host_probe import host_key  # noqa: E402
from test_host_client import (  # noqa: E402
    FAKE_BUILD,
    HostUnderTest,
    band_menu,
    described_build,
    menu_items,
    wait_host_section,
    first_project,
    host_row_ids,
    host_status_row,
    quiet_tab,
    require_test_mode,
    start_session,
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


def client_libghostty_build() -> str:
    """The libghostty build this client pins, as a string."""
    return sessionlib.real_identity()["libghostty_build"]


# ---------------------------------------------------------------------------
# The host, its daemon, and what the teardown is answerable for
# ---------------------------------------------------------------------------


@dataclass
class Ground:
    """This lane's `localhost` host, plus every session id the test has
    put on the sentinel socket or connected to there."""

    host: HostUnderTest
    caused: set[str] = field(default_factory=set)
    #: The pid the launcher reported for the daemon now on the sentinel
    #: socket, for the one case that kills it outright.
    pid: int | None = None

    def start_daemon(self, **overrides: str) -> str:
        """Daemonize this lane's session and claim what it turned out to
        be.

        `Launch.verdict` carries a kind and a pid and nothing else, so
        the identity comes from `session.identify` — and it is recorded
        before anything else can fail, because it is what lets the
        teardown stop this daemon and refuse to stop any other.
        """
        self.pid = start_session(self.host.env, **overrides).verdict.pid
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
    env = sessionlib.make_env(root=_ROOT, state_dir=session_state, binary=_REAL_SESSION)

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
    # Plan 063 AC10, and it belongs in *this* lane above all: a saved
    # `localhost` host is exactly what plan 063 calls the slot, so a UI
    # that had come up in session mode would render this host as its own
    # local band and withhold half its verbs. Every assertion below is
    # written against the in-process rendering.
    assert roost.identify()["local_backend"] == "in-process", roost.identify()

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

    The restart lets go of the stream once it has re-checked what it
    stops, and before it stops it (plan 076 D8). The states are sampled
    for the whole restart for the reason the ssh sibling names: a client
    that still had a stream up when the session stopped would hear it.

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

    landed, states = watch_the_restart(ground)
    assert landed["connect"]["reduced_fidelity"] is False, landed
    # Everything a *let-go* host can be while the restart runs, and
    # nothing else. `stopped` is the tell: it is what the far side's
    # `session.stopping` looks like to a client that was still listening,
    # which is precisely the client this confirm was supposed to have
    # disconnected.
    outside = sorted(set(states) - {"connected", "disconnected", "connecting"})
    assert not outside, (
        f"the host observed {outside} while the restart ran — a disconnected "
        f"client cannot hear the session it no longer holds (states "
        f"{sorted(set(states))} over {len(states)} samples)"
    )
    assert landed["connect"]["session_id"] != started, (
        "the host came back on the session the restart was supposed to have "
        f"stopped: {landed}"
    )

    with ground.host.client() as session:
        restored = session.tabs()
    assert [tab["cwd"] for tab in restored] == before, restored


# ---------------------------------------------------------------------------
# 3. A session killed outright, and the reconnect that finds its successor
# ---------------------------------------------------------------------------


def test_a_killed_session_started_again_comes_back_connected(ground: Ground):
    """`kill -9` the daemon, start a fresh one, and the ladder comes back
    `connected` on the new incarnation.

    SIGKILL rather than `roostctl session stop`: a stop sends
    `session.stopping`, which is terminal by contract and ends the
    ladder. A kill is the drop auto-reconnect exists for — the socket
    dies with no envelope at all — and nothing here asks the UI to
    reconnect, so what lands the host is the schedule.

    A restarted session is simply a different session, and the band says
    so by naming its id.
    """
    first = ground.start_daemon()
    ground.host.connect_and_wait()
    assert wait_live_connect(ground.host)["connect"]["session_id"] == first

    killed = ground.pid
    assert killed is not None, "the launcher reported no pid, so there is none to kill"
    os.kill(killed, signal.SIGKILL)
    ground.host.env.wait_pid_gone(killed)

    second = ground.start_daemon()
    assert second != first, "the relaunch came back as the same incarnation"

    # Generous, and deliberately so: the ladder backs off while nothing
    # is listening, so the first attempt after the relaunch can be a few
    # rungs in.
    row = wait_live_connect(ground.host, timeout=120.0)
    assert row["connect"]["session_id"] == second, (
        "the host came back on something other than the session this lane "
        f"just started: {row}"
    )


# ---------------------------------------------------------------------------
# 4. The seed row's label, when an SSH host already holds the plain one
# ---------------------------------------------------------------------------


def test_the_seed_row_steps_past_a_label_an_ssh_host_already_holds(
    ground: Ground, roost: Roost
):
    """Plan 063 §D7's collision rule, reached the way a person reaches
    it — by pressing `Connect Host: localhost`.

    The rule lives in bootstrap's `slot_label`, but the palette is a
    second caller, and a literal `"localhost"` there fails
    `Workspace::add_host`'s duplicate-label check the moment an SSH host
    is called that. The failure is quiet: the row acts, nothing is saved,
    and the picker's whole point — reaching this machine's session
    without an Add Host detour — is gone.

    This lane above all, because the row *connects*: the sentinel it
    dials is redirected here, so the daemon it reaches is the one this
    fixture started and the teardown can account for.
    """
    # Claimed first, so the row's `SpawnIfMissing` connect dials a daemon
    # this lane owns instead of starting one nothing reaped.
    ground.start_daemon()
    # The seed row is gated on "no saved host has a Localhost transport",
    # so this lane's own localhost host stands down for the case.
    ground.host.remove()

    squatter = roost.call(
        "host.add", {"label": "localhost", "target": "ssh://squatter.invalid"}
    )["host"]
    seeded_id: str | None = None
    try:
        assert "host:connect_seed" in host_row_ids(roost), host_row_ids(roost)

        activate(roost, "host:connect_seed")

        def seeded() -> dict | None:
            rows = roost.host_status()["hosts"]
            return next((row for row in rows if row["target"] == "localhost"), None)

        row = wait_until(seeded, 30.0, "the seeded localhost host to be saved")
        seeded_id = row["id"]
        assert row["label"] == "localhost (2)", (
            "the seed row used the literal label an SSH host already holds "
            f"instead of stepping past it: {row}"
        )
    finally:
        for host_id in (seeded_id, squatter["id"]):
            if host_id is None:
                continue
            with contextlib.suppress(Exception):
                roost.call("host.disconnect", {"id": host_id})
            with contextlib.suppress(Exception):
                roost.call("host.remove", {"id": host_id})


def test_the_picker_row_steps_past_the_same_label(ground: Ground, roost: Roost):
    """The second caller of §D7's rule: `New Project on…`'s `localhost`
    row when this machine's session is not saved yet.

    Its own case rather than a second phase of the one above, because it
    is a different dispatch (`host:create_on_localhost` →
    `create_on_localhost`) reached through a different frame, and the
    literal label was written out twice.
    """
    ground.start_daemon()
    ground.host.remove()

    squatter = roost.call(
        "host.add", {"label": "localhost", "target": "ssh://squatter.invalid"}
    )["host"]
    seeded_id: str | None = None
    try:
        roost.palette_open("commands")
        try:
            picker = roost.palette_activate("host:new_project_on")
            rows = {item["id"] for item in picker["items"]}
            assert "host:create_on_localhost" in rows, picker
            roost.palette_activate("host:create_on_localhost")
        finally:
            roost.palette_dismiss()

        def seeded() -> dict | None:
            hosts = roost.host_status()["hosts"]
            return next((row for row in hosts if row["target"] == "localhost"), None)

        row = wait_until(seeded, 30.0, "the picker's localhost host to be saved")
        seeded_id = row["id"]
        assert row["label"] == "localhost (2)", (
            "the picker's localhost row used the literal label an SSH host "
            f"already holds instead of stepping past it: {row}"
        )
    finally:
        for host_id in (seeded_id, squatter["id"]):
            if host_id is None:
                continue
            with contextlib.suppress(Exception):
                roost.call("host.disconnect", {"id": host_id})
            with contextlib.suppress(Exception):
                roost.call("host.remove", {"id": host_id})


# ---------------------------------------------------------------------------
# 5. What `host.status` says about updating (plan 076 D3/D4)
# ---------------------------------------------------------------------------
#
# Every case runs its daemon from a copy of this tree's `roost-session`
# with a `.test-identity` sidecar beside it, so one binary answers as
# whichever build the case needs. The UI's only restart candidate is
# `_CANDIDATE` (`ROOST_SESSION_BIN`), which a case may also replace.


@pytest.fixture
def restore_candidate():
    """Put the real binary back at `_CANDIDATE` after a case replaced it."""
    yield
    _CANDIDATE.unlink(missing_ok=True)
    sessionlib.identity_sidecar(_CANDIDATE).unlink(missing_ok=True)
    shutil.copy2(_REAL_SESSION, _CANDIDATE)


def start_as(ground: Ground, name: str, **identity) -> str:
    """Start this lane's daemon from a copy that identifies as
    `identity` (any of `app_version`, `dev`, `git_sha`)."""
    copy = ground.host.env.root / f"build-{name}" / "roost-session"
    copy.parent.mkdir(exist_ok=True)
    shutil.copy2(_REAL_SESSION, copy)
    sessionlib.plant_identity(copy, **identity)
    env = dataclasses.replace(ground.host.env, binary=copy)
    ground.pid = start_session(env).verdict.pid
    session_id = ground.claim(ground.host.env.identify()["session_id"])
    if "app_version" in identity:
        assert ground.host.env.identify()["app_version"] == identity["app_version"]
    return session_id


def fake_candidate(identify_stdout: str) -> None:
    """Stand a script at `_CANDIDATE` whose `identify` prints this."""
    _CANDIDATE.unlink()
    _CANDIDATE.write_text(f"#!/bin/sh\nprintf '%s\\n' '{identify_stdout}'\n")
    _CANDIDATE.chmod(0o755)


def host_update(host: HostUnderTest, session_id: str, timeout: float = 60.0) -> dict:
    """The host's `update` object once its restart candidate has been
    identified for this session — a target or a reason, either way."""

    def probe() -> dict | None:
        row = host_status_row(host.roost, host.saved_id)
        connect = row.get("connect")
        update = row.get("update")
        if connect is None or connect["session_id"] != session_id or update is None:
            return None
        restart = update["restart"]
        return update if "target" in restart or "why" in restart else None

    return wait_until(probe, timeout, "the host's restart candidate to be identified")


def connect_as(ground: Ground, name: str, **identity) -> dict:
    session_id = start_as(ground, name, **identity)
    ground.host.connect_and_wait()
    assert wait_live_connect(ground.host)["connect"]["session_id"] == session_id
    return host_update(ground.host, session_id)


def this_roost(update: dict) -> str:
    """The ` · this Roost dev …` an unordered line ends with, when this
    client is a dev build (plan 076 D6)."""
    client = update["client"]
    if not client.get("dev"):
        return ""
    return f" · this Roost dev {client['sha']}" if client.get("sha") else " · this Roost dev"


def leave_rows() -> list:
    return [None, "Disconnect", "Stop Session…"]


def test_the_same_build_on_both_sides_is_up_to_date(ground: Ground, restore_candidate):
    real = sessionlib.real_identity()
    update = connect_as(ground, "same")
    # Two dev builds of one version are only `Same` with a sha to
    # compare, which a tree built without git does not have.
    want = "unordered" if real.get("dev") and not real.get("git_sha") else "up-to-date"
    assert update["state"] == want, update
    assert update["session"]["version"] == real["app_version"], update
    assert update["restart"]["offered"] is True, update
    assert update["restart"]["target"]["source"] == "override", update
    assert update["restart"]["target"]["version"] == real["app_version"], update
    # Plan 076 D6: the plain maintenance Restart is on the menu only.
    session = described_build(update["session"])
    line = (
        f"Session {session}{this_roost(update)}"
        if want == "unordered"
        else f"Session {session} · up to date"
    )
    assert band_menu(ground.host.roost, ground.host.saved_id) == [
        f"# {line}",
        "Restart Session…",
        *leave_rows(),
    ]
    rows = host_row_ids(ground.host.roost)
    assert f"host:restart:{ground.host.saved_id}" not in rows, sorted(rows)


def test_an_older_session_with_a_newer_candidate_is_staged(ground: Ground, restore_candidate):
    update = connect_as(ground, "old", app_version="0.0.1")
    assert update["state"] == "staged", update
    assert update["session"]["version"] == "0.0.1", update
    assert update["restart"]["offered"] is True, update
    assert update["restart"]["target"]["version"] == sessionlib.real_identity()["app_version"], update
    assert update["restart"]["target"]["source"] == "override", update
    assert "staged" not in update, "nothing is installed on localhost"
    # Localhost's staged reads "available": nothing was installed.
    target = described_build(update["restart"]["target"])
    line = f"Session {described_build(update['session'])} · {target} available"
    assert band_menu(ground.host.roost, ground.host.saved_id) == [
        f"# {line}",
        "Restart Session…",
        *leave_rows(),
    ]
    rows = host_row_ids(ground.host.roost)
    assert f"host:restart:{ground.host.saved_id}" in rows, sorted(rows)
    assert f"host:install:{ground.host.saved_id}" not in rows, sorted(rows)
    # A project row's host block opens with the host's name.
    section = wait_host_section(
        ground.host.roost,
        ground.host.saved_id,
        lambda section: section["projects"],
        "the host's projects to reach the sidebar",
    )
    project = menu_items(
        ground.host.roost.context_menu_dump({"project_id": section["projects"][0]["key"]})
    )
    assert project[-6:] == [None, f"# {ground.host.label} · {line}", "Restart Session…", *leave_rows()], project


def test_a_newer_session_is_session_newer_and_never_restarted_older(
    ground: Ground, restore_candidate
):
    update = connect_as(ground, "new", app_version="99.0.0")
    assert update["state"] == "session-newer", update
    assert update["blocked"] is False, update
    assert update["restart"] == {"offered": False, "why": "older"}, update
    assert band_menu(ground.host.roost, ground.host.saved_id) == [
        f"# Session {described_build(update['session'])} · newer than this Roost"
        " · its roost-session is older",
        "Disconnect",
        "Stop Session…",
    ]


def test_a_dev_session_at_this_version_is_unordered(ground: Ground, restore_candidate):
    update = connect_as(
        ground,
        "dev",
        app_version=sessionlib.real_identity()["app_version"],
        dev=True,
        git_sha="0000000",
    )
    assert update["state"] == "unordered", update
    assert update["session"] == {
        "version": sessionlib.real_identity()["app_version"],
        "dev": True,
        "sha": "0000000",
    }, update
    # Unordered is not a downgrade: the candidate stays usable.
    assert update["restart"]["offered"] is True, update
    assert band_menu(ground.host.roost, ground.host.saved_id) == [
        f"# Session {described_build(update['session'])}{this_roost(update)}",
        "Restart Session…",
        *leave_rows(),
    ]


def test_a_candidate_on_another_protocol_is_no_restart(ground: Ground, restore_candidate):
    real = sessionlib.real_identity()
    fake_candidate(
        json.dumps({**real, "app_version": "99.0.0", "session_protocol": real["session_protocol"] + 1})
    )
    update = connect_as(ground, "incompatible")
    assert update["restart"] == {"offered": False, "why": "incompatible"}, update
    header = band_menu(ground.host.roost, ground.host.saved_id)[0]
    assert header.endswith(" · its roost-session can't talk to this Roost"), header


def test_a_candidate_that_will_not_identify_is_no_restart(ground: Ground, restore_candidate):
    fake_candidate("not an identity")
    update = connect_as(ground, "unreadable")
    assert update["restart"] == {"offered": False, "why": "unreadable"}, update
    header = band_menu(ground.host.roost, ground.host.saved_id)[0]
    assert header.endswith(" · can't read its roost-session"), header


def test_an_override_that_is_gone_is_the_whole_answer(ground: Ground, restore_candidate):
    _CANDIDATE.unlink()
    update = connect_as(ground, "override")
    assert update["restart"] == {"offered": False, "why": "override"}, update
    menu = band_menu(ground.host.roost, ground.host.saved_id)
    assert menu[0].endswith(" · ROOST_SESSION_BIN names nothing this user can run"), menu
    assert "Restart Session…" not in menu, menu


# ---------------------------------------------------------------------------
# 6. Restart Session (plan 076 D4, D7, D8)
# ---------------------------------------------------------------------------


def wait_restarted(ground: Ground, host: HostUnderTest, before: str, timeout: float = 180.0) -> dict:
    """The row once a restart has settled on a session other than
    `before`, claimed for the teardown."""

    def landed() -> dict | None:
        row = host_status_row(host.roost, host.saved_id)
        connect = row.get("connect")
        update = row.get("update")
        if connect is not None:
            ground.claim(connect["session_id"])
        if connect is None or update is None or connect["session_id"] == before:
            return None
        action = update.get("action", {})
        return row if action.get("phase") in ("done", "failed") else None

    return wait_until(landed, timeout, "the restart to land a new session")


def test_restart_runs_the_override_and_lands_up_to_date(ground: Ground, restore_candidate):
    """Restart Session onto its D4 target. `ROOST_SESSION_BIN` is set, so
    the override is the only candidate and the one the relaunch runs: the
    new session's own `exe_path` says which binary that was."""
    before = start_as(ground, "old", app_version="0.0.1")
    ground.host.connect_and_wait()
    update = host_update(ground.host, before)
    assert update["state"] == "staged", update
    assert update["restart"]["target"]["source"] == "override", update

    accepted = ground.host.roost.call(
        "host.restart", {"id": ground.host.saved_id, "confirm": True}
    )
    assert accepted == {"accepted": True}, accepted
    # One action per session (D8): the claim is held from the op on.
    with pytest.raises(RoostError) as busy:
        ground.host.roost.call("host.restart", {"id": ground.host.saved_id, "confirm": True})
    assert busy.value.code == "busy", busy.value

    row = wait_restarted(ground, ground.host, before)
    action = row["update"]["action"]
    real = sessionlib.real_identity()
    shown = sessionlib.describe_build(real)
    assert action == {
        "kind": "restart",
        "phase": "done",
        "message": f"{ground.host.label} restarted on roost-session {shown}",
    }, action
    assert row["update"]["session"]["version"] == real["app_version"], row
    serving = ground.host.env.identify()
    assert serving["session_id"] == row["connect"]["session_id"], serving
    assert serving["exe_path"] == str(_CANDIDATE.resolve()), serving


@contextlib.contextmanager
def relaunched_ui(target: str, **env: str):
    """This lane's UI again, with `env` over its environment; the usual
    one is put back afterwards. `ROOST_SESSION_BIN` is read once by the
    UI process, so a case that needs it unset needs its own UI."""
    ui.quit(target)
    try:
        ui.launch(target, force=True, extra_env=env)
        with Roost(str(ui.socket_path(target)), timeout=scaled_timeout(30.0)) as client:
            yield client
    finally:
        with contextlib.suppress(Exception):
            ui.quit(target)
        ui.launch(target, force=True)


def test_a_newer_session_restarts_onto_its_own_binary(
    ground: Ground, target: str, restore_candidate
):
    """AC4/AC5: with no override, a session newer than both this client
    and the bundled build restarts onto the binary it is running
    (`exe_path`, source `current`) — never down onto the bundled one —
    and stays `session-newer`."""
    before = start_as(ground, "newer", app_version="99.0.0")
    copy = (ground.host.env.root / "build-newer" / "roost-session").resolve()
    # Empty is unset, as `locate_session_binary` reads it (plan 076 D4):
    # the candidates are the bundled sibling and the running binary.
    with relaunched_ui(target, ROOST_SESSION_BIN="") as roost:
        host = dataclasses.replace(ground.host, roost=roost)
        try:
            host.connect_and_wait()
            update = host_update(host, before)
            assert update["state"] == "session-newer", update
            assert update["restart"]["target"]["source"] == "running", update
            assert update["restart"]["target"]["version"] == "99.0.0", update

            roost.call("host.restart", {"id": host.saved_id, "confirm": True})
            row = wait_restarted(ground, host, before)
            assert row["update"]["action"]["phase"] == "done", row
            assert row["update"]["session"]["version"] == "99.0.0", row
            assert row["update"]["state"] == "session-newer", row
            serving = ground.host.env.identify()
            assert serving["exe_path"] == str(copy), serving
        finally:
            with contextlib.suppress(Exception):
                host.disconnect()
            with contextlib.suppress(Exception):
                host.remove()


def test_a_target_gone_before_the_relaunch_starts_nothing(ground: Ground, restore_candidate):
    """AC8: the target is held for the whole attempt. It is deleted
    while the old session is stopping — a tab that ignores SIGHUP holds
    the stop open — and the restart fails naming it, with nothing
    started in its place: no fall back to the launch ladder."""
    before = start_as(ground, "old", app_version="0.0.1")
    ground.host.connect_and_wait()
    host_update(ground.host, before)
    with ground.host.client() as session:
        project = first_project(session)
        hold = session.open_tab(
            project,
            cwd=str(ground.host.env.launch_cwd),
            argv=["/bin/sh", "-c", "trap '' HUP TERM; while :; do sleep 1; done"],
        )

    ground.host.roost.call("host.restart", {"id": ground.host.saved_id, "confirm": True})

    def stopping() -> bool:
        # Once the stop is under way the session refuses every mutation;
        # by then the restart has re-checked its target and moved on.
        try:
            with ground.host.client(timeout=5.0) as session:
                session.call("tab.set_title", {"tab_id": str(hold), "title": "hold"})
        except RoostError as error:
            return error.code == "shutting-down"
        except OSError:
            return False
        return False

    wait_until(stopping, 60.0, "the old session to begin stopping", 0.02)
    _CANDIDATE.unlink()
    # The hold is what orders the unlink ahead of the restart's re-check:
    # the old session is still serving, so its stop has not finished.
    assert ground.host.env.answering() is not None, "the stop finished before the unlink"

    # Mid-restart, with the stream let go: the action is still reported
    # (D7), and a second one is `busy` rather than "not connected" (D8).
    mid = host_status_row(ground.host.roost, ground.host.saved_id)
    assert mid["state"] != "connected", mid
    assert mid["update"]["action"] == {"kind": "restart", "phase": "running"}, mid
    with pytest.raises(RoostError) as busy:
        ground.host.roost.call("host.restart", {"id": ground.host.saved_id, "confirm": True})
    assert busy.value.code == "busy", busy.value

    def settled() -> dict | None:
        row = host_status_row(ground.host.roost, ground.host.saved_id)
        reason = row.get("reason") or ""
        return row if str(_CANDIDATE) in reason else None

    row = wait_until(settled, 120.0, "the restart to fail naming its target")
    assert row["state"] == "disconnected", row
    assert "nothing was started" in row["reason"], row
    assert row.get("connect") is None, row
    action = row["update"]["action"]
    assert action["kind"] == "restart" and action["phase"] == "failed", row
    assert str(_CANDIDATE) in action["message"], action
    status = roostctl_session("status")
    assert status.returncode == NOT_RUNNING_EXIT, (
        "a restart whose target is gone must not start anything else: "
        f"{status.stdout!r}"
    )
