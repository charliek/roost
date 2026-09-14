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
from session import wait_until  # noqa: E402
from test_host_local_spawn import roostctl_session, running_session_id  # noqa: E402

pytestmark = pytest.mark.host_client

#: `crates/roost-iced/src/app/local_backend.rs`'s `SWITCH_BUSY`.
BUSY = "busy: a local-backend switch is in progress"
USE_SESSION = "local:use_session"
USE_IN_PROCESS = "local:use_in_process"
JOURNAL = "switch-journal.json"
#: `STATUS_NOT_RUNNING_EXIT` (`crates/roost-cli/src/session.rs`).
NOT_RUNNING_EXIT = 3


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
    assert roost.list() == [], "the in-process workspace is emptied"
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
    assert roost.list() == []

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
        assert [p["name"] for p in roost.list()] == ["stuck"]
        assert len(roost.list()[0]["tabs"]) == 1, "with its tab still running"
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
    assert roost.list() == []


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
    `session` — which is the "hand-edited key" case §D5 names.
    """
    roost = lane.start("in-process")
    want = layout(source_layout(roost))
    ui.quit(lane.target)

    # The layout really is on disk, with its tabs.
    state = json.loads((lane.state_dir / "state.json").read_text())
    saved = {p["name"]: len(p["tabs"]) for p in state["projects"]}
    assert saved == {"alpha": 2, "beta": 1}, state

    lane.write_config("session")
    roost = lane.restart()
    assert roost.identify()["local_backend"] == "session"
    # **The state the fix is about**, asserted before the switch touches
    # it: `Workspace::open` loaded the rows, and session-mode bootstrap
    # did not hydrate them, so they are projects with no shells. The
    # sidebar draws the slot's band instead, which is why nobody sees
    # this.
    retained = roost.list()
    assert [p["name"] for p in retained] == ["alpha", "beta"], retained
    assert all(not p["tabs"] for p in retained), retained
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
    roost = lane.start(
        "in-process", extra_env={"ROOST_SESSION_BIN": str(_ROOT / "no-such-session")}
    )
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


# ---------------------------------------------------------------------------
# 4. The journal, at every phase
# ---------------------------------------------------------------------------


def write_journal(lane: Lane, **fields) -> None:
    lane.journal_path.write_text(json.dumps(fields))


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
        source_snapshot=[],
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


@pytest.mark.parametrize("phase", ["committing", "cleaning-up"])
def test_a_forward_journal_past_the_commit_point_finishes(lane: Lane, phase: str):
    """§D8b's finish arm, at both phases on that side of the line.

    The mirror image of the case above: the same journal one phase later,
    with the key on disk deliberately saying `in-process`. The launch
    must come up on `session` and finish emptying the source.
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
    assert [p["name"] for p in roost.list()] == ["kept"], (
        "the source deletion is finished — for the journal's own list, and no wider"
    )
    assert layout(lane.session_projects()) == want, "the destination is untouched"
    assert lane.journal() is None
    assert "local-backend = session" in lane.config.read_text()


def test_a_reverse_journal_resolves_symmetrically(lane: Lane):
    """§D8b's "reverse → symmetric", both sides of the same line.

    Reverse copies nothing, so the only thing either arm may do is settle
    the mode — and the finish arm in particular must **not** empty the
    in-process workspace it is coming back to.
    """
    roost = lane.start("in-process")
    lane.start_daemon()
    lane.empty_the_session()
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
    roost = lane.restart()
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
        source_snapshot=[],
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
