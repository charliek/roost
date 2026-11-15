"""The process layer (#621) end to end, on both terminals that can be a
tab's authoritative one: a session tab's, inside a real `roost-session`
(marked `session_daemon`, so it runs in `make e2e-session`), and an
in-process tab's, inside the harness UI (unmarked, so it runs in the iced
lanes).

The agents are real processes. A tab is a bare bash whose
`PROMPT_COMMAND` prints the `OSC 133 ; D` Roost's own integration prints,
and the agent is a job it runs in front; the hook claim comes from the
test, and `Tab.process` names the job the claim was stamped with, so the
test kills exactly that process.

Condition waits only.
"""

from __future__ import annotations

import os
import re
import signal
import subprocess
import uuid
from dataclasses import dataclass
from typing import Callable

import pytest
import session as sessionlib
from client import Roost
from util import BARE_SHELL_ARGV, wait_tab_attached

COLS, ROWS = 80, 24
MARKS = r"""PROMPT_COMMAND='printf "\033]133;D\007"'"""
PID = re.compile(r"SHELL_PID_(\d+)_")


@dataclass
class Lane:
    """One authoritative terminal's side of the op set: a client on its
    socket and how to open a tab there."""

    name: str
    client: Roost
    open_tab: Callable[[list[str]], int]

    def wait(self, tab: int, what: str, done: Callable[[dict], bool]) -> dict:
        box: dict = {}

        def listed() -> bool:
            box["tab"] = self.client.tab(tab)
            return box["tab"] is not None and done(box["tab"])

        self.client._wait(listed, 15.0, f"tab {tab}: {what}")
        return box["tab"]

    def bash_tab(self) -> tuple[int, int]:
        """A bash tab whose every prompt prints a `D` mark, and its shell's
        pid. The pid goes through `%s`, so only the output has digits after
        the marker; the echoed command line has `%s`."""
        tab = self.open_tab(BARE_SHELL_ARGV)
        self.client.run(tab, MARKS + r"""; printf 'SHELL_PID_%s_\n' "$$" """)
        box: dict = {}

        def printed() -> bool:
            box["pid"] = PID.search(self.client.dump_text(tab))
            return box["pid"] is not None

        self.client._wait(printed, 15.0, f"tab {tab}'s shell pid")
        self.wait(tab, "the first D mark", lambda t: t["shell_state"] == "at_prompt")
        return tab, int(box["pid"].group(1))

    def stamped_job(self, tab: int, shell: int) -> dict:
        """`sleep 300` in front of `shell`, claimed by an agent and stamped:
        `Tab.process`. The claim waits for the job to have the terminal, or
        the stamp would find the shell in front."""
        self.client.send(tab, "sleep 300\n")
        box: dict = {}

        def in_front() -> bool:
            box["job"] = job_in_front(shell)
            return box["job"] is not None

        self.client._wait(in_front, 15.0, f"a job in front of shell {shell}")
        self.client.agent_report(
            tab, "claude", "claim", session_id=uuid.uuid4().hex, lifecycle="working"
        )
        listed = self.wait(tab, "the job's stamp", lambda t: "process" in t)
        process = listed["process"]
        assert int(process["pid"]) == box["job"], (process, box["job"])
        assert (process["state"], process["agent"]) == ("running", "claude"), process
        return process


@pytest.fixture(
    params=[
        pytest.param("session", marks=pytest.mark.session_daemon),
        "iced",
    ]
)
def lane(request):
    if request.param == "session":
        yield from _session_lane()
    else:
        yield from _iced_lane(request.getfixturevalue("roost"), request.getfixturevalue("project"))


def _session_lane():
    env = sessionlib.make_env()
    try:
        launch = env.start_daemonized()
        assert launch.returncode == 0, f"start failed: {launch.stdout!r} / {launch.stderr!r}"
        assert launch.verdict.kind == "ready", launch.verdict
        env.wait_answering()
        with env.client() as client:
            project = int(client.list()[0]["id"])

            def open_tab(argv: list[str]) -> int:
                return client.open_tab(
                    project, cwd=str(env.launch_cwd), cols=COLS, rows=ROWS, argv=argv
                )

            yield Lane("session", client, open_tab)
            client.call("session.stop")
    finally:
        env.teardown()


def _iced_lane(roost: Roost, project: int):
    if roost.identify().get("local_backend", "in-process") != "in-process":
        pytest.skip("an in-process tab needs a UI on `local-backend = in-process`")

    def open_tab(argv: list[str]) -> int:
        tab = roost.open_tab(project, cwd="/tmp", cols=COLS, rows=ROWS, argv=argv)
        wait_tab_attached(roost, tab)
        return tab

    yield Lane("iced", roost, open_tab)


def ps_field(pid: int, field: str) -> str:
    out = subprocess.run(
        ["ps", "-o", f"{field}=", "-p", str(pid)], capture_output=True, text=True
    )
    return out.stdout.strip()


def job_in_front(shell: int) -> int | None:
    """The leader of the group in front of `shell`'s terminal, when that is
    not the shell: the shell's own `tpgid`."""
    tpgid = ps_field(shell, "tpgid")
    if not tpgid.lstrip("-").isdigit():
        return None
    tpgid = int(tpgid)
    return tpgid if tpgid > 0 and tpgid != shell else None


def agent_row(lane: Lane, tab: int) -> dict | None:
    """The tab's Agents palette row: the iced lane's only."""
    return lane.client.palette_row("agents", f"agent:{tab}")


def test_a_killed_agent_leaves_no_owner_and_no_row(lane):
    """E1: the stamped agent is SIGKILLed. Its shell takes the terminal
    back and prints `D`; with the process gone and the shell in front, the
    owner is released, and with it the Agents row."""
    tab, shell = lane.bash_tab()
    process = lane.stamped_job(tab, shell)
    if lane.name == "iced":
        assert agent_row(lane, tab) is not None, "the precondition: a row"

    os.kill(int(process["pid"]), signal.SIGKILL)
    listed = lane.wait(tab, "the owner's release", lambda t: t.get("ownership") is None)
    assert listed["hook_active"] is False, listed
    assert "process" not in listed, listed
    assert listed["state"] == "none", listed
    if lane.name == "iced":
        lane.client._wait(
            lambda: agent_row(lane, tab) is None, 5.0, f"tab {tab}'s Agents row to go"
        )


def test_a_suspended_agent_keeps_its_owner_and_its_row(lane):
    """E2: Ctrl-Z stops the stamped agent and the shell prints `D`, but a
    stopped agent is alive: the owner and the row stay, `Tab.process` says
    suspended, and the tab shows what a prompt shows. Once `fg` runs it
    again, the next sample brings back what it was doing."""
    tab, shell = lane.bash_tab()
    process = lane.stamped_job(tab, shell)
    pid = int(process["pid"])

    lane.client.send(tab, b"\x1a")
    listed = lane.wait(
        tab,
        "the suspended agent",
        lambda t: t.get("process", {}).get("state") == "suspended",
    )
    assert (listed.get("ownership") or {}).get("source") == "claude", listed
    assert listed["hook_active"] is True, listed
    assert listed["agent_lifecycle"] == "inactive", listed
    assert listed["state"] == "none", listed
    if lane.name == "iced":
        assert agent_row(lane, tab) is not None, "the row survives a suspend"

    lane.client.send(tab, "fg\n")
    lane.client._wait(
        lambda: not ps_field(pid, "stat").startswith("T"), 15.0, f"job {pid} to run again"
    )
    # A focus asks for a sample; bash < 4.4 (macOS's /bin/bash) prints no
    # `C` for the `fg` that would ask for one itself.
    lane.client.focus(tab)
    listed = lane.wait(
        tab, "the agent's lifecycle back", lambda t: t["agent_lifecycle"] == "working"
    )
    assert listed["process"]["state"] == "running", listed
    assert listed["state"] == "running", listed
    if lane.name == "iced":
        assert agent_row(lane, tab) is not None

    os.kill(pid, signal.SIGKILL)
    lane.wait(tab, "the owner's release", lambda t: t.get("ownership") is None)


def test_a_command_tab_takes_its_agent_state_with_it(lane):
    """E7: a command tab whose own child is the agent needs no stamp. Its
    exit closes the tab, and the row and the state go with it."""
    tab = lane.open_tab(
        [
            "/bin/sh",
            "-c",
            r"printf 'AGENT_PID_%s_\n' $$; "
            r"printf '\033]7501;state=working:app=fakeagent\033\\'; "
            "exec sleep 300",
        ]
    )
    listed = lane.wait(
        tab,
        "the agent's working record",
        lambda t: (t.get("program_status") or {}).get("effective_lifecycle") == "working",
    )
    assert listed["state"] == "running", listed
    box: dict = {}

    def printed() -> bool:
        box["pid"] = re.search(r"AGENT_PID_(\d+)_", lane.client.dump_text(tab))
        return box["pid"] is not None

    lane.client._wait(printed, 15.0, f"tab {tab}'s agent pid")
    if lane.name == "iced":
        assert agent_row(lane, tab) is not None, "the precondition: a row"

    os.kill(int(box["pid"].group(1)), signal.SIGKILL)
    lane.client.wait_gone(tab, timeout=15.0)
    if lane.name == "iced":
        assert agent_row(lane, tab) is None
