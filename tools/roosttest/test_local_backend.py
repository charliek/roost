"""Plan 063 §D8 (#426): switching which backend the local band runs on.

# What this proves

`local:use_session` moves a whole in-process layout onto a
`roost-session` and ends the shells it was running in;
`local:use_in_process` flips back without copying anything (§D8's
No-Replay). Both write the `local-backend` key, and both are journalled
so a crash resolves to a definite mode with the **source layout intact**
— which is the only guarantee available across a `config.conf` and two
`state.json`s that cannot be written atomically together.

Every case here drives the switch the way a person does: a palette row,
then the confirm card, through `palette.activate` and
`app.dialog_dump`/`app.dialog_answer`. Nothing pokes the state machine
directly.

# Owning a machine-wide sentinel

Same arrangement as `test_host_localhost.py`, and for the same reason:
the switch's destination is *this build's* session socket, one path per
build per user. This module mints a private root and points
`XDG_RUNTIME_DIR` (with its `XDG_*` siblings) at it **at import**, before
`conftest.py`'s session fixture launches the UI, so the UI's own socket
and the sentinel it resolves for `localhost` move together. A developer's
own session is then not merely left alone but unreachable.

`HOME` is deliberately not redirected: the shells are not under test.

# Why its own pytest invocation

The import-time environment poisons every module imported after it in
the same process, and this lane relaunches the UI under a *different*
`local-backend` for most of its cases — which no other lane expects.
`pytestmark = pytest.mark.host_client` keeps it out of whole-directory
runs (and off the Swift target, which answers `unknown-op` to every
`host.*` op).

Condition waits only.
"""

from __future__ import annotations

import atexit
import contextlib
import json
import os
import platform
import shutil
import signal
import stat
import subprocess
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

import pytest

if platform.system() == "Darwin":
    pytest.skip(
        "the session sentinel is redirected through XDG_RUNTIME_DIR, which is "
        "Linux only; the macOS variant is #390",
        allow_module_level=True,
    )

import agent_jail  # noqa: E402
import session as sessionlib  # noqa: E402
import util  # noqa: E402

_ROOT = Path(tempfile.mkdtemp(prefix="roost-lb-", dir="/tmp")).resolve()
_RUN = _ROOT / "run"
# 0700, which `validate_runtime_dir` insists on and a plain `mkdir` under
# umask 0002 does not give.
agent_jail.make_private_runtime_dir(_RUN)
for _sibling in ("data", "state", "cache", "conf"):
    (_ROOT / _sibling).mkdir()

os.environ["XDG_RUNTIME_DIR"] = str(_RUN)
os.environ["XDG_DATA_HOME"] = str(_ROOT / "data")
os.environ["XDG_STATE_HOME"] = str(_ROOT / "state")
os.environ["XDG_CACHE_HOME"] = str(_ROOT / "cache")
# The one shell every replayed tab gets, on both sides of the switch. A
# developer's zsh with Roost's shell integration would rewrite the very
# titles and cwds this lane matches source against destination on.
os.environ["SHELL"] = "/bin/sh"
# The binary a UI-driven spawn must find, so a switch's destination is
# this tree's session and never something on `PATH`.
os.environ["ROOST_SESSION_BIN"] = str(sessionlib.session_binary())

atexit.register(shutil.rmtree, _ROOT, ignore_errors=True)

import ui  # noqa: E402
from client import Roost, RoostError, scaled_timeout  # noqa: E402
from eventstream import ENDED_EVENT, EventStream  # noqa: E402
from session import wait_until  # noqa: E402
from test_host_local_spawn import roostctl_session, running_session_id  # noqa: E402

pytestmark = pytest.mark.host_client

#: `crates/roost-ipc/src/local_route.rs`'s `SWITCH_BUSY`.
BUSY = "busy: a local-backend switch is in progress"
USE_SESSION = "local:use_session"
USE_IN_PROCESS = "local:use_in_process"
JOURNAL = "switch-journal.json"
#: `STATUS_NOT_RUNNING_EXIT` (`crates/roost-cli/src/session.rs`).
NOT_RUNNING_EXIT = 3
#: A launch with no session to reach and nothing to start one with.
#:
#: §D5's launch migration parks until the slot connects, so this is how a
#: case gets a `session`-mode launch over a populated in-process
#: workspace to hold still. It is the same lever `test_a_destination_
#: that_cannot_start_changes_nothing` pulls on the verb.
NO_SESSION = {"ROOST_SESSION_BIN": str(_ROOT / "no-such-session")}


# ---------------------------------------------------------------------------
# The lane's world: one UI, one session, both inside the private root
# ---------------------------------------------------------------------------


@dataclass
class Lane:
    """The UI, its state dir, and the session on the sentinel socket.

    Every case starts from a wiped state dir and a UI launched under a
    backend it names, because the switch is *about* the state on disk:
    a case inheriting the previous one's `state.json`, config key or
    journal would be testing the leftovers.
    """

    target: str
    state_dir: Path
    #: The env for the daemon this lane starts itself, and the handle its
    #: teardown reaps through.
    env: "sessionlib.SessionEnv"
    #: The pid of a daemon this lane started itself, for the one case
    #: that has to stop it answering without stopping it listening.
    pid: int | None = None

    # -- paths ----------------------------------------------------------
    @property
    def config(self) -> Path:
        path = ui.owned_session_config_path()
        assert path is not None, "this lane requires a harness-owned UI"
        return path

    @property
    def journal_path(self) -> Path:
        return self.state_dir / JOURNAL

    def journal(self) -> dict | None:
        try:
            return json.loads(self.journal_path.read_text())
        except FileNotFoundError:
            return None

    def in_process_projects(self) -> list[dict]:
        """The workspace `session` mode hides, read off its own disk.

        Since §D10 the UI socket's `tab.list` under `session` answers
        with the **slot's** projects, so this is the only observable the
        in-process layout has left. It is not a weaker one: the
        workspace writes through on every mutation, and the rows carry
        `name` + `tabs[].title` + `tabs[].user_titled` — everything
        `layout` reduces.
        """
        try:
            return json.loads((self.state_dir / "state.json").read_text())["projects"]
        except FileNotFoundError:
            return []

    # -- the UI ---------------------------------------------------------
    def write_config(self, backend: str, path: Path | None = None) -> None:
        """Rewrite `local-backend` in the config the UI reads, leaving
        every other line (the launcher commands, `agent-hooks = off`)
        alone."""
        path = path or self.config
        lines = [
            line
            for line in path.read_text().splitlines()
            if not line.strip().startswith("local-backend")
        ]
        lines.append(f"local-backend = {backend}")
        path.write_text("\n".join(lines) + "\n")

    def start(
        self,
        backend: str,
        *,
        wipe: bool = True,
        extra_env: dict[str, str] | None = None,
    ) -> Roost:
        """Bring the UI up on `backend`, from a clean slate by default."""
        ui.quit(self.target)
        if wipe:
            self.wipe()
        self.write_config(backend)
        ui.launch(self.target, state_dir=self.state_dir, force=True, extra_env=extra_env)
        client = Roost(ui.socket_path(self.target))
        assert client.identify()["local_backend"] == backend
        return client

    def restart(self, **kwargs) -> Roost:
        """Quit and come back on whatever is already written — the
        crash-recovery cases' relaunch, which must not rewrite the key
        the case is testing the recovery of."""
        ui.quit(self.target)
        ui.launch(self.target, state_dir=self.state_dir, force=True, **kwargs)
        return Roost(ui.socket_path(self.target))

    def wipe(self) -> None:
        """Everything a previous case may have left on disk, including
        the daemon it spawned. The UI must be down first."""
        self.stop_daemon()
        for leftover in ("state.json", JOURNAL):
            (self.state_dir / leftover).unlink(missing_ok=True)
        shutil.rmtree(self.state_dir / ui.DERIVED_SESSION_SUBDIR, ignore_errors=True)

    # -- the session ----------------------------------------------------
    def session(self) -> Roost:
        return self.env.client()

    def start_daemon(self) -> str:
        """Put a session on the sentinel, for a case that wants the
        destination to exist *before* the switch reaches phase 1."""
        self.pid = self.env.start_daemonized().verdict.pid
        return self.env.identify()["session_id"]

    def session_projects(self) -> list[dict]:
        with self.session() as c:
            return c.list()

    def empty_the_session(self) -> None:
        """Leave the daemon running with no projects at all.

        A first-ever `roost-session` seeds `Untitled 1` of its own
        (`hydrate.rs`), which is a *destination* project the switch never
        made — so a case that wants "the destination is exactly the
        source" has to clear it first. Nothing in the session exits on
        empty; only the UI has that rule (§D9).
        """
        with self.session() as c:
            for project in c.list():
                c.delete_project(int(project["id"]))
        assert self.session_projects() == []

    def stop_daemon(self) -> None:
        """Stop whatever answers the sentinel.

        Unconditional, unlike the sibling host lanes' identity check, and
        for a reason this lane has and they do not: the sentinel resolves
        inside a directory minted seconds ago for this run, and the
        fixture asserts nothing answers it before each case. A daemon
        here is therefore always one this lane started — directly, or
        through the switch's own phase-1 spawn, which is a session no
        test holds an id for until it comes up.
        """
        if running_session_id() is None:
            return
        stopped = roostctl_session("stop")
        assert stopped.returncode == 0, (
            f"stopping the session this lane started failed ({stopped.returncode}): "
            f"{stopped.stdout!r} / {stopped.stderr!r}"
        )
        self.env.wait_socket_gone()


@pytest.fixture(scope="module", autouse=True)
def _restore_the_shared_ui(target):
    """Hand the session-scoped UI fixture back a UI it can quit.

    Every case here relaunches, and the last one may leave the UI down or
    on `session` — which `end_session` would then tear down against a
    `local-backend` no other lane in the same job expects. Module-scoped,
    so it runs before the session fixture's own teardown.
    """
    yield
    with contextlib.suppress(Exception):
        ui.quit(target)
    with contextlib.suppress(Exception):
        state_dir = ui.session_state_dir()
        if state_dir is not None:
            shutil.rmtree(state_dir / ui.DERIVED_SESSION_SUBDIR, ignore_errors=True)


@pytest.fixture
def lane(target):
    """One case's UI + session, wiped before and reaped after."""
    assert target == "iced", "this lane drives the Rust UI's switch verbs"
    state_dir = ui.session_state_dir()
    assert state_dir is not None, (
        "the harness did not launch the UI, so its state dir — where the "
        "switch journal lives — is unknowable. Under this lane's redirected "
        "XDG_RUNTIME_DIR there should never have been an instance to reuse"
    )
    env = sessionlib.make_env(
        root=_ROOT, state_dir=state_dir / ui.DERIVED_SESSION_SUBDIR
    )
    made = Lane(target=target, state_dir=state_dir, env=env)
    ui.quit(target)
    status = roostctl_session("status")
    assert status.returncode == NOT_RUNNING_EXIT, (
        "a roost-session already answers this lane's own session socket "
        f"(`roostctl session status` exited {status.returncode}: "
        f"{status.stdout.strip()!r}). That directory was minted for this run, "
        "so it can only be a daemon an earlier case leaked"
    )
    made.wipe()
    try:
        yield made
    finally:
        with contextlib.suppress(Exception):
            ui.quit(target)
        try:
            made.stop_daemon()
        finally:
            env.teardown()
            # The config is shared with every other case, so it goes back
            # to the pin `fixtures/launcher.conf` carries.
            with contextlib.suppress(Exception):
                made.write_config("in-process")


# ---------------------------------------------------------------------------
# Driving the verbs the way a person does
# ---------------------------------------------------------------------------


def command_rows(roost: Roost) -> list[str]:
    roost.palette_open("commands")
    try:
        return [item["id"] for item in roost.palette_state()["items"]]
    finally:
        roost.palette_dismiss()


def activate(roost: Roost, item_id: str) -> None:
    """Press a command-frame row, the way a person reaches these verbs."""
    roost.palette_open("commands")
    try:
        roost.palette_activate(item_id)
    finally:
        roost.palette_dismiss()


def raise_switch(roost: Roost, row: str) -> dict:
    """Press a switch row and return the confirm card it raised."""
    activate(roost, row)
    card = roost.call("app.dialog_dump", {})
    assert card["dialog"] == "confirm_switch", card
    return card


def switch(roost: Roost, row: str, to: str, timeout: float = 120.0) -> None:
    """Press the row, confirm the card, and wait for the switch to
    settle — the mode landed *and* the phase gone, which is the guard
    coming off (§D8's phase 6)."""
    raise_switch(roost, row)
    roost.call("app.dialog_answer", {"action": "confirm"})
    wait_until(
        lambda: settled(roost) == to,
        timeout,
        f"the local backend to settle on {to}",
    )


def settled(roost: Roost) -> str | None:
    """The backend, once nothing is switching. `None` while a phase is
    still in flight — the mode cell flips mid-sequence, so reading it
    alone would report a switch as finished at its commit point."""
    reply = roost.identify()
    if reply.get("local_backend_switch") is not None:
        return None
    return reply["local_backend"]


def layout(projects: list[dict]) -> list[tuple[str, tuple[tuple[str, bool], ...]]]:
    """A layout reduced to what a switch is supposed to preserve: names,
    in order, each with its tabs' titles and title locks."""
    return [
        (
            p["name"],
            tuple((t["title"], bool(t.get("user_titled"))) for t in p["tabs"]),
        )
        for p in projects
    ]


def local_band(roost: Roost) -> dict:
    band = roost.sidebar_local_band()
    assert band is not None, roost.sidebar_dump()
    return band


def source_layout(roost: Roost) -> list[dict]:
    """A two-project in-process layout with **different tab counts** and
    one manually titled tab.

    Uneven on purpose: a replay that mapped tabs to the wrong project, or
    dropped one, or lost a title lock, each fails on its own here — a
    symmetric two-by-one layout would let a transposition pass.
    """
    boot = roost.list()[0]
    roost.rename_project(int(boot["id"]), "alpha")
    roost.open_tab(int(boot["id"]), cwd="/tmp", title="second-tab")
    second = roost.create_project(name="beta", cwd="/tmp")
    tab = roost.open_tab(second, cwd="/tmp")
    roost.set_title(tab, "pinned")
    return roost.list()


# ---------------------------------------------------------------------------
# 1. Forward
# ---------------------------------------------------------------------------


def test_the_forward_switch_moves_the_layout_onto_an_empty_session(lane: Lane):
    """AC3's forward clause, against a session that holds nothing.

    Emptying the daemon first is what makes the count assertion sharp in
    two directions at once: the destination ends up with *exactly* the
    source, so a dropped project fails it — and so does the seed §D12's
    `SwitchDestination` purpose exists to suppress, since a landing that
    seeded would leave one project the source never had.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    want = layout(source_layout(roost))
    assert len(want) == 2, want

    rows = command_rows(roost)
    assert USE_SESSION in rows
    assert USE_IN_PROCESS not in rows, "the backend you are on is not offered"

    card = raise_switch(roost, USE_SESSION)
    assert card["variant"] == "session"
    assert "2 projects and 3 tabs" in card["body"], card["body"]
    roost.call("app.dialog_answer", {"action": "confirm"})
    wait_until(lambda: settled(roost) == "session", 120.0, "the switch to settle")

    assert layout(lane.session_projects()) == want
    assert lane.in_process_projects() == [], "the in-process workspace is emptied"
    assert lane.journal() is None, "the journal is deleted after the final delete"

    # The band the sidebar draws is the slot's, and the tab it selected
    # is the one that was active in the source (§D8 phase 6).
    band = local_band(roost)
    assert band["role"] == "session", band
    slot_rows = roost.sidebar_host(band["saved_id"])
    assert slot_rows is not None, roost.sidebar_dump()
    assert [p["name"] for p in slot_rows["projects"]] == ["alpha", "beta"]
    # §D8 phase 6's *outcome* — the mapped tab is selected and attached —
    # asserted through the attachment itself: the UI holds a live client
    # terminal for exactly the host tab it is showing (`host_focus_tab`
    # detaches every other), and `tab.dump` on a host-qualified key
    # answers off that terminal, so it succeeds for the selected tab and
    # is `not-found` for any other. Two-sided, which "a dump succeeded"
    # alone would not be.
    #
    # NOT the fence's *ordering*. Phase 6 exists because a control reply
    # can precede its broadcast, and against a session on this machine
    # the mirror is never behind long enough for that to show: releasing
    # the guard early leaves this assertion passing (recorded, with the
    # mutation, in `negative-controls.md`). The ordering is pinned by
    # `fence_holds`' own unit table instead.
    #
    # Not read off `identify.active_*`: those report the slot's ids only
    # once §D10's forwarding lands (C7), and answer 0 under `session`
    # until then.
    keys = {
        project["name"]: [tab["key"] for tab in project["tabs"]]
        for project in slot_rows["projects"]
    }
    roost.call("tab.dump", {"tab_id": keys["beta"][0]})
    for stale in keys["alpha"]:
        with pytest.raises(RoostError) as unattached:
            roost.call("tab.dump", {"tab_id": stale})
        assert "no live terminal" in str(unattached.value), unattached.value

    # The verbs swapped over, and the session outlives the switch.
    rows = command_rows(roost)
    assert USE_IN_PROCESS in rows and USE_SESSION not in rows
    assert running_session_id() is not None


def test_a_switch_that_starts_the_session_lands_on_exactly_the_source(lane: Lane):
    """AC3's literal clause, on the path a real user takes.

    Nothing is listening when the switch begins, so **the switch's own
    phase-1 spawn** is what starts the daemon — and that spawn carries
    the no-seed hint (§D8 phase 1), so the session hydrates with nothing
    of its own. The destination is then exactly the source: no
    `Untitled 1` the daemon seeded itself, and no seed-on-connect the
    `SwitchDestination` purpose suppressed.

    The case above proves the *client's* seed is withheld against a
    session that already exists. This one proves the *daemon's* is, and
    it is the one a first-use walk through this feature actually hits.
    """
    roost = lane.start("in-process")
    assert running_session_id() is None, "the switch must be the thing that starts it"
    want = layout(source_layout(roost))

    switch(roost, USE_SESSION, "session")

    assert running_session_id() is not None, "phase 1 started the destination"
    assert layout(lane.session_projects()) == want, (
        "a session the switch started must hold the migrated layout and nothing else"
    )
    assert lane.in_process_projects() == []

    # **The hint is consumed, not merely read.** It is an env var on the
    # daemon's own process, so a session that did not erase it before
    # forking would put `ROOST_SESSION_NO_SEED=1` into the environment of
    # every shell it ever spawns — `ROOST_SESSION_LAUNCH_CWD`'s reason
    # for being consumed-once, and the only place it is observable is
    # inside one of those shells.
    migrated = int(lane.session_projects()[0]["tabs"][0]["id"])
    with lane.session() as c:
        c.send(migrated, 'echo "NO_SEED=[${ROOST_SESSION_NO_SEED-unset}]"\n')
        wait_until(
            lambda: "NO_SEED=[" in c.dump_text(migrated) or None,
            scaled_timeout(30.0),
            "the shell to answer what it inherited",
        )
        inherited = c.dump_text(migrated)
    assert "NO_SEED=[unset]" in inherited, inherited


def test_a_tab_that_cannot_be_replayed_keeps_its_source_project(lane: Lane):
    """§D8 phase 3/5: phase 5 deletes exactly what phase 3 landed.

    A tab whose directory the daemon cannot enter is the one `tab.open`
    failure a test can manufacture from outside, and it is a real one:
    `replay_cwd`'s fallback only fires for a directory that is *gone*, so
    a `0000` one passes the check and the spawn's `chdir` fails on the
    far side. The project it belongs to must then survive on **both**
    sides — the user has it twice, which they can fix, rather than short
    a tab, which they cannot.

    The two projects that did land whole are still deleted, which is what
    keeps this from being a test of "the switch gave up".
    """
    locked = _ROOT / f"locked-{uuid.uuid4().hex[:8]}"
    locked.mkdir()
    try:
        roost = lane.start("in-process")
        want = layout(source_layout(roost))
        stuck = roost.create_project(name="stuck", cwd=str(locked))
        roost.open_tab(stuck, cwd=str(locked))
        # After the tab exists: the UI could not have opened it either.
        locked.chmod(0)

        switch(roost, USE_SESSION, "session")

        # The destination holds the two that landed whole, and nothing
        # of the third: the engine closes a tab whose shell will not
        # start, and closing a project's last tab deletes the project.
        assert layout(lane.session_projects()) == want
        # **And its source is kept**, while the two that landed whole are
        # gone. That asymmetry is the whole fix: phase 5 deletes exactly
        # what phase 3 proved present, so a project the destination does
        # not hold stays where the user left it.
        kept = lane.in_process_projects()
        assert [p["name"] for p in kept] == ["stuck"]
        assert len(kept[0]["tabs"]) == 1, "with its tab row kept"
    finally:
        locked.chmod(0o755)


def test_a_non_empty_destination_keeps_what_it_had_and_gains_the_source(lane: Lane):
    """§D8's "well-defined per D12": the migration is appended, and
    nothing on the destination is disturbed or duplicated."""
    roost = lane.start("in-process")
    lane.start_daemon()
    # **A daemon started by any other path still seeds.** This one came
    # up through the ordinary launcher, so §D4's rule applies to it in
    # full — the withheld first project is the switch's spawn and
    # nothing else. Asserted here rather than assumed, because it is
    # the half of §D8 phase 1's change that must *not* have happened.
    assert layout(lane.session_projects()) == [("Untitled 1", (("home", False),))], (
        lane.session_projects()
    )
    with lane.session() as c:
        held = c.create_project(name="already-here", cwd="/tmp")
        c.open_tab(held, cwd="/tmp")
    before = layout(lane.session_projects())
    assert len(before) == 2, before

    want = layout(source_layout(roost))
    switch(roost, USE_SESSION, "session")

    after = layout(lane.session_projects())
    assert after == before + want, after
    assert lane.in_process_projects() == []


# ---------------------------------------------------------------------------
# 2. Reverse — No-Replay
# ---------------------------------------------------------------------------


def test_the_reverse_switch_flips_the_key_and_copies_nothing_back(lane: Lane):
    """AC3's reverse clause (§D8's No-Replay, owner-pinned).

    The three things that must all be true at once: the session keeps
    every project, the in-process band comes back with **only** its own
    fresh seed, and the slot is demoted rather than disconnected.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    want = layout(source_layout(roost))
    switch(roost, USE_SESSION, "session")
    assert layout(lane.session_projects()) == want

    card = raise_switch(roost, USE_IN_PROCESS)
    assert card["variant"] == "in-process"
    assert "nothing is copied back" in card["body"], card["body"]
    roost.call("app.dialog_answer", {"action": "confirm"})
    wait_until(lambda: settled(roost) == "in-process", 120.0, "the flip back")

    assert layout(lane.session_projects()) == want, "the session is untouched"
    seeded = roost.list()
    assert len(seeded) == 1 and len(seeded[0]["tabs"]) == 1, seeded
    assert seeded[0]["name"] == "Untitled 1", seeded
    assert seeded[0]["cwd"] == os.path.expanduser("~"), seeded

    # Demoted, not disconnected, stopped or removed (§D8's "Reverse").
    band = local_band(roost)
    assert band["role"] == "local", band
    hosts = roost.host_status()["hosts"]
    slot = next(h for h in hosts if h["target"] == "localhost")
    assert slot["state"] == "connected", slot
    assert running_session_id() is not None
    sections = roost.sidebar_sections()
    assert [s["role"] for s in sections] == ["local", "host"], sections


def test_reverse_over_a_retained_layout_hydrates_it_rather_than_counting_it(lane: Lane):
    """§D8's reverse, over the state a session-mode launch actually
    leaves behind.

    `Workspace::open` loads `state.json` — project rows *and* a retained
    tab layout — and session-mode bootstrap deliberately does not
    hydrate it (§D5). So the in-process workspace here has projects with
    **no live tabs**. A reverse that saw a non-empty project list and
    declared itself finished would hand back a band of empty rows; the
    next forward switch would then snapshot those empty tab lists and
    delete the originals, and the retained layout would be gone through
    two gestures that both reported success.

    Built the way a user reaches it: a real in-process layout, flushed
    by a real quit, then a relaunch with the key hand-edited to
    `session` — and **no session to reach**, so §D5's launch migration
    parks instead of moving the layout away. That pairing is the whole
    of how this state survives a launch now: a slot that connects takes
    the layout to the session (`test_a_hand_edited_key_moves_the_layout_
    at_launch`), and a slot that never does leaves it retained, right
    here, for the reverse to bring back.
    """
    roost = lane.start("in-process")
    want = layout(source_layout(roost))
    ui.quit(lane.target)

    # The layout really is on disk, with its tabs.
    state = json.loads((lane.state_dir / "state.json").read_text())
    saved = {p["name"]: len(p["tabs"]) for p in state["projects"]}
    assert saved == {"alpha": 2, "beta": 1}, state

    lane.write_config("session")
    roost = lane.restart(extra_env=NO_SESSION)
    assert roost.identify()["local_backend"] == "session"
    # **The state the fix is about**, as far as §D10 leaves it visible
    # from outside: this socket's `tab.list` now answers with the slot's
    # list, so the retained layout is read off the disk it is retained
    # on. That it is still whole there is the precondition — session
    # bootstrap neither hydrated it nor persisted over it — and the
    # post-switch assertions below are what prove the rows in memory
    # carried no tabs.
    retained = lane.in_process_projects()
    assert layout(retained) == want, retained
    assert local_band(roost)["role"] == "session"

    switch(roost, USE_IN_PROCESS, "in-process")

    assert layout(roost.list()) == want, (
        "the retained layout comes back with its tabs, its order and its title lock"
    )
    # Rows are not shells. A tab that reopened has a live PTY behind it,
    # and `tab.resize` is the op that reaches one.
    for project in roost.list():
        for tab in project["tabs"]:
            roost.resize(int(tab["id"]), 100, 30)


def test_two_round_trips_move_the_work_once_and_the_seed_once(lane: Lane):
    """The anti-accumulation claim (§D8's rejected "Copy" alternative).

    Two full round trips, with the destination counted after each. The
    migrated layout moves exactly once; what grows is the *fresh seed*
    the reverse leaves behind, which the second forward carries over —
    one project, not a second copy of the layout.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    want = layout(source_layout(roost))

    switch(roost, USE_SESSION, "session")
    assert layout(lane.session_projects()) == want

    switch(roost, USE_IN_PROCESS, "in-process")
    assert layout(lane.session_projects()) == want
    assert len(roost.list()) == 1

    seed = layout(roost.list())
    switch(roost, USE_SESSION, "session")
    assert layout(lane.session_projects()) == want + seed, "the seed moved, once"

    switch(roost, USE_IN_PROCESS, "in-process")
    assert layout(lane.session_projects()) == want + seed
    assert len(roost.list()) == 1, "and in-process holds only its own fresh seed"


# ---------------------------------------------------------------------------
# 3. Failures, cancel, and quiescence
# ---------------------------------------------------------------------------


def test_a_destination_that_cannot_start_changes_nothing(lane: Lane):
    """§D8 phase 1's failure: the spawn ladder has nothing to run.

    Nothing is saved, nothing is written, nothing is moved — including
    the slot the switch added on its way in, which a reported failure
    must not leave behind.
    """
    roost = lane.start("in-process", extra_env=NO_SESSION)
    want = layout(source_layout(roost))
    before = roost.host_status()["hosts"]

    raise_switch(roost, USE_SESSION)
    roost.call("app.dialog_answer", {"action": "confirm"})
    wait_until(
        lambda: settled(roost) == "in-process" and USE_SESSION in command_rows(roost),
        180.0,
        "the switch to give up",
    )

    assert layout(roost.list()) == want, "the source layout is untouched"
    assert roost.identify()["local_backend"] == "in-process"
    assert lane.journal() is None
    after = roost.host_status()["hosts"]
    assert [h["id"] for h in after] == [h["id"] for h in before], (
        "a switch that failed while saving the slot must un-save it"
    )


def test_a_key_write_that_fails_rolls_the_destination_back(lane: Lane):
    """§D8 phase 4's failure — the one that has a rollback behind it.

    The replay has already committed projects on the session by the time
    `set_key` is tried, so this is the only path that must *undo* work on
    the destination. Injected for real: the config lives in a directory
    the UI cannot write, so the atomic tmp+rename `set_key` does has
    nowhere to land.
    """
    conf_dir = _ROOT / "conf"
    conf = conf_dir / "launcher.conf"
    shutil.copyfile(ui.SEED_CONFIG, conf)
    lane.write_config("in-process", path=conf)

    roost = lane.start("in-process")
    ui.quit(lane.target)
    conf_dir.chmod(stat.S_IRUSR | stat.S_IXUSR)
    try:
        ui.launch(
            lane.target,
            state_dir=lane.state_dir,
            force=True,
            extra_env={"ROOST_CONFIG": str(conf)},
        )
        roost = Roost(ui.socket_path(lane.target))
        want = layout(source_layout(roost))
        assert running_session_id() is None

        raise_switch(roost, USE_SESSION)
        roost.call("app.dialog_answer", {"action": "confirm"})
        wait_until(
            lambda: settled(roost) == "in-process"
            and USE_SESSION in command_rows(roost),
            180.0,
            "the switch to roll back",
        )

        assert layout(roost.list()) == want, "the source layout is untouched"
        assert roost.identify()["local_backend"] == "in-process"
        assert lane.journal() is None

        # The daemon phase 1 started is **left running and empty**, and
        # empty is a legitimate state rather than a wedged one: it
        # answers, and the next *ordinary* connect seeds it (§D6's
        # "empty at connect is seeded, not forgotten"). Asserted, not
        # assumed — this is the one path that leaves a session nobody
        # asked for behind, and "it is fine, it heals" has to be shown.
        assert running_session_id() is not None, "the rollback must not stop a session"
        assert lane.session_projects() == [], (
            "the projects the replay had already made must be deleted again"
        )
        activate(roost, "host:connect_seed")
        wait_until(
            lambda: len(lane.session_projects()) == 1 or None,
            120.0,
            "an ordinary connect to seed the session the rollback emptied",
        )
    finally:
        conf_dir.chmod(stat.S_IRWXU)


def test_cancelling_the_confirm_leaves_the_verb_and_the_layout_alone(lane: Lane):
    """The card is the only place either verb pauses, so dismissing it
    is the whole of "not yet"."""
    roost = lane.start("in-process")
    want = layout(source_layout(roost))
    raise_switch(roost, USE_SESSION)
    roost.call("app.dialog_answer", {"action": "cancel"})

    assert roost.call("app.dialog_dump", {}).get("dialog") is None
    assert roost.identify()["local_backend"] == "in-process"
    assert roost.identify().get("local_backend_switch") is None
    assert layout(roost.list()) == want
    assert lane.journal() is None
    assert running_session_id() is None, "cancelling started no daemon"
    assert USE_SESSION in command_rows(roost), "and the verb is still offered"


def test_a_switch_in_flight_hides_its_verb_and_refuses_local_mutations(lane: Lane):
    """§D8a's quiescence, observed in a phase held open on purpose.

    The hold is real rather than simulated: `ROOST_SESSION_BIN` points at
    a script that sleeps, so the destination connect §D8 phase 1 waits on
    never lands and the switch sits in `preparing` for as long as the
    assertions need. That is the only phase this lane can pin from
    outside — the others are a round trip wide — and it is the one every
    clause of §D8a is written against.
    """
    stub = _ROOT / "slow-session"
    stub.write_text("#!/bin/sh\nexec sleep 120\n")
    stub.chmod(0o755)
    roost = lane.start("in-process", extra_env={"ROOST_SESSION_BIN": str(stub)})
    want = layout(source_layout(roost))

    raise_switch(roost, USE_SESSION)
    roost.call("app.dialog_answer", {"action": "confirm"})
    try:
        wait_until(
            lambda: roost.identify().get("local_backend_switch") == "preparing",
            60.0,
            "the switch to reach its first phase",
        )
        # Neither verb is offered, in either direction.
        rows = command_rows(roost)
        assert USE_SESSION not in rows and USE_IN_PROCESS not in rows, rows
        # A second activation is refused by name rather than starting a
        # second switch.
        with pytest.raises(RoostError) as raised:
            roost.palette_open("commands")
            try:
                roost.palette_activate(USE_SESSION)
            finally:
                roost.palette_dismiss()
        assert "not-found" in raised.value.code or BUSY in str(raised.value), raised.value

        # Every local-backend mutation answers with the one stable
        # string, from the surface `roostctl` reaches — the four command
        # rows **and** the creation picker, which reaches the same
        # dispatches through a different frame (including the slot, the
        # very workspace a switch is mid-way through moving).
        for row in (
            "new_tab",
            "new_project",
            "close_tab",
            "close_project",
            "host:new_project_on",
        ):
            roost.palette_open("commands")
            try:
                with pytest.raises(RoostError) as refused:
                    roost.palette_activate(row)
                assert BUSY in str(refused.value), (row, refused.value)
            finally:
                roost.palette_dismiss()

        # And the mode has not moved: `preparing` is before the commit
        # point.
        assert roost.identify()["local_backend"] == "in-process"
        assert layout(roost.list()) == want
    finally:
        # Abandon the switch with the process rather than waiting out the
        # ladder; the stub is `sleep`, so it is bounded either way.
        ui.quit(lane.target)
        subprocess.run(["pkill", "-f", str(stub)], check=False)


def test_a_switch_ends_the_in_process_event_stream_and_refuses_a_new_one(lane: Lane):
    """Plan 066 §3.1: a stream of the in-process workspace ends with the
    switch that is about to hide it — exactly one `stream.ended`, then
    the close — and while the switch is in flight a fresh subscribe is
    refused rather than handed a stream nothing would end.

    Held in `preparing` the way the quiescence case above holds it.
    """
    stub = _ROOT / "slow-session-stream"
    stub.write_text("#!/bin/sh\nexec sleep 120\n")
    stub.chmod(0o755)
    roost = lane.start("in-process", extra_env={"ROOST_SESSION_BIN": str(stub)})
    stream = EventStream(ui.socket_path(lane.target))
    try:
        fence = stream.subscribe()
        assert stream.session_id == roost.identify()["instance_id"]

        raise_switch(roost, USE_SESSION)
        roost.call("app.dialog_answer", {"action": "confirm"})
        frames = stream.recv_to_close(timeout=60.0)
        assert [f for f in frames if f.get("event") == ENDED_EVENT] == frames[-1:], frames
        assert frames[-1] == {"event": ENDED_EVENT, "data": {"reason": "backend-switch"}}, frames
        stream.expect_contiguous(frames[:-1], fence)

        assert roost.identify().get("local_backend_switch") == "preparing"
        with EventStream(ui.socket_path(lane.target)) as late:
            with pytest.raises(RoostError) as refused:
                late.subscribe()
        assert refused.value.code == "host-unavailable", refused.value
        assert refused.value.message == BUSY, refused.value
    finally:
        stream.close()
        ui.quit(lane.target)
        subprocess.run(["pkill", "-f", str(stub)], check=False)


def test_under_session_mode_the_ui_socket_points_a_subscriber_at_the_session(lane: Lane):
    """Plan 063 §D10's `Unsupported`, as plan 066 left it: the in-process
    workspace is the hidden one, so the UI socket serves no stream of it
    and says where the stream is instead."""
    roost = lane.start("session", extra_env=NO_SESSION)
    assert "events.subscribe" not in roost.identify()["ops"]
    with EventStream(ui.socket_path(lane.target)) as stream:
        with pytest.raises(RoostError) as refused:
            stream.subscribe()
    assert refused.value.code == "not-implemented", refused.value
    assert refused.value.message.endswith("dial identify.local_session_socket"), refused.value


# ---------------------------------------------------------------------------
# 4. The journal, at every phase
# ---------------------------------------------------------------------------


def write_journal(lane: Lane, **fields) -> None:
    lane.journal_path.write_text(json.dumps(fields))


def snapshot_of(*tab_counts: int) -> list[dict]:
    """A `source_snapshot`, one entry per destination project.

    Every journal a real replay writes carries one — it is written at
    phase 2, before the first `project.create` — and the rollback needs
    it: `created_dest_ids[i]` is the copy of `source_snapshot[i]`, and
    the tab count is how the rollback tells its own abandoned copy from
    a project somebody else has since worked in. A journal with no
    snapshot vouches for nothing and so deletes nothing.
    """
    return [
        {
            "name": f"p{index}",
            "cwd": "/tmp",
            "tabs": [
                {"cwd": "/tmp", "title": "", "user_titled": False} for _ in range(count)
            ],
        }
        for index, count in enumerate(tab_counts)
    ]


@pytest.mark.parametrize("phase", ["preparing", "replaying"])
def test_a_forward_journal_before_the_commit_point_rolls_back(lane: Lane, phase: str):
    """§D8b's rollback arm, at both phases on that side of the line.

    The key on disk is deliberately left saying `session` — the crash
    this recovers from can land *after* `set_key` and before the journal
    moves — so the assertion that the launch comes up `in-process` is an
    assertion that the **phase** decided it, not the file.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    want = layout(source_layout(roost))

    # A partial destination copy, exactly as a crashed replay leaves one.
    with lane.session() as c:
        partial = [c.create_project(name=f"half-{n}", cwd="/tmp") for n in range(2)]
        for project in partial:
            c.open_tab(project, cwd="/tmp")
    ui.quit(lane.target)
    lane.write_config("session")
    write_journal(
        lane,
        from_mode="in-process",
        to_mode="session",
        phase=phase,
        source_snapshot=snapshot_of(1, 1),
        created_dest_ids=partial,
    )

    roost = lane.restart()
    assert roost.identify()["local_backend"] == "in-process", (
        "the phase says the destination is partial, so the source wins"
    )
    assert layout(roost.list()) == want, "the source layout is intact"
    assert lane.session_projects() == [], "the partial copy is deleted"
    assert lane.journal() is None
    # And the key on disk was corrected, not merely ignored for one run.
    assert "local-backend = in-process" in lane.config.read_text()


def test_a_rollback_leaves_a_copy_somebody_else_has_worked_in(lane: Lane):
    """§D8b's rollback may not delete work that is not its to delete.

    The ids in a journal were minted by a run that is gone, and a
    `roost-session` serves every client at once — that is the whole
    point of it. Between the crash and this launch somebody opened a tab
    in one of the abandoned copies and started working there, and
    `project.delete` cascades.

    Two projects, deliberately **not** symmetric: one holds exactly what
    the replay left it and one holds a tab more. A rollback that deleted
    on the strength of the recorded id takes the second with it; one
    that declined everything leaves the first behind. Only telling them
    apart passes.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    want = layout(source_layout(roost))

    with lane.session() as c:
        abandoned = c.create_project(name="half-0", cwd="/tmp")
        c.open_tab(abandoned, cwd="/tmp")
        worked_in = c.create_project(name="half-1", cwd="/tmp")
        c.open_tab(worked_in, cwd="/tmp")
        # Another client, in the copy, after the crash.
        c.open_tab(worked_in, cwd="/tmp")
    ui.quit(lane.target)

    lane.write_config("session")
    write_journal(
        lane,
        from_mode="in-process",
        to_mode="session",
        phase="replaying",
        source_snapshot=snapshot_of(1, 1),
        created_dest_ids=[abandoned, worked_in],
    )

    roost = lane.restart(extra_env=NO_SESSION)
    assert roost.identify()["local_backend"] == "in-process"
    assert layout(roost.list()) == want, "the source layout is intact"
    survivors = {p["name"]: len(p["tabs"]) for p in lane.session_projects()}
    assert survivors == {"half-1": 2}, (
        "the untouched copy goes and the one somebody is working in stays, whole",
        survivors,
    )
    assert lane.journal() is None, (
        "a project this client will never delete is disowned, not left pending — "
        "a journal that re-declines it every launch is a record that never clears"
    )


@pytest.mark.parametrize("phase", ["committing", "cleaning-up"])
def test_a_forward_journal_past_the_commit_point_finishes(lane: Lane, phase: str):
    """§D8b's finish arm, at both phases on that side of the line.

    The mirror image of the case above: the same journal one phase later,
    with the key on disk deliberately saying `in-process`. The launch
    must come up on `session` and finish emptying the source.

    What is left over afterwards is then §D5's business, and the two
    mechanisms make this assertion sharper than either alone. The
    recovery deletes the journal's own list and no wider, so `kept`
    survives it; the launch migration then finds a populated in-process
    workspace and moves *that* — exactly one project — onto the session.
    A recovery that swept the workspace would leave the session without
    `kept`; one that deleted nothing would carry `alpha` and `beta` over
    a second time.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    sources = [int(p["id"]) for p in source_layout(roost)]
    # A third project the running switch would have *kept* — its replay
    # did not land whole — so it is absent from `deletable_sources` and
    # the recovery must leave it exactly where it is.
    kept = roost.create_project(name="kept", cwd="/tmp")
    roost.open_tab(kept, cwd="/tmp")
    kept_layout = layout([p for p in roost.list() if int(p["id"]) == kept])

    # The destination the replay finished writing.
    with lane.session() as c:
        migrated = [c.create_project(name=n, cwd="/tmp") for n in ("alpha", "beta")]
        for project in migrated:
            c.open_tab(project, cwd="/tmp")
    want = layout(lane.session_projects())
    ui.quit(lane.target)
    lane.write_config("in-process")
    write_journal(
        lane,
        from_mode="in-process",
        to_mode="session",
        phase=phase,
        source_snapshot=[],
        created_dest_ids=migrated,
        deletable_sources=[str(id) for id in sources],
    )

    roost = lane.restart()
    assert roost.identify()["local_backend"] == "session"
    wait_until(
        lambda: settled(roost) == "session" and lane.in_process_projects() == [],
        180.0,
        "the launch migration to move what the recovery left behind",
    )
    assert layout(lane.session_projects()) == want + kept_layout, (
        "the source deletion covered the journal's own list, and no wider"
    )
    assert lane.journal() is None
    assert "local-backend = session" in lane.config.read_text()


def test_a_reverse_journal_resolves_symmetrically(lane: Lane):
    """§D8b's "reverse → symmetric", both sides of the same line.

    Reverse copies nothing, so the only thing either arm may do is settle
    the mode — and the finish arm in particular must **not** empty the
    in-process workspace it is coming back to.

    The rollback arm lands on `session` over a populated in-process
    workspace, which is §D5's launch migration by definition. It is kept
    out of the way with `NO_SESSION` rather than worked around: the
    migration waits for a slot to connect, and with nothing listening and
    no binary to start one it never arms, so what this case is about —
    which mode the phase settles on — is all that happens.
    """
    roost = lane.start("in-process")
    want = layout(source_layout(roost))
    ui.quit(lane.target)

    # Before the commit point: back to `session`.
    lane.write_config("in-process")
    write_journal(
        lane,
        from_mode="session",
        to_mode="in-process",
        phase="preparing",
        source_snapshot=[],
        created_dest_ids=[],
    )
    roost = lane.restart(extra_env=NO_SESSION)
    assert roost.identify()["local_backend"] == "session"
    assert lane.journal() is None
    ui.quit(lane.target)

    # Past it: `in-process`, with the layout still there.
    lane.write_config("session")
    write_journal(
        lane,
        from_mode="session",
        to_mode="in-process",
        phase="committing",
        source_snapshot=[],
        created_dest_ids=[],
    )
    roost = lane.restart()
    assert roost.identify()["local_backend"] == "in-process"
    assert layout(roost.list()) == want, (
        "No-Replay has no source teardown; finishing it must not delete one"
    )
    assert lane.journal() is None


def test_a_recovery_that_cannot_restore_the_key_keeps_its_journal(lane: Lane):
    """§D8b: the journal describes the key too.

    The rollback picks the source mode and *writes* it — and when that
    write fails, this launch still runs on the value in memory, so
    nothing looks wrong. The next launch reads the file. A journal
    cleared over a key that still names the other mode is a rollback
    with no record of itself, and the launch after comes up on the very
    mode this one decided against.

    Injected for real: the config lives in a directory the UI cannot
    write, so `set_key`'s atomic tmp+rename has nowhere to land.
    """
    conf_dir = _ROOT / f"ro-{uuid.uuid4().hex[:8]}"
    conf_dir.mkdir()
    conf = conf_dir / "launcher.conf"
    shutil.copyfile(ui.SEED_CONFIG, conf)

    roost = lane.start("in-process")
    want = layout(source_layout(roost))
    ui.quit(lane.target)

    # A forward switch that crashed before the commit point, over a key
    # the crash had already moved.
    lane.write_config("session", path=conf)
    write_journal(
        lane,
        from_mode="in-process",
        to_mode="session",
        phase="replaying",
        source_snapshot=[],
        created_dest_ids=[],
    )
    conf_dir.chmod(stat.S_IRUSR | stat.S_IXUSR)
    try:
        roost = lane.restart(extra_env={"ROOST_CONFIG": str(conf)})
        assert roost.identify()["local_backend"] == "in-process", (
            "this launch still rolls back — the mode is a value, not the file"
        )
        assert layout(roost.list()) == want
        assert lane.journal() is not None, (
            "but the key on disk still says session, so the record of that has to stay"
        )
        assert "local-backend = session" in conf.read_text()
    finally:
        conf_dir.chmod(stat.S_IRWXU)


def test_a_bootstrap_rollback_cannot_hang_the_launch(lane: Lane):
    """§D8b's rollback runs **before the window exists**.

    A daemon that is listening but not servicing — SIGSTOPped here,
    wedged or mid-swap in the wild — accepts the connect and never
    answers the op. Unbounded, that is a Roost that never reaches
    hydration and never draws anything at all: not a slow start, no
    start. The switch's own 120s phase backstop lives on the running
    app's driver and does not reach this path.

    So the launch must come up regardless, and the journal must stay for
    a launch that can finish the job.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    with lane.session() as c:
        orphan = c.create_project(name="orphan", cwd="/tmp")
        c.open_tab(orphan, cwd="/tmp")
    want = layout(source_layout(roost))
    ui.quit(lane.target)

    lane.write_config("session")
    write_journal(
        lane,
        from_mode="in-process",
        to_mode="session",
        phase="replaying",
        source_snapshot=snapshot_of(1),
        created_dest_ids=[orphan],
    )
    assert lane.pid is not None
    os.kill(lane.pid, signal.SIGSTOP)
    try:
        started = time.monotonic()
        roost = lane.restart()
        elapsed = time.monotonic() - started
    finally:
        os.kill(lane.pid, signal.SIGCONT)

    assert roost.identify()["local_backend"] == "in-process", "the rollback still decided"
    assert layout(roost.list()) == want, "and the source is untouched"
    assert elapsed < scaled_timeout(90.0), (
        f"the launch waited {elapsed:.0f}s on a session that was never going to answer"
    )
    assert lane.journal() is not None, (
        "the copy it could not delete stays described, for a launch that can"
    )
    # The daemon is fine — it was stopped, not broken. Its *projects*
    # are deliberately not asserted: the rollback's `project.delete` was
    # already written to the socket when the wait expired, so it lands
    # whenever the daemon runs again. Bounding the wait bounds the
    # launch, which is what this is about; it does not (and need not)
    # recall the request.
    assert running_session_id() is not None


def test_a_journal_naming_no_switch_is_left_alone(lane: Lane):
    """A hand-edited or truncated file is not a switch, and the launch
    is an ordinary one — the ladder decides the mode and the file stays
    where somebody can look at it."""
    roost = lane.start("in-process")
    want = layout(source_layout(roost))
    ui.quit(lane.target)
    write_journal(
        lane,
        from_mode="session",
        to_mode="session",
        phase="committing",
        created_dest_ids=[],
    )

    roost = lane.restart()
    assert roost.identify()["local_backend"] == "in-process"
    assert layout(roost.list()) == want
    assert lane.journal() is not None, "an unresolvable journal is not deleted"


# ---------------------------------------------------------------------------
# 5. AC4: the session outlives the UI, and a relaunch reattaches
# ---------------------------------------------------------------------------


def test_a_relaunch_under_session_reattaches_the_same_tabs(lane: Lane):
    """AC4, on the far side of a switch.

    The token is printed *before* the quit and read back *after* it, out
    of the same tab's scrollback: a relaunch that spawned a fresh shell
    would answer with the same tab ids only by accident, and could not
    answer with the token at all.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    source_layout(roost)
    switch(roost, USE_SESSION, "session")

    before = sorted(int(t["id"]) for p in lane.session_projects() for t in p["tabs"])
    token = f"ROOST-SWITCH-{int(time.time() * 1000)}"
    with lane.session() as c:
        c.send(before[0], f"echo {token}\n")
        wait_until(
            lambda: token in c.dump_text(before[0]),
            scaled_timeout(30.0),
            "the token to reach the session's tab",
        )

    ui.quit(lane.target)
    roost = lane.restart()
    assert roost.identify()["local_backend"] == "session"
    after = sorted(int(t["id"]) for p in lane.session_projects() for t in p["tabs"])
    assert after == before, "the same tabs, not a fresh spawn"
    with lane.session() as c:
        dumped = c.dump(before[0], scrollback=200)
        history = "\n".join(dumped.get("scrollback_text", []) + dumped["rows_text"])
    assert token in history, (
        "the shell that printed it is still the shell behind the tab"
    )


# ---------------------------------------------------------------------------
# 6. §D10: what `roostctl` and a hook reach on the UI socket under `session`
# ---------------------------------------------------------------------------
#
# Every case here leans on one fact: under `session` the UI's own
# in-process workspace holds **nothing**. So an answer that names a
# project, moves a shell, or refuses with the session's verdict is an
# answer the local path could not have produced — and the one that could
# (an empty list, a `not-found`) is asserted against by name.


def session_ui(lane: Lane, **launch) -> Roost:
    """A UI up on `session`, its slot connected and its tab selected.

    The selection wait is not politeness: `identify.active_*` reporting
    the slot's pair is §D1's half of §D10, and it is what makes
    `roostctl` with no `--tab` address anything at all.
    """
    roost = lane.start("session", **launch)
    wait_until(
        lambda: bool(lane.session_projects()),
        120.0,
        "the slot to come up holding a project",
    )
    wait_until(
        lambda: roost.identify()["active_tab_id"] != 0,
        scaled_timeout(60.0),
        "the UI to select and attach the slot's tab",
    )
    return roost


def roostctl(*args: str, timeout: float = 60.0) -> subprocess.CompletedProcess:
    """`roostctl` as a user with no Roost environment runs it.

    `ROOST_SOCKET` and `ROOST_TAB_ID` are removed rather than left
    alone, which is the whole claim of AC8's "no `--tab`, profile
    route": the target is resolved from the bundle profile and the tab
    from `identify`.
    """
    env = {
        key: value
        for key, value in os.environ.items()
        if key not in ("ROOST_SOCKET", "ROOST_TAB_ID")
    }
    return subprocess.run(
        [util.roostctl_path(), "--target", "iced", *args],
        capture_output=True,
        text=True,
        timeout=scaled_timeout(timeout),
        env=env,
    )


def token() -> str:
    return f"ROOST-D10-{uuid.uuid4().hex[:8]}"


def session_tab_ids(lane: Lane) -> list[int]:
    return [int(t["id"]) for p in lane.session_projects() for t in p["tabs"]]


def test_bare_ids_on_the_ui_socket_act_on_the_session(lane: Lane):
    """AC8's first clause, on the ops a hook and a script actually send.

    The project created here exists only on the slot, so every later
    assertion is about *which workspace answered*. The last one reads the
    UI's own `state.json` after the quit: that is the workspace `session`
    mode hides, and it has to be empty — a `project.create` answered
    locally would have landed there, and a `tab.open` with
    `project_id: 0` would have created a project there to land in.
    """
    roost = session_ui(lane)
    project = roost.create_project(name="forwarded", cwd="/tmp")
    tab = roost.open_tab(project, cwd="/tmp", title="forwarded-tab")

    # `project.ensure` finds the slot's project and creates on the slot.
    found = roost.ensure_project("forwarded")
    assert (found["created"], int(found["project"]["id"])) == (False, project), found
    assert roost.ensure_project("ensured", cwd="/tmp")["created"] is True

    on_the_session = lane.session_projects()
    assert "forwarded" in [p["name"] for p in on_the_session]
    assert "ensured" in [p["name"] for p in on_the_session]
    assert [p["name"] for p in roost.list()] == [p["name"] for p in on_the_session], (
        "the UI socket's tab.list is the session's list"
    )

    # `tab.write` reaches the shell the *session* is running.
    printed = token()
    roost.send(tab, f"echo {printed}\n")
    with lane.session() as c:
        wait_until(
            lambda: printed in c.dump_text(tab),
            scaled_timeout(30.0),
            "the forwarded write to reach the session's shell",
        )

    # `tab.set_state` lands on the session's row, not on a local one.
    roost.set_state(tab, "needs_input")
    with lane.session() as c:
        assert c.agent_lifecycle(tab) == "waiting", c.tab(tab)
        assert c.ownership(tab)["source"] == "manual"

    # The fence is stripped at this boundary even though the session's
    # own answer carries one: a UI socket cannot serve the stream it
    # fences (§D10).
    assert "revision" not in roost.call("tab.list", {})
    with lane.session() as c:
        assert "revision" in c.call("tab.list", {}), (
            "the session still fences its own answer, which is what "
            "makes the line above a strip rather than a coincidence"
        )

    ui.quit(lane.target)
    persisted = json.loads((lane.state_dir / "state.json").read_text())
    assert persisted["projects"] == [], (
        f"something landed in the workspace this mode hides: {persisted['projects']}"
    )


def test_roostctl_with_no_tab_flag_drives_the_session(lane: Lane):
    """AC8's `roostctl tab list/send/set-state` clause.

    No `--tab`, no `ROOST_SOCKET`: the socket comes from the bundle
    profile and the tab from `identify.active_tab_id`, which under
    `session` is the slot's pair. Before §D10 that field answered `0`
    and every one of these exited non-zero with "no active tab".
    """
    roost = session_ui(lane)
    active = roost.identify()["active_tab_id"]
    assert active in session_tab_ids(lane), (
        "the active tab roostctl will resolve is one of the session's"
    )

    listed = roostctl("tab", "list", "--json")
    assert listed.returncode == 0, listed
    payload = json.loads(listed.stdout)
    assert "revision" not in payload, payload
    assert [p["name"] for p in payload["projects"]] == [
        p["name"] for p in lane.session_projects()
    ]

    printed = token()
    sent = roostctl("tab", "send", "--bytes", f"echo {printed}\\n")
    assert sent.returncode == 0, sent
    with lane.session() as c:
        wait_until(
            lambda: printed in c.dump_text(active),
            scaled_timeout(30.0),
            "roostctl's write to reach the session's shell",
        )

    stated = roostctl("tab", "set-state", "--state", "needs_input")
    assert stated.returncode == 0, stated
    with lane.session() as c:
        assert c.agent_lifecycle(active) == "waiting", c.tab(active)


def test_a_forwarded_refusal_is_the_sessions_own_verdict(lane: Lane):
    """AC8's error-parity clause, on a refusal only the session can give.

    `tab.reorder` naming a tab that belongs to *another* project is
    `invalid-param` on the session, because the project is there and the
    tab is not in it. Answered against the hidden workspace it would be
    `not-found`, because no project with that id is there at all — so the
    code alone separates the two, and the message is compared byte for
    byte on top.
    """
    roost = session_ui(lane)
    home = int(roost.list()[0]["id"])
    other = roost.create_project(name="elsewhere", cwd="/tmp")
    stray = roost.open_tab(other, cwd="/tmp")

    with lane.session() as c:
        with pytest.raises(RoostError) as direct:
            c.reorder_tabs(home, [stray])
    with pytest.raises(RoostError) as forwarded:
        roost.reorder_tabs(home, [stray])

    assert direct.value.code == "invalid-param", direct.value
    assert forwarded.value.code == direct.value.code, (
        f"a local answer would have been not-found: {forwarded.value}"
    )
    assert forwarded.value.message == direct.value.message


def test_the_ui_reports_the_tab_it_is_showing_not_the_hidden_workspaces(lane: Lane):
    """`app.selected_tab_id` is UI truth, and under `session` the tab on
    screen lives on the slot.

    The in-process workspace holds nothing at all here, so the op read
    against it answers `0` — a wrong answer that looks like a legitimate
    "nothing selected". Directly assertable, and asserted against
    `identify`, which is the same question at a different width.
    """
    roost = session_ui(lane)
    active = roost.identify()["active_tab_id"]
    assert active != 0
    assert roost.app_selected_tab_id() == active
    assert active in session_tab_ids(lane)
    assert roost.list() != [], "and the list it came from is the slot's"


def test_a_forwarded_delete_of_the_last_project_is_answered_before_the_exit(lane: Lane):
    """AC7 on the far side of §D10: the deletion reply is written before
    the process goes.

    Same assertion shape as `test_exit_on_empty.py`: `Roost.call` raises
    on an error envelope **and** on a socket that closes mid-response, so
    a normal return from the last `project.delete` is the proof. The
    difference is where the delete goes — it is forwarded to the slot,
    and the auto-remove it triggers is what takes the slot out of the
    registry and lets §D9's predicate close the window.

    **What this does not pin.** No control flips it: neither dropping
    the forward's `HostOpsInFlight` registration nor removing `main.rs`'s
    one-message-hop before `iced::exit()` makes it fail, because the
    auto-remove asks the session `tab.list` before it forgets the host,
    and that round trip is slack enough on its own. So this pins the
    *outcome* — the reply arrives and the process ends cleanly — and not
    the ordering mechanism, which no test in this tree reaches. See
    `negative-controls.md`.
    """
    roost = session_ui(lane)
    projects = [int(p["id"]) for p in roost.list()]
    assert projects, roost.list()
    process = ui.owned_process(lane.target)
    assert process is not None, "this lane owns the UI it is about to watch exit"

    for project in projects[:-1]:
        roost.delete_project(project)
    assert process.poll() is None, "a non-empty slot must not exit"

    # The reply for THIS call is the assertion.
    roost.delete_project(projects[-1])

    exit_code = process.wait(timeout=scaled_timeout(30.0))
    assert exit_code == 0, (
        f"the UI must end its run loop normally (exit {exit_code})"
    )
    assert not ui.is_alive(lane.target), "the IPC socket must not answer after the exit"


def test_a_slot_that_goes_away_stops_being_what_a_bare_id_means(lane: Lane):
    """§D10's route follows the **connection**, not just the selection.

    Stopping the daemon under a UI that has one of its tabs selected
    moves no selection — `reconcile_host_selection` keeps the frozen
    frame it is still drawing — so a route published only on selection
    and mode changes would go on naming a tab on a dead incarnation, and
    `roostctl` with no `--tab` would keep addressing it.
    """
    roost = session_ui(lane)
    active = roost.identify()["active_tab_id"]
    assert active != 0

    lane.stop_daemon()
    wait_until(
        lambda: local_band(roost)["state"] != "connected",
        scaled_timeout(60.0),
        "the slot's band to leave connected",
    )
    wait_until(
        lambda: roost.identify()["active_tab_id"] == 0,
        scaled_timeout(30.0),
        "identify to stop naming a tab on a connection that is gone",
    )

    # And the rewrite destination went with it, rather than addressing
    # the dead incarnation.
    with pytest.raises(RoostError) as refused:
        roost.call("tab.dump", {"tab_id": str(active)})
    assert refused.value.code == "host-unavailable", refused.value
    assert "local session is not connected" in refused.value.message


def test_a_host_qualified_ref_is_never_re_addressed_to_the_slot(lane: Lane):
    """§D10's ordering clause, end to end.

    The slot's own `h<n>.<id>` spelling is read off `app.sidebar_dump`,
    so the pair below differs in exactly one thing: which host the ref
    names. Naming the slot works; naming an incarnation nobody holds is
    `not-found` — it is never quietly turned into the slot's tab, which
    is what a generic "session mode ⇒ forward" branch would have done.
    """
    roost = session_ui(lane)
    bare = roost.identify()["active_tab_id"]
    keys = [
        tab["key"]
        for host in roost.sidebar_hosts()
        for project in host["projects"]
        for tab in project["tabs"]
    ]
    qualified = next(key for key in keys if key.endswith(f".{bare}"))
    assert qualified != str(bare), keys

    bare_rows = roost.call("tab.dump", {"tab_id": str(bare)})["rows_text"]
    assert roost.call("tab.dump", {"tab_id": qualified})["rows_text"] == bare_rows, (
        "the explicit spelling of the slot's own tab is the same tab"
    )

    with pytest.raises(RoostError) as refused:
        roost.call("tab.dump", {"tab_id": "h9999.%d" % bare})
    assert refused.value.code == "not-found", refused.value

    # And a host-qualified *mutation* is not re-addressed either: the
    # selection must not have moved onto the slot's tab.
    with pytest.raises(RoostError) as focus:
        roost.call("tab.focus", {"tab_id": "h9999.%d" % bare})
    assert focus.value.code == "not-found", focus.value
    assert roost.identify()["active_tab_id"] == bare


# ---------------------------------------------------------------------------
# 7. §D5: a `session` launch over a populated in-process workspace
# ---------------------------------------------------------------------------


def migration_layout(roost: Roost) -> list[dict]:
    """Three projects, 2 / 1 / 3 tabs, distinct cwds, two title locks.

    Deliberately none of those numbers alike. The migration's snapshot
    has to come off the **retained tab descriptors** — a workspace a
    `session` launch loaded is never hydrated, so its project rows carry
    no live tabs at all — and a snapshot taken from the rows instead
    would replay three *empty* projects and then delete the originals.
    One project with one tab could not tell those two readings apart;
    this layout fails on the tab counts, on the cwds, on the two locks
    and on the active pair independently.
    """
    boot = roost.list()[0]
    roost.rename_project(int(boot["id"]), "alpha")
    roost.set_title(roost.open_tab(int(boot["id"]), cwd="/tmp"), "alpha-two")
    beta = roost.create_project(name="beta", cwd="/usr")
    roost.open_tab(beta, cwd="/usr")
    gamma = roost.create_project(name="gamma", cwd="/var")
    roost.open_tab(gamma, cwd="/var")
    roost.set_title(roost.open_tab(gamma, cwd="/var/tmp"), "gamma-two")
    last = roost.open_tab(gamma, cwd="/var")
    # The active pair is the LAST tab of the LAST project: a position no
    # "take the first" and no off-by-one lands on by accident.
    roost.focus(last)
    return roost.list()


def with_cwds(projects: list[dict]) -> list[tuple]:
    """`layout`, plus every cwd — the project's and each tab's.

    The cwds are the sharpest thing the retained descriptors carry: a
    tab replayed from the wrong source has the wrong directory *and*,
    since an untitled tab's title is derived from it, the wrong title.
    """
    return [
        (
            p["name"],
            p["cwd"],
            tuple(
                (t["title"], t["cwd"], bool(t.get("user_titled"))) for t in p["tabs"]
            ),
        )
        for p in projects
    ]


def migrated(lane: Lane, roost: Roost, timeout: float = 240.0) -> None:
    """Wait for §D5's launch migration to finish."""
    wait_until(
        lambda: settled(roost) == "session" and lane.in_process_projects() == [],
        timeout,
        "the launch migration to move the in-process layout onto the session",
    )


def test_a_hand_edited_key_moves_the_layout_at_launch(lane: Lane):
    """AC11, on the path that names it: someone edits the key by hand.

    Nothing is listening when the relaunch happens, so the launch's own
    slot dial is what starts the session — and it carries the no-seed
    hint, because a launch that owes a migration connects for
    `SwitchDestination` rather than `EnsureNonempty` (§D5/§D12). The
    destination is therefore *exactly* the source: a seed from either
    side would show up as a fourth project.
    """
    roost = lane.start("in-process")
    want = with_cwds(migration_layout(roost))
    assert [len(p[2]) for p in want] == [2, 1, 3], want
    active = roost.identify()["active_tab_id"]
    assert active == int(roost.list()[-1]["tabs"][-1]["id"]), "the fixture's own premise"
    ui.quit(lane.target)

    lane.write_config("session")
    assert running_session_id() is None, "the launch must be what starts it"
    roost = lane.restart()
    assert roost.identify()["local_backend"] == "session"
    migrated(lane, roost)

    assert running_session_id() is not None, "the launch started the destination"
    assert with_cwds(lane.session_projects()) == want, (
        "the layout moved whole — tab counts, directories and title locks"
    )
    assert lane.journal() is None, "the journal goes with the last source delete"

    # Rows are not shells: every replayed tab has a live PTY behind it.
    with lane.session() as c:
        for project in c.list():
            for tab in project["tabs"]:
                c.resize(int(tab["id"]), 100, 30)

    # The band is the slot's, and the tab the migration selected is the
    # one that was active in the source — asserted through the
    # attachment, as the forward switch's case is: `tab.dump` on a
    # host-qualified key answers off the UI's own client terminal, and
    # `host_focus_tab` detaches every other.
    band = local_band(roost)
    assert band["role"] == "session", band
    slot_rows = roost.sidebar_host(band["saved_id"])
    assert [p["name"] for p in slot_rows["projects"]] == ["alpha", "beta", "gamma"]
    # The tab the migration selected is the one that was active in the
    # source, mapped by position onto ids the destination minted.
    # `identify.active_tab_id` is the slot's selected pair (§D1/§D10),
    # and the fence does not release until that tab is attached and
    # streaming — so reaching `settled` at all is the attachment half.
    wanted = int(lane.session_projects()[-1]["tabs"][-1]["id"])
    assert roost.identify()["active_tab_id"] == wanted, (
        "the source's active tab is the last tab of the last project"
    )
    roost.call("tab.dump", {"tab_id": str(wanted)})


def test_a_crashed_launch_migration_clears_its_copy_and_stays_on_session(lane: Lane):
    """AC11's "or a crash mid-switch resumes", for the migration's own
    journal.

    A launch migration's rollback is **not** the verb's. The verb began
    from `in-process` and a failure owes the user that back; this began
    from a key that already said `session`, so rolling its mode back
    would silently un-edit the key and come up on a backend nobody
    chose. What the rollback owes is the partial copy — and then the
    same launch finds the same populated workspace and migrates it
    properly, which is what "idempotent across a crash" means here.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    want = with_cwds(migration_layout(roost))

    # The half a crashed replay leaves on the destination.
    with lane.session() as c:
        partial = [c.create_project(name=f"half-{n}", cwd="/tmp") for n in range(2)]
        for project in partial:
            c.open_tab(project, cwd="/tmp")
    ui.quit(lane.target)
    lane.write_config("session")
    write_journal(
        lane,
        from_mode="in-process",
        to_mode="session",
        phase="replaying",
        source_snapshot=snapshot_of(1, 1),
        created_dest_ids=partial,
        launch_migration=True,
    )

    roost = lane.restart()
    assert roost.identify()["local_backend"] == "session", (
        "a launch migration has no backend to roll back to; the key stands"
    )
    assert "local-backend = session" in lane.config.read_text()
    migrated(lane, roost)

    assert with_cwds(lane.session_projects()) == want, (
        "the abandoned copy is gone and the source landed exactly once"
    )
    assert lane.journal() is None


def test_a_migration_adopts_an_unresolved_rollback_instead_of_orphaning_it(lane: Lane):
    """§D8b: starting a switch replaces the journal, so what the journal
    still owed has to come with it.

    The sequence, all three steps of it: a switch crashed leaving a copy
    on the session; the next launch's rollback could not reach the
    daemon (SIGSTOPped here, wedged in the wild) and correctly kept its
    journal; then the daemon answers again and §D5's migration arms. A
    migration that wrote a fresh journal would erase the only record of
    that copy — it is orphaned for good — and then replay the same
    layout over it, leaving the user with everything twice. Which is the
    duplication the owner's No-Replay decision was taken to avoid
    elsewhere.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
    want = with_cwds(migration_layout(roost))

    with lane.session() as c:
        orphan = c.create_project(name="half-0", cwd="/tmp")
        c.open_tab(orphan, cwd="/tmp")
    ui.quit(lane.target)

    lane.write_config("session")
    write_journal(
        lane,
        from_mode="in-process",
        to_mode="session",
        phase="replaying",
        source_snapshot=snapshot_of(1),
        created_dest_ids=[orphan],
        launch_migration=True,
    )

    # The launch that cannot finish the rollback.
    assert lane.pid is not None
    os.kill(lane.pid, signal.SIGSTOP)
    try:
        roost = lane.restart()
    finally:
        os.kill(lane.pid, signal.SIGCONT)
    assert roost.identify()["local_backend"] == "session"
    assert lane.journal() is not None, (
        "the copy it could not delete stays described — the precondition"
    )

    # The daemon answers again, so the migration arms.
    migrated(lane, roost)
    assert with_cwds(lane.session_projects()) == want, (
        "the adopted copy was removed before the replay, and nothing landed twice"
    )
    assert lane.journal() is None


# ---------------------------------------------------------------------------
# 8. §D11: a link clicked in a session tab opens on THIS machine
# ---------------------------------------------------------------------------
#
# True by construction — `url_launcher.rs` runs in the UI process and the
# session never opens a URL — which is exactly why it is worth a pin: the
# thing that would break it is someone moving the launch to where the
# shell is. Linux-only for a concrete reason: `xdg-open` is resolved
# through `PATH`, and macOS spawns `/usr/bin/open` by absolute path,
# which no stub can stand in front of.

#: `mods::ALT` in the key encoder's bit layout (shift 1, ctrl 2, alt 4,
#: super 8) — the Linux link modifier (`keybind::default_link_modifier`).
LINK_MOD = 4


def url_cell(roost: Roost, tab_id: int, url: str) -> tuple[int, int]:
    """Where the printed URL is on the grid, as (cell_x, cell_y)."""
    rows = roost.call("tab.dump", {"tab_id": str(tab_id)})["rows_text"]
    for y, row in enumerate(rows):
        index = row.find(url)
        if index >= 0:
            # The middle of the run, so neither end can be an off-by-one
            # onto a cell the hyperlink span does not cover.
            return index + len(url) // 2, y
    raise AssertionError(f"{url!r} is not on the grid: {rows!r}")


@pytest.mark.skipif(
    platform.system() != "Linux", reason="the xdg-open stub is a PATH stub; #390"
)
def test_a_link_in_a_session_tab_opens_on_the_machine_the_window_is_on(lane: Lane):
    """AC9. The URL is printed by a shell **inside the session** and the
    launcher that opens it is the UI's own.

    The stub records every argument it is handed, and the no-modifier
    press goes first: the file having exactly one line at the end is the
    negative control, and it is a condition a wait can settle on — which
    "nothing happened" on its own is not.
    """
    opened = _ROOT / f"opened-{uuid.uuid4().hex[:8]}.txt"
    stub_dir = _ROOT / f"bin-{uuid.uuid4().hex[:8]}"
    stub_dir.mkdir()
    stub = stub_dir / "xdg-open"
    stub.write_text(f'#!/bin/sh\nprintf "%s\\n" "$1" >> {opened}\n')
    stub.chmod(0o755)

    # The stub is only reachable because `xdg-open` is resolved through
    # `PATH` — which is why this case is Linux-only.
    roost = session_ui(
        lane, extra_env={"PATH": f"{stub_dir}:{os.environ['PATH']}"}
    )
    tab = roost.identify()["active_tab_id"]
    url = f"https://roost.test/{uuid.uuid4().hex[:8]}"
    # Printed by the session's own shell, through the session's socket:
    # the tab under the pointer is a *remote* one as far as this window
    # is concerned, which is the whole of §D11.
    with lane.session() as c:
        c.send(tab, f"printf '%s\\n' {url}\n")
    wait_until(
        lambda: url in "\n".join(roost.call("tab.dump", {"tab_id": str(tab)})["rows_text"])
        or None,
        scaled_timeout(30.0),
        "the URL to reach the window",
    )
    cell_x, cell_y = url_cell(roost, tab, url)

    # No modifier: an ordinary press, which begins a selection and opens
    # nothing.
    roost.tab_dispatch_mouse_event(tab, "press", "left", cell_x, cell_y, mods=0)
    roost.tab_dispatch_mouse_event(tab, "release", "left", cell_x, cell_y, mods=0)
    # With it: the launcher runs, here.
    roost.tab_dispatch_mouse_event(tab, "press", "left", cell_x, cell_y, mods=LINK_MOD)
    roost.tab_dispatch_mouse_event(tab, "release", "left", cell_x, cell_y, mods=LINK_MOD)

    wait_until(
        lambda: opened.exists() and opened.read_text().strip() or None,
        scaled_timeout(30.0),
        "the local URL launcher to be handed the link",
    )
    assert opened.read_text().splitlines() == [url], (
        "exactly one launch, from the modifier-held press — a press without "
        "the link modifier must open nothing"
    )
