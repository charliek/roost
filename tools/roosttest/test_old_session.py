"""Plan 072 D13 (#537): today's UI against released sessions.

# What this proves

A real upgrade. A released `roost-session` is **already running**, with a
tab and output in it, when this tree's UI starts on `local-backend =
session` — the moment every user who installs a new Roost meets. Nothing
else runs a released session against the current UI: every other lane
builds both sides from one commit.

Which checks run is chosen by the old binary's own generation, from
`roost-session identify` (offline, in every release), against this tree's:

* **The same generation** — the compatibility checks, against the latest
  release: the UI adopts the running session and shows its tab, ⌘T
  lands in the right directory, typing round-trips, tabs and projects are
  created, renamed and closed, a relaunch comes back to the tab last
  viewed, and today's `roostctl` drives the old session directly.
* **An older generation** — the forced update: the UI says the slot needs a
  restart rather than hanging or erroring, the restart replaces the old
  session with today's, and today's session reads the old one's
  `state.json`. v0.0.19 (generation 2) is pinned for it, and "latest" gets
  it too the day a protocol bump leaves the latest release behind.

v0.0.20 is pinned as well, for ⌘T's `cwd_from_tab` → `unknown-field` retry
(`open_host_tab_flow`): it predates the field, and "latest" soon will not.

"Latest" is resolved at run time, and `ROOST_OLD_SESSION_VERSION` stands in
for it. The binaries come from `old_session/fetch.sh`, which skips — with a
GitHub warning — when GitHub cannot serve them and nothing is cached.

# The namespace

A released session binds `roost-session/`; this tree's debug builds resolve
`roost-session-dev/`. `ROOST_TEST_SESSION_DIR_NAMES=release` moves a debug
build onto the shipped names (`roost_ipc::paths`, ignored by a packaged
one), and it reaches everything here that resolves the session: the UI,
the session the UI's restart spawns, and `roostctl`.

# Owning the sentinel

`test_local_backend.py`'s arrangement: a private root, with
`XDG_RUNTIME_DIR` and its siblings pointed into it **at import**, so the
UI, both sessions and `roostctl` resolve one socket nobody else has.
Hence its own pytest invocation (`make e2e-old-session`), and
`pytestmark = pytest.mark.host_client` so whole-directory runs deselect it.

One module-scoped old session + UI per release, condition waits only, and
every assertion on the op set — `host.status`, `tab.list`, the window's
own terminal — never on a log.
"""

from __future__ import annotations

import atexit
import contextlib
import json
import os
import platform
import queue
import shutil
import subprocess
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass
from functools import cache
from pathlib import Path

import pytest

if platform.system() == "Darwin":
    pytest.skip(
        "releases ship roost-session for Linux only, and the sentinel is "
        "redirected through XDG_RUNTIME_DIR",
        allow_module_level=True,
    )

import agent_jail  # noqa: E402
import session as sessionlib  # noqa: E402

_ROOT = Path(tempfile.mkdtemp(prefix="roost-os-", dir="/tmp")).resolve()
_RUN = _ROOT / "run"
agent_jail.make_private_runtime_dir(_RUN)
for _sibling in ("data", "state", "cache"):
    (_ROOT / _sibling).mkdir()

os.environ["XDG_RUNTIME_DIR"] = str(_RUN)
os.environ["XDG_DATA_HOME"] = str(_ROOT / "data")
os.environ["XDG_STATE_HOME"] = str(_ROOT / "state")
os.environ["XDG_CACHE_HOME"] = str(_ROOT / "cache")
# The shell the restart's session reopens every tab with, as on the old
# side (`make_env`): nothing may rewrite the titles and cwds compared.
os.environ["SHELL"] = "/bin/sh"
# What the forced update restarts into: this tree's session.
os.environ["ROOST_SESSION_BIN"] = str(sessionlib.session_binary())

atexit.register(shutil.rmtree, _ROOT, ignore_errors=True)

import ui  # noqa: E402
import util  # noqa: E402
from client import Roost, RoostError, scaled_timeout  # noqa: E402
from session import wait_until  # noqa: E402

pytestmark = pytest.mark.host_client

FETCH = Path(__file__).resolve().parent / "old_session" / "fetch.sh"
#: Predates `tab.open`'s `cwd_from_tab`, so ⌘T against it takes the retry.
RETRY_PIN = "0.0.20"
#: Generation 2: the forced update's subject whatever "latest" becomes.
FORCED_UPDATE_PIN = "0.0.19"
#: Every process here resolves the shipped session directory names.
DIR_NAMES = "release"
DIR_NAMES_ENV = {sessionlib.SESSION_DIR_NAMES_ENV: DIR_NAMES}
#: The directory the pre-upgrade tab runs in, under the project's own —
#: so a new tab that ignored its source tab would land somewhere else.
WORK = _ROOT / "work"
DIR_A = WORK / "dir-a"


# ---------------------------------------------------------------------------
# Releases
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Release:
    version: str
    binary: Path
    #: `SESSION_PROTOCOL_VERSION` as the binary itself reports it.
    generation: int


def generation(binary: Path) -> int:
    """The binary's `SESSION_PROTOCOL_VERSION`, from `roost-session
    identify`: offline, one JSON line, in every release."""
    result = subprocess.run(
        [str(binary), "identify"], capture_output=True, text=True, timeout=30, check=True
    )
    return int(json.loads(result.stdout)["session_protocol"])


@cache
def main_generation() -> int:
    return generation(sessionlib.session_binary())


def announce(request: pytest.FixtureRequest, line: str) -> None:
    """Write `line` where pytest writes its own report, past the output
    capture: a skip reaches CI as a GitHub `::warning::` annotation only
    from a line the runner actually sees."""
    reporter = request.config.pluginmanager.get_plugin("terminalreporter")
    if reporter is not None:
        reporter.write_line(line)


def fetch(request: pytest.FixtureRequest, which: str) -> Release | str:
    """A verified release binary, or the reason GitHub could not serve one.

    `fetch.sh` exiting non-zero — a broken release, a binary that does not
    match its `.sha256` — fails the run: those are not outages."""
    result = subprocess.run(
        ["bash", str(FETCH), which], capture_output=True, text=True, timeout=300
    )
    if result.returncode != 0:
        pytest.fail(
            f"fetch.sh {which} failed ({result.returncode}): {result.stderr.strip()}"
        )
    fields = {}
    for line in result.stdout.splitlines():
        if line.startswith("::"):
            announce(request, line)
        elif "=" in line:
            key, _, value = line.partition("=")
            fields[key] = value
    if "skip" in fields:
        return fields["skip"]
    binary = Path(fields["binary"])
    return Release(fields["version"], binary, generation(binary))


# ---------------------------------------------------------------------------
# The world: one released session, and today's UI on it
# ---------------------------------------------------------------------------


def token(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def echo_line(printed: str) -> str:
    """A command whose output contains `printed` and whose echo does not."""
    head, tail = printed.split("-", 1)
    return f'echo {head}""-{tail}'


def mirrored(roost: Roost, tab: int) -> str:
    """The window's own terminal for a slot tab, as text:
    `tab.dump_resolved` reads that terminal and nothing else."""
    dumped = roost.tab_dump_resolved(tab)
    rows = [[" "] * dumped["cols"] for _ in range(dumped["rows"])]
    for cell in dumped["cells"]:
        rows[cell["row"]][cell["col"]] = cell["text"] or " "
    return "\n".join("".join(row) for row in rows)


def selected(roost: Roost) -> tuple[int, int]:
    reply = roost.identify()
    return reply["active_project_id"], reply["active_tab_id"]


def lands_on(roost: Roost, project: int, tab: int, what: str) -> None:
    try:
        wait_until(lambda: selected(roost) == (project, tab), 60.0, what)
    except TimeoutError:
        raise AssertionError(
            f"waiting for {what}: the window is showing {selected(roost)}, "
            f"not ({project}, {tab})"
        ) from None


def slot_row(roost: Roost) -> dict:
    """`host.status`'s row for the local session slot."""
    rows = [row for row in roost.host_status()["hosts"] if row["target"] == "localhost"]
    assert len(rows) == 1, rows
    return rows[0]


def slot_settles(roost: Roost, state: str, timeout: float, what: str) -> dict:
    """Wait for the slot's row to reach `state`, and return it. A
    `connected` row counts only once it carries `connect`: the connect
    facts reach the UI as their own feed item, just after the state
    (`test_host_client.wait_live_connect`)."""

    def settled() -> dict | None:
        row = slot_row(roost)
        if row["state"] != state or (state == "connected" and "connect" not in row):
            return None
        return row

    return wait_until(settled, timeout, what)


def window_projects(roost: Roost, saved_id: str) -> dict[str, list[str]]:
    """The slot's projects as the window's sidebar draws them: name → tab
    titles."""
    host = roost.sidebar_host(saved_id) or {"projects": []}
    return {
        project["name"]: [tab["title"] for tab in project["tabs"]]
        for project in host["projects"]
    }


def layout(projects: list[dict]) -> list[tuple[str, str, tuple[str, ...]]]:
    """What a restart promises to keep: every project's name and cwd, and
    its tabs' directories, in order."""
    return [
        (p["name"], p["cwd"], tuple(tab["cwd"] for tab in p["tabs"])) for p in projects
    ]


def launch_ui(target: str) -> Roost:
    ui.launch(target, state_dir=ui.session_state_dir(), force=True, extra_env=DIR_NAMES_ENV)
    return Roost(ui.socket_path(target), timeout=scaled_timeout(30.0))


@dataclass
class World:
    release: Release
    env: sessionlib.SessionEnv
    target: str
    old_session_id: str
    old_pid: int
    project: int
    #: The tab that was there before the upgrade, and what it printed.
    tab: int
    printed: str
    roost: Roost

    def ready(self) -> Roost:
        """The slot connected on the old session, and the window showing
        a tab."""
        slot_settles(self.roost, "connected", 60.0, "the slot to connect to the running session")
        wait_until(
            lambda: self.roost.identify()["active_tab_id"] != 0,
            60.0,
            "the window to select one of the session's tabs",
        )
        return self.roost

    def show(self, tab: int) -> None:
        """Select a slot tab and wait until it streams into the window."""
        self.roost.focus(tab)
        streamed = token("SHOWN")
        with self.env.client() as c:
            c.send(tab, echo_line(streamed) + "\n")
        wait_until(
            lambda: streamed in mirrored(self.roost, tab),
            30.0,
            f"slot tab {tab} to stream into the window",
        )

    def tab_ids(self) -> set[int]:
        with self.env.client() as c:
            return {int(t["id"]) for t in c.tabs()}

    def relaunch(self) -> Roost:
        ui.quit(self.target)
        self.roost.close()
        self.roost = launch_ui(self.target)
        return self.roost


def write_backend(backend: str) -> None:
    """Rewrite `local-backend` in the harness's config copy, leaving every
    other line alone."""
    path = ui.owned_session_config_path()
    assert path is not None, "this lane requires a harness-owned UI"
    lines = [
        line
        for line in path.read_text().splitlines()
        if not line.strip().startswith("local-backend")
    ]
    lines.append(f"local-backend = {backend}")
    path.write_text("\n".join(lines) + "\n")


def sessions_under_the_root() -> list[int]:
    """Every `roost-session` resolving this module's private runtime dir:
    the old one, or the one a restart spawned. Nothing else can resolve
    that directory, so nothing else is found."""
    marker = f"XDG_RUNTIME_DIR={_RUN}".encode()
    found = []
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            if (proc / "comm").read_text().strip() == sessionlib.BIN_NAME and marker in (
                proc / "environ"
            ).read_bytes().split(b"\0"):
                found.append(int(proc.name))
        except OSError:
            continue
    return found


class Worlds:
    """At most one world alive at a time: the harness drives one UI."""

    def __init__(self, request: pytest.FixtureRequest, target: str):
        self.request = request
        self.target = target
        self.releases: dict[str, Release | str] = {}
        self.current: World | None = None

    def release(self, which: str) -> Release:
        if which not in self.releases:
            self.releases[which] = fetch(self.request, which)
        found = self.releases[which]
        if isinstance(found, str):
            pytest.skip(found)
        return found

    def enter(self, release: Release) -> World:
        if self.current is not None and self.current.release == release:
            return self.current
        self.close()
        self.current = self._build(release)
        return self.current

    def _build(self, release: Release) -> World:
        ui.quit(self.target)
        state_dir = ui.session_state_dir()
        assert state_dir is not None, (
            "the harness did not launch the UI, so its state dir — where the "
            "restart's session hydrates from — is unknowable"
        )
        assert ui.test_mode_active(), (
            "this lane types through `tab.feed_ime`, which needs ROOST_TEST_MODE=1 "
            "at UI launch (`make e2e-old-session` sets it)"
        )
        for leftover in ("state.json", "switch-journal.json"):
            (state_dir / leftover).unlink(missing_ok=True)
        shutil.rmtree(state_dir / ui.DERIVED_SESSION_SUBDIR, ignore_errors=True)
        DIR_A.mkdir(parents=True, exist_ok=True)

        env = sessionlib.make_env(
            root=_ROOT,
            state_dir=state_dir / ui.DERIVED_SESSION_SUBDIR,
            binary=release.binary,
            dir_names=DIR_NAMES,
        )
        assert env.namespace == "roost-session", env.namespace
        assert env.answering() is None, f"something already answers {env.socket}"
        try:
            launch = env.start_daemonized()
            assert launch.verdict.kind == "ready" and launch.verdict.pid, launch
            identity = env.wait_answering()

            printed = token("BEFORE")
            with env.client() as c:
                seeded = [int(p["id"]) for p in c.list()]
                project = c.create_project(name="upgrade", cwd=str(WORK))
                tab = c.open_tab(project, cwd=str(DIR_A), title="before")
                for other in seeded:
                    c.delete_project(other)
                c.run(tab, echo_line(printed), ready_timeout=30.0)
                c.wait_text(tab, printed, timeout=30.0)

            write_backend("session")
            return World(
                release=release,
                env=env,
                target=self.target,
                old_session_id=identity["session_id"],
                old_pid=launch.verdict.pid,
                project=project,
                tab=tab,
                printed=printed,
                roost=launch_ui(self.target),
            )
        except BaseException:
            self._tear_down(env)
            raise

    def _tear_down(self, env: sessionlib.SessionEnv) -> None:
        """`env.teardown()` stops, then SIGTERMs and SIGKILLs, what it
        tracks; the session a restart spawned is tracked here first."""
        with contextlib.suppress(Exception):
            ui.quit(self.target)
        try:
            for pid in sessions_under_the_root():
                env.track_pid(pid)
        finally:
            env.teardown()

    def close(self) -> None:
        world, self.current = self.current, None
        if world is None:
            return
        with contextlib.suppress(Exception):
            world.roost.close()
        self._tear_down(world.env)


@pytest.fixture(scope="module")
def worlds(request, target):
    assert target == "iced", "this lane drives the Rust UI's session slot"
    held = Worlds(request, target)
    try:
        yield held
    finally:
        try:
            held.close()
        finally:
            with contextlib.suppress(Exception):
                write_backend("in-process")


@pytest.fixture
def compatible_latest(worlds: Worlds) -> World:
    release = worlds.release("latest")
    if release.generation != main_generation():
        pytest.skip(
            f"latest is v{release.version}, generation {release.generation}; this tree "
            f"is {main_generation()}, so it gets the forced-update check instead"
        )
    return worlds.enter(release)


@pytest.fixture
def retry_pin(worlds: Worlds) -> World:
    release = worlds.release(RETRY_PIN)
    if release.generation != main_generation():
        pytest.skip(
            f"v{RETRY_PIN} is generation {release.generation} and this tree is "
            f"{main_generation()}: there is no compatible session left to retry against"
        )
    return worlds.enter(release)


@pytest.fixture(params=["latest", FORCED_UPDATE_PIN])
def older_generation(request, worlds: Worlds):
    """Closed straight after its test: the forced update replaced its
    session, so nothing else may reuse it."""
    release = worlds.release(request.param)
    if release.generation == main_generation():
        pytest.skip(
            f"{request.param} is v{release.version}, generation {release.generation} "
            "like this tree: it gets the compatibility checks"
        )
    assert release.generation < main_generation(), (
        f"v{release.version} speaks generation {release.generation}, newer than "
        f"this tree's {main_generation()}"
    )
    yield worlds.enter(release)
    worlds.close()


# ---------------------------------------------------------------------------
# Compatibility: the latest release, when it speaks this tree's generation
# ---------------------------------------------------------------------------


def test_1_the_ui_adopts_the_running_session_and_renders_its_tab(compatible_latest: World):
    w = compatible_latest
    row = slot_settles(
        w.roost, "connected", 60.0, "the slot to connect to the session that was already running"
    )
    assert row["connect"]["session_id"] == w.old_session_id, (
        f"the UI started a session of its own instead of adopting v{w.release.version}'s: {row}"
    )
    assert w.env.identify()["app_version"] == w.release.version
    lands_on(w.roost, w.project, w.tab, "the window to show the pre-upgrade tab")
    wait_until(
        lambda: w.printed in mirrored(w.roost, w.tab),
        30.0,
        "the pre-upgrade output to render in the window's own terminal",
    )


def assert_new_tab_starts_where_its_source_tab_is(w: World) -> None:
    """⌘T, through the palette's `new_tab` row (the same dispatch the
    keybind runs), from the pre-upgrade tab: the new tab's shell starts in
    that tab's directory, not the project's."""
    roost = w.ready()
    w.show(w.tab)
    before = w.tab_ids()
    util.press_new_tab(roost)
    with w.env.client() as c:
        new = util.spawned_tab_id(c, before, "⌘T to open a tab", timeout=30.0)
    try:
        lands_on(roost, w.project, new, "⌘T's tab to be selected in the source's project")
        with w.env.client() as c:
            util.assert_opened_in(c, new, DIR_A)
    finally:
        with contextlib.suppress(Exception), w.env.client() as c:
            c.close_tab(new)


def test_2_a_new_tab_starts_in_the_directory_of_the_tab_it_came_from(
    compatible_latest: World,
):
    assert_new_tab_starts_where_its_source_tab_is(compatible_latest)


def test_3_typing_into_a_tab_round_trips(compatible_latest: World):
    """Text committed through the window's keyboard route — the path an
    input method takes (`tab.feed_ime`) — runs in the old session's shell
    and comes back into the window's terminal."""
    w = compatible_latest
    roost = w.ready()
    w.show(w.tab)
    printed = token("TYPED")
    roost.tab_feed_ime(w.tab, "commit", echo_line(printed) + "\r")
    wait_until(
        lambda: printed in mirrored(roost, w.tab),
        30.0,
        "what was typed to run and render in the window",
    )
    with w.env.client() as c:
        assert printed in c.dump_text(w.tab), "it ran in the old session's shell"


def test_4_create_rename_and_close_reach_the_session_and_the_window(compatible_latest: World):
    w = compatible_latest
    roost = w.ready()
    saved_id = slot_row(roost)["id"]
    project = roost.create_project(name="made-here", cwd=str(WORK))
    tab = roost.open_tab(project, cwd=str(DIR_A), title="made-tab")
    try:
        roost.set_title(tab, "renamed-tab")
        roost.rename_project(project, "renamed-here")
        wait_until(
            lambda: window_projects(roost, saved_id).get("renamed-here") == ["renamed-tab"],
            30.0,
            "the window to draw the renamed project and tab",
        )
        with w.env.client() as c:
            assert c.project(project)["name"] == "renamed-here"
            assert c.tab(tab)["title"] == "renamed-tab"

        roost.close_tab(tab)
        wait_until(
            lambda: "renamed-tab"
            not in window_projects(roost, saved_id).get("renamed-here", []),
            30.0,
            "the window to drop the closed tab",
        )
        with w.env.client() as c:
            assert c.tab(tab) is None, "the old session still lists the closed tab"
    finally:
        with contextlib.suppress(Exception), w.env.client() as c:
            if c.project(project) is not None:
                c.delete_project(project)


def test_5_a_ui_relaunch_keeps_the_tabs_and_lands_on_the_tab_last_viewed(
    compatible_latest: World,
):
    """071-D11's memory against an old session. The tab viewed is neither
    the project's first nor the session's active one, which is moved to
    another project on the session's own socket before the relaunch."""
    w = compatible_latest
    roost = w.ready()
    with w.env.client() as c:
        viewed = c.open_tab(w.project, cwd=str(DIR_A), title="b")
        c.open_tab(w.project, cwd=str(DIR_A), title="c")
    w.show(viewed)
    lands_on(roost, w.project, viewed, "the window to show the tab it will come back to")
    with w.env.client() as c:
        elsewhere = c.create_project(name="elsewhere", cwd=str(WORK))
        active = c.open_tab(elsewhere, cwd=str(WORK))
        assert c.tab(active)["is_active"] is True, "the session's active tab left the project"
    kept = w.tab_ids()
    saved_id = slot_row(roost)["id"]

    def tabs_shown(roost: Roost) -> dict[str, int]:
        return {name: len(tabs) for name, tabs in window_projects(roost, saved_id).items()}

    shown = wait_until(
        lambda: (seen := tabs_shown(roost)).get("elsewhere") == 1 and seen,
        30.0,
        "the window to mirror the project opened on the session's socket",
    )

    roost = w.relaunch()

    lands_on(roost, w.project, viewed, "the relaunch to land on the tab last viewed")
    assert w.tab_ids() == kept, "the relaunch kept every tab"
    wait_until(
        lambda: tabs_shown(roost) == shown,
        30.0,
        f"the relaunched window to show every tab it showed before: {shown}",
    )
    row = slot_settles(roost, "connected", 60.0, "the relaunched slot's connect facts")
    assert row["connect"]["session_id"] == w.old_session_id


def test_6_todays_roostctl_drives_the_old_session(compatible_latest: World):
    """`open`, `tab list`, `tab dump`, `wait` and `events`, straight at the
    old session (`--target session`)."""
    w = compatible_latest

    def roostctl(*args: str) -> subprocess.CompletedProcess:
        result = w.env.roostctl("--target", "session", *args, timeout=60.0)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result

    opened = json.loads(roostctl("open", "--project", "roostctl", "--cwd", str(DIR_A)).stdout)
    tab = int(opened["tab"]["id"])
    assert opened["created"] is True, opened

    lines: queue.Queue[str] = queue.Queue()
    events = subprocess.Popen(
        [util.roostctl_path(), "--target", "session", "events", "--tab", str(tab)],
        env=w.env.command_env(),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )

    def read() -> None:
        for line in events.stdout:
            lines.put(line)

    threading.Thread(target=read, daemon=True).start()
    renames: list[str] = []
    try:
        # `events` prints nothing until something happens, so nothing says
        # when its subscription is live: rename until one rename streams.
        deadline = time.monotonic() + scaled_timeout(30.0)
        seen: list[dict] = []
        while not any(
            e.get("event") == "tab.title_changed" and e["data"]["title"] in renames
            for e in seen
        ):
            assert time.monotonic() < deadline, (
                f"`roostctl events` streamed none of {renames}: {seen}"
            )
            renames.append(f"via-roostctl-{len(renames)}")
            roostctl("set-title", "--tab", str(tab), "--title", renames[-1])
            with contextlib.suppress(queue.Empty):
                seen.append(json.loads(lines.get(timeout=scaled_timeout(1.0))))
                while not lines.empty():
                    seen.append(json.loads(lines.get_nowait()))
    finally:
        events.terminate()
        try:
            events.wait(timeout=scaled_timeout(10.0))
        except subprocess.TimeoutExpired:
            events.kill()
            events.wait()

    printed = token("CTL")
    roostctl("tab", "send", "--tab", str(tab), "--bytes", echo_line(printed) + "\\n")
    roostctl("wait", "--tab", str(tab), "--text", printed, "--timeout", str(int(scaled_timeout(30))))
    assert printed in roostctl("tab", "dump", "--tab", str(tab)).stdout

    listed = json.loads(roostctl("--json", "tab", "list").stdout)
    rows = {int(t["id"]): t for p in listed["projects"] for t in p["tabs"]}
    assert rows[tab]["title"] == renames[-1], rows[tab]


# ---------------------------------------------------------------------------
# 7. D4 (#564): a refocused tab never replays at the width it left
# ---------------------------------------------------------------------------

#: Window sizes whose terminals sit on either side of 100 columns, so the
#: marker [`WIDTH_MARKER`] writes at `CUP 5;100` lands in different cells
#: at the two: the narrow grid's last column, or column 100 of the wide one.
NARROW_WINDOW = (760.0, 560.0)
WIDE_WINDOW = (1600.0, 560.0)
#: `\043` rather than `#`, so the command line the shell echoes carries no
#: marker of its own.
WIDTH_MARKER = "printf '\\033[5;100H\\043'"


def screen(dumped: dict) -> tuple[int, int, dict[tuple[int, int], str]]:
    """A resolved dump's grid and its text, cell for cell."""
    cells = {(c["row"], c["col"]): c["text"] for c in dumped["cells"] if c["text"].strip()}
    return dumped["cols"], dumped["rows"], cells


def marks(dumped: dict) -> list[tuple[int, int]]:
    """Where the marker's row holds a `#`."""
    return sorted((c["row"], c["col"]) for c in dumped["cells"] if c["row"] == 4 and c["text"] == "#")


@pytest.mark.parametrize("widen", ["window", "font"])
def test_7_a_refocused_tab_never_replays_at_the_width_it_left(compatible_latest: World, widen: str):
    """A tab detached at a narrow grid; the old session writing at that
    grid; the window's grid widened past 100 columns — by the window, or
    by the font; the tab focused again. The window must then draw what the
    session holds.

    A released session cuts the resume after resizing to the attach's
    grid, and has no D4b to refuse it, so only the client can: this is
    D4a's end-to-end control. The marker is written before the widen
    because the window resizes the session's unshown tabs along with
    itself (D5), forgetting their resume points as it does — at once on a
    font change, so there the re-grid site's forgetting and the wave's
    cover for each other. With both removed, the marker lands at column
    100 in the window and at the narrow grid's last column in the session.
    """
    w = compatible_latest
    roost = w.ready()
    before = roost.window_metrics()
    with w.env.client() as c:
        tab = c.open_tab(w.project, cwd=str(DIR_A), title="width")
        away = c.open_tab(w.project, cwd=str(DIR_A), title="away")
    try:
        roost.window_resize(*NARROW_WINDOW)
        w.show(tab)
        narrow = roost.tab_dump_resolved(tab)["cols"]
        assert narrow < 100, f"the narrow window gave {narrow} columns"
        # The window streams one tab at a time: this detaches `tab` with a
        # resume point at the narrow grid.
        w.show(away)
        with w.env.client() as c:
            c.send(tab, WIDTH_MARKER + "\n")
            wait_until(
                lambda: marks(c.tab_dump_resolved(tab)) == [(4, narrow - 1)],
                30.0,
                "the old session to write the marker at its narrow grid",
            )

        if widen == "window":
            roost.window_resize(*WIDE_WINDOW)
        else:
            util.widen_by_font(roost, tab, 100)
        wait_until(
            lambda: roost.tab_dump_resolved(tab)["cols"] > 100,
            30.0,
            f"the {widen} to widen the detached tab's terminal past 100 columns",
        )

        roost.focus(tab)

        def served() -> dict:
            with w.env.client() as c:
                return c.tab_dump_resolved(tab)

        try:
            wait_until(
                lambda: screen(roost.tab_dump_resolved(tab)) == screen(served()),
                30.0,
                "the refocused tab to show the old session's screen",
            )
        except TimeoutError:
            raise AssertionError(
                f"the window drew the marker at {marks(roost.tab_dump_resolved(tab))}, "
                f"the old session at {marks(served())}: the resume replayed records "
                f"written for {narrow} columns at the widened grid"
            ) from None
    finally:
        if widen == "font":
            util.palette_command(roost, "font_reset")
        roost.window_resize(before["window_width"], before["window_height"])
        with contextlib.suppress(Exception), w.env.client() as c:
            c.close_tab(tab)
            c.close_tab(away)


# ---------------------------------------------------------------------------
# The pinned v0.0.20: ⌘T's `unknown-field` retry
# ---------------------------------------------------------------------------


def test_2_pinned_v0_0_20_new_tab_reaches_its_directory_through_the_unknown_field_retry(
    retry_pin: World,
):
    """Check 2 against the pin, and only the pin: v0.0.20's `tab.open`
    predates `cwd_from_tab`, so ⌘T lands only through
    `open_host_tab_flow`'s `unknown-field` retry. Its negative control —
    skip the retry — goes red against this release and against no session
    that knows the field, which "latest" soon will."""
    w = retry_pin
    with w.env.client() as c, pytest.raises(RoostError) as refused:
        c.open_tab(w.project, cwd=str(DIR_A), cwd_from_tab=w.tab)
    assert refused.value.code == "unknown-field", (
        f"v{RETRY_PIN} no longer refuses cwd_from_tab, so this check no longer "
        f"reaches the retry: {refused.value}"
    )
    assert_new_tab_starts_where_its_source_tab_is(w)


# ---------------------------------------------------------------------------
# The forced update: a release older than this tree's generation
# ---------------------------------------------------------------------------


def test_an_older_session_is_named_restarted_and_its_layout_kept(older_generation: World):
    w = older_generation
    roost = w.roost
    with w.env.client() as c:
        c.open_tab(w.project, cwd=str(WORK), title="second")
        before = layout(c.list())

    # 1. Named: the slot settles needing a restart, and says why.
    row = slot_settles(
        roost,
        "needs-restart",
        60.0,
        f"the slot to settle needing a restart against v{w.release.version}",
    )
    assert "connect" not in row, row
    roost.call("host.connect", {"id": row["id"], "test_user_origin": True})
    card = roost.call("app.dialog_dump", {})
    assert card.get("dialog") == "confirm_restart", card
    assert (
        f"session protocol {w.release.generation}, this client speaks {main_generation()}"
        in card["body"]
    ), card["body"]

    # 2. Restarted: today's session replaces the old one on the same socket.
    roost.call("app.dialog_answer", {"action": "confirm"})
    row = slot_settles(
        roost, "connected", 60.0, "the restart to connect the slot to today's session"
    )
    w.env.wait_pid_gone(w.old_pid)
    now = w.env.identify()
    assert now["session_protocol"] == main_generation(), now
    assert row["connect"]["session_id"] == now["session_id"] != w.old_session_id, (row, now)

    # 3. Kept: today's session read the old one's state.json.
    with w.env.client() as c:
        assert layout(c.list()) == before
    wait_until(
        lambda: len(window_projects(roost, row["id"]).get("upgrade", [])) == len(before[0][2]),
        60.0,
        "the window to list the restored layout",
    )
