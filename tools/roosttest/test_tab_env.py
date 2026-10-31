"""A tab's environment comes from who opened it (plan 085 D7, D9), seen
from a UI: what iced opens itself, a socket `tab.open` that states
nothing, and what `roostctl` opens.

iced is relaunched here with a fake agent, bus and SSH login of its own
beside the harness's real display, so its values are told apart from
every other opener's, and a tab that carried its `SSH_*` would show it.
Each tab is read from its process (`tab_env`), comparing the opener set
and `XDG_RUNTIME_DIR` only — a shell adds variables of its own. On macOS,
where CI's iced lane runs this module too, iced's shell is a stand-in
whose environment can be read (`tab_env.readable_shell`) and an opened
tab runs `tab_env.sleep_argv`. `roostctl` runs with an explicit
environment per case, never the runner's.

Against Roost.app (Swift is frozen and refuses both fields, rule 7) only
that `roostctl` keeps working is asserted: by profile it states nothing,
by `--socket` it is refused and asks again without.
"""

from __future__ import annotations

import contextlib
import json
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

import pytest
import tab_env
import ui
import util
from client import Roost, scaled_timeout

#: iced's own agent, bus and SSH login: none of them real, all distinct
#: from every opener a case states.
ICED_ENV = {
    "SSH_AUTH_SOCK": "/fake/iced/agent",
    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/fake/iced/bus",
    "SSH_CONNECTION": "100.91.0.1 50100 100.91.0.2 22",
    "SSH_CLIENT": "100.91.0.1 50100 22",
}
#: `roostctl` at a desk.
ROOSTCTL_DESK = {
    "DISPLAY": ":93",
    "WAYLAND_DISPLAY": "/roostctl/run/wayland-9",
    "SSH_AUTH_SOCK": "/roostctl/agent",
}
#: `roostctl` in an SSH login, with a forwarded display of its own.
ROOSTCTL_LOGIN = {
    "SSH_CONNECTION": "100.92.0.3 50200 100.92.0.4 22",
    "SSH_CLIENT": "100.92.0.3 50200 22",
    "SSH_TTY": "/dev/pts/9",
    "DISPLAY": "localhost:11.0",
}
ROOSTCTL_REMOTE = {
    "SSH_CONNECTION": ROOSTCTL_LOGIN["SSH_CONNECTION"],
    "SSH_CLIENT": ROOSTCTL_LOGIN["SSH_CLIENT"],
}


def roostctl(*args: str, opener_env: dict[str, str]) -> subprocess.CompletedProcess:
    """`roostctl <args>` with `opener_env` and nothing else of the runner's
    but `PATH` and `HOME` (the Mac profile's socket is under `HOME`)."""
    return util.explicit_roostctl(*args, env={"HOME": str(Path.home()), **opener_env})


class Iced:
    """The harness's iced, relaunched with `ICED_ENV` over a private state
    dir; `launch` again to restart it over a hand-written layout."""

    def __init__(self, target: str, root: Path):
        self.target = target
        self.state_dir = root / "state"
        self.state_dir.mkdir()
        self.binary = ui.rust_binary_path(target)[0]
        shell = tab_env.readable_shell(root) if sys.platform == "darwin" else Path("/bin/sh")
        self.extra_env = {**ICED_ENV, "SHELL": str(shell)}
        self.roost: Roost | None = None
        self.pid = 0
        self.env: dict[str, str] = {}

    def launch(self, state: dict | None = None) -> Roost:
        if self.roost is not None:
            self.roost.close()
        ui.quit(self.target)
        if state is not None:
            (self.state_dir / "state.json").write_text(json.dumps(state))
        ui.launch(self.target, state_dir=self.state_dir, force=True, extra_env=self.extra_env)
        proc = ui.owned_process(self.target)
        assert proc is not None, "the harness launched this iced"
        self.pid = proc.pid
        self.env = tab_env.child_environ(self.pid, binaries=[self.binary])
        self.roost = Roost(ui.socket_path(self.target))
        return self.roost

    def tab_environ(self, tab_id: int) -> dict[str, str]:
        _, environ = tab_env.tab_process(self.pid, tab_id, binaries=[self.binary])
        return environ

    def opener(self) -> dict[str, str]:
        """What iced brings as a local opener, from the environment it was
        exec'd with."""
        return tab_env.local_set_from(self.env)


@pytest.fixture(scope="module")
def iced(target):
    if target != "iced":
        pytest.skip("iced's own opener; the Swift app is frozen (rule 7)")
    if ui.session_state_dir() is None:
        pytest.skip("relaunching needs a harness-owned UI")
    if ui.iced_bundle_app() is not None:
        pytest.skip("an `open`-launched bundle takes no environment of the harness's")
    root = Path(tempfile.mkdtemp(prefix="roost-te-", dir="/tmp"))
    world = Iced(target, root)
    try:
        world.launch()
        yield world
    finally:
        if world.roost is not None:
            world.roost.close()
        with contextlib.suppress(Exception):
            ui.quit(target)
        ui.launch(target, force=True)
        shutil.rmtree(root, ignore_errors=True)


def first_project(roost: Roost) -> int:
    return int(roost.list()[0]["id"])


def roostctl_opened(reply: subprocess.CompletedProcess, verb: str) -> int:
    assert reply.returncode == 0, (reply.returncode, reply.stdout, reply.stderr)
    if verb == "open":
        return int(json.loads(reply.stdout)["tab"]["id"])
    return int(reply.stdout.strip())


def roostctl_open(socket: Path, verb: str, project: int, opener_env: dict[str, str]) -> int:
    """A sleeping tab opened by `roostctl tab open` or `roostctl open` on
    `socket`, run in `opener_env`."""
    argv = tab_env.sleep_argv(600)
    if verb == "open":
        args = ["--socket", str(socket), "open", "--project", f"te-{uuid.uuid4().hex[:6]}",
                "--cwd", "/tmp", "--", *argv]
    else:
        args = ["--socket", str(socket), "tab", "open", "--project-id", str(project),
                "--cwd", "/tmp", "--", *argv]
    return roostctl_opened(roostctl(*args, opener_env=opener_env), verb)


# ---------------------------------------------------------------------------
# iced, in-process
# ---------------------------------------------------------------------------


def test_a_tab_iced_opens_has_iceds_local_values_and_no_ssh(iced: Iced):
    """The palette's `new_tab`, as a person presses it: iced is the opener,
    a local one, though its own environment is an SSH login's."""
    assert iced.env["SSH_CONNECTION"] == ICED_ENV["SSH_CONNECTION"], "the premise"
    roost = iced.roost
    before = {int(tab["id"]) for tab in roost.tabs()}
    util.press_new_tab(roost)
    tab = util.spawned_tab_id(roost, before, "the palette's new tab", timeout=scaled_timeout(30.0))

    environ = iced.tab_environ(tab)
    assert tab_env.opener_vars(environ) == iced.opener()
    assert environ.get("XDG_RUNTIME_DIR") == iced.env.get("XDG_RUNTIME_DIR")


def test_a_socket_open_that_states_nothing_gets_the_baseline(iced: Iced):
    """No opener stated is no opener: iced's own values do not stand in."""
    roost = iced.roost
    tab = roost.open_tab(first_project(roost), cwd="/tmp", argv=tab_env.sleep_argv(600))
    assert tab_env.opener_vars(iced.tab_environ(tab)) == {}


@pytest.mark.parametrize("verb", ["tab open", "open"])
@pytest.mark.parametrize(
    ("opener_env", "expected"),
    [
        pytest.param(ROOSTCTL_DESK, ROOSTCTL_DESK, id="desk"),
        pytest.param(ROOSTCTL_LOGIN, ROOSTCTL_REMOTE, id="ssh-login"),
    ],
)
def test_roostctl_opens_its_tab_with_its_own_opener(iced: Iced, verb, opener_env, expected):
    """At a desk `roostctl` brings its display and agent; in an SSH login
    only that login's identity."""
    roost = iced.roost
    tab = roostctl_open(ui.socket_path(iced.target), verb, first_project(roost), opener_env)
    assert tab_env.opener_vars(iced.tab_environ(tab)) == expected


def test_a_tab_saved_before_openers_restores_with_iceds_values(iced: Iced):
    """§4.A.3 (b): only iced, or a local caller on its socket, opened the
    tabs of an in-process workspace, so a tab saved before tabs carried
    an opener is restored as iced's."""
    cwd = str(iced.state_dir)
    state = {
        "next_id": 100,
        "projects": [
            {
                "id": 1,
                "name": "legacy",
                "cwd": cwd,
                "position": 0,
                "created_at": 1,
                "tabs": [{"title": "legacy", "cwd": cwd, "position": 0, "user_titled": True}],
            }
        ],
    }
    roost = iced.launch(state)
    restored = [int(tab["id"]) for tab in roost.tabs() if tab["title"] == "legacy"]
    assert len(restored) == 1, roost.list()
    assert tab_env.opener_vars(iced.tab_environ(restored[0])) == iced.opener()


# ---------------------------------------------------------------------------
# Roost.app: roostctl keeps working
# ---------------------------------------------------------------------------


@pytest.fixture
def mac_project(target, roost):
    if target != "mac":
        pytest.skip("Roost.app's frozen tab.open")
    project = roost.create_project(name=f"pytest-te-{uuid.uuid4().hex[:8]}", cwd="/tmp")
    try:
        yield project
    finally:
        with contextlib.suppress(Exception):
            roost.delete_project(project)


@pytest.mark.parametrize(
    "opener_env",
    [
        pytest.param({"DISPLAY": ":93"}, id="desk"),
        pytest.param({"DISPLAY": ":93", "SSH_CONNECTION": ROOSTCTL_LOGIN["SSH_CONNECTION"]}, id="ssh-login"),
    ],
)
@pytest.mark.parametrize(
    "route",
    [pytest.param(["--target", "mac"], id="by-profile"), pytest.param(None, id="by-socket")],
)
def test_roostctl_still_opens_tabs_on_roost_app(roost, mac_project, opener_env, route):
    """By profile no opener is stated; by `--socket` it is, Swift refuses
    the field by name, and roostctl asks once more without it. `open`
    degrades as it always has — Roost.app serves no `project.ensure` —
    rather than failing on the opener."""
    route = route or ["--socket", str(ui.socket_path("mac"))]
    opened = roostctl(*route, "tab", "open", "--project-id", str(mac_project), "--cwd", "/tmp",
                      opener_env=opener_env)
    tab = roostctl_opened(opened, "tab open")
    assert roost.tab(tab) is not None, roost.list()

    ensured = roostctl(*route, "--json", "open", "--project", "pytest-te", "--cwd", "/tmp",
                       opener_env=opener_env)
    assert ensured.returncode == 1, (ensured.stdout, ensured.stderr)
    assert json.loads(ensured.stderr)["error"]["code"] == "unsupported", ensured.stderr
