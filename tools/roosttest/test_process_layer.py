"""The process layer (#621, #519) end to end, on both terminals that can
be a tab's authoritative one: a session tab's, inside a real
`roost-session` (marked `session_daemon`, so it runs in `make
e2e-session`), and an in-process tab's, inside the harness UI (unmarked,
so it runs in the iced lanes).

The agents are real processes. A tab is a bare bash whose
`PROMPT_COMMAND` prints the `OSC 133 ; D` Roost's own integration prints,
or a `/bin/sh` that prints no marks at all, and the agent is a job it runs
in front. The hook claim comes from the test, and an agent's OSC 7501
records come from `tab.feed_pty_bytes` while its job is in front, so the
stamp is the job's; `Tab.process` names the job, so the test kills
exactly that process.

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
from test_program_status import osc7501
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
        pid."""
        tab = self.open_tab(BARE_SHELL_ARGV)
        shell = self.shell_pid(tab, MARKS + "; ")
        self.wait(tab, "the first D mark", lambda t: t["shell_state"] == "at_prompt")
        return tab, shell

    def sh_tab(self) -> tuple[int, int]:
        """A `/bin/sh` tab, which prints no prompt marks at all (dash on
        Linux, bash in POSIX mode on macOS; Roost wires no integration into
        either), and its shell's pid."""
        tab = self.open_tab(["/bin/sh"])
        return tab, self.shell_pid(tab, "")

    def shell_pid(self, tab: int, before: str) -> int:
        """Run `before` and print the shell's pid. The pid goes through
        `%s`, so only the output has digits after the marker; the echoed
        command line has `%s`."""
        self.client.run(tab, before + r"""printf 'SHELL_PID_%s_\n' "$$" """)
        box: dict = {}

        def printed() -> bool:
            box["pid"] = PID.search(self.client.dump_text(tab))
            return box["pid"] is not None

        self.client._wait(printed, 15.0, f"tab {tab}'s shell pid")
        return int(box["pid"].group(1))

    def in_front(self, tab: int, shell: int, command: str) -> int:
        """Run `command` as a job in front of `shell`, and answer the
        leader of its group once it has the terminal: a stamp taken any
        earlier would find the shell in front."""
        self.client.send(tab, command + "\n")
        box: dict = {}

        def in_front() -> bool:
            box["job"] = job_in_front(shell)
            return box["job"] is not None

        self.client._wait(in_front, 15.0, f"a job in front of shell {shell}")
        return box["job"]

    def stamped_job(self, tab: int, shell: int, command: str = "sleep 300") -> dict:
        """`command` in front of `shell`, claimed by an agent and stamped:
        `Tab.process`."""
        job = self.in_front(tab, shell, command)
        self.client.agent_report(
            tab, "claude", "claim", session_id=uuid.uuid4().hex, lifecycle="working"
        )
        listed = self.wait(tab, "the job's stamp", lambda t: "process" in t)
        process = listed["process"]
        assert int(process["pid"]) == job, (process, job)
        assert (process["state"], process["agent"]) == ("running", "claude"), process
        return process

    def stamped_record(self, tab: int, shell: int, app: str = "codex") -> dict:
        """A `sleep` job in front of `shell` reporting OSC 7501 `working`
        as `app`, with no hooks: the records decide the tab, and are
        stamped with the job. `Tab.process`."""
        job = self.in_front(tab, shell, "sleep 300")
        self.client.tab_feed_pty_bytes(tab, osc7501(state="working", app=app))
        listed = self.wait(tab, "the record's stamp", lambda t: "process" in t)
        process = listed["process"]
        assert int(process["pid"]) == job, (process, job)
        assert (process["state"], process["agent"]) == ("running", app), process
        assert listed["state"] == "running", listed
        assert "ownership" not in listed, listed
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
        launch = env.start_daemonized(ROOST_TEST_MODE="1")
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


def group_members(pgid: int) -> list[int]:
    out = subprocess.run(["pgrep", "-g", str(pgid)], capture_output=True, text=True)
    return [int(pid) for pid in out.stdout.split()]


def children_of(pid: int) -> list[int]:
    out = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True)
    return [int(child) for child in out.stdout.split()]


def is_gone(pid: int) -> bool:
    """No process at `pid`, or a zombie."""
    stat = ps_field(pid, "stat")
    return stat == "" or stat.startswith("Z")


def kill_group(pgid: int) -> None:
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        pass


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


def test_without_marks_a_killed_agents_record_ends_and_unmutes_the_tab(lane):
    """E3 (#519): in a `/bin/sh` tab no prompt mark ever says a program
    ended. A 7501 agent's `working` record, stamped with its job, mutes raw
    notifications while it lives; once the job is SIGKILLed, the tick finds
    it gone with the shell back in front and ends the record — the tab
    leaves `running`, `process` goes, and the next program's raw OSC 9
    raises a notification again."""
    tab, shell = lane.sh_tab()
    lane.open_tab(BARE_SHELL_ARGV)  # the active one, so `tab` is not watched
    process = lane.stamped_record(tab, shell)
    lane.client.tab_feed_pty_bytes(tab, b"\x1b]9;muted\x07")
    lane.client.dump(tab)
    assert lane.client.tab(tab)["has_notification"] is False, "the precondition: muted"

    os.kill(int(process["pid"]), signal.SIGKILL)
    listed = lane.wait(tab, "the tick ending the record", lambda t: "process" not in t)
    assert listed["state"] == "none", listed
    assert "program_status" not in listed, listed

    lane.client.send(tab, "printf '\\033]9;after\\007'\n")
    lane.wait(tab, "the next program's notification", lambda t: t["has_notification"])


def test_a_suspended_7501_agent_reads_suspended_and_resumes(lane):
    """E4: Ctrl-Z stops a 7501 agent mid-turn. The shell's `D` sets its
    record aside rather than dropping it: the tab shows what a prompt
    shows (`none`), `process` reads `suspended`, and the Agents row stays,
    reading Suspended. After `fg` the tick finds it running again and the
    record comes back — no `C` mark or focus needed."""
    tab, shell = lane.bash_tab()
    process = lane.stamped_record(tab, shell)
    pid = int(process["pid"])

    lane.client.send(tab, b"\x1a")
    listed = lane.wait(
        tab,
        "the suspended agent",
        lambda t: t.get("process", {}).get("state") == "suspended",
    )
    assert listed["state"] == "none", listed
    assert listed["process"]["agent"] == "codex", listed
    if lane.name == "iced":
        row = lane.client.wait_palette_row_field(
            "agents", f"agent:{tab}", "agent.status_text", "Suspended"
        )
        assert row["agent"]["agent"] == "Codex", row

    lane.client.send(tab, "fg\n")
    listed = lane.wait(
        tab,
        "the record back",
        lambda t: t["state"] == "running" and t.get("process", {}).get("state") == "running",
    )
    assert listed["program_status"]["effective_lifecycle"] == "working", listed

    os.kill(pid, signal.SIGKILL)
    lane.wait(tab, "the end", lambda t: "process" not in t and t["state"] == "none")


def test_a_backgrounded_agent_keeps_working_until_it_is_killed(lane):
    """E5: Ctrl-Z, then `bg`: the agent runs on in the background, so it
    keeps the state it had (Charlie's ruling), though the shell has the
    terminal. `kill %1` ends it."""
    tab, shell = lane.bash_tab()
    lane.stamped_record(tab, shell)

    lane.client.send(tab, b"\x1a")
    lane.wait(
        tab, "the suspended agent", lambda t: t.get("process", {}).get("state") == "suspended"
    )
    lane.client.send(tab, "bg\n")
    listed = lane.wait(
        tab,
        "the agent working in the background",
        lambda t: t["state"] == "running" and t.get("process", {}).get("state") == "running",
    )
    assert job_in_front(shell) is None, "the shell has the terminal"
    assert listed["process"]["agent"] == "codex", listed

    lane.client.send(tab, "kill %1\n")
    listed = lane.wait(tab, "the end", lambda t: "process" not in t)
    assert listed["state"] == "none", listed


def test_a_pipeline_keeps_its_owner_and_a_waiting_wrapper_ends_with_its_agent(lane):
    """E6: what is stamped is the leader of the group in front, and only
    a gone leader whose group has left the front ends anything.

    * A pipeline whose leader dies first keeps its owner while the rest of
      it has the terminal. The proof is a fence: a `/bin/sh` tab (no
      marks) opened after it, whose stamped job is killed once the leader
      is gone. Only the tick can release that one, and the tick reads and
      applies the stamped tabs in id order, the pipeline's first.
    * A wrapper that waits for its agent (`sh -c 'sleep; exit 0'`) is the
      stamped leader: killing the agent ends the owner once the wrapper
      exits and the shell takes the terminal back."""
    pipe, pipe_shell = lane.bash_tab()
    fence, fence_shell = lane.sh_tab()
    assert pipe < fence, "the precondition: tab order is id order"
    leader = int(lane.stamped_job(pipe, pipe_shell, "sleep 300 | sleep 301")["pid"])
    try:
        fenced = int(lane.stamped_job(fence, fence_shell)["pid"])
        lane.client._wait(lambda: len(group_members(leader)) == 2, 15.0, "both pipeline members")
        mate = next(pid for pid in group_members(leader) if pid != leader)

        os.kill(leader, signal.SIGKILL)
        lane.client._wait(lambda: is_gone(leader), 15.0, f"leader {leader} gone")
        os.kill(fenced, signal.SIGKILL)
        lane.wait(fence, "the tick releasing the fence", lambda t: t.get("ownership") is None)
        listed = lane.client.tab(pipe)
        assert (listed.get("ownership") or {}).get("source") == "claude", listed
        assert int(listed["process"]["pid"]) == leader, listed

        os.kill(mate, signal.SIGKILL)
        lane.wait(pipe, "the pipeline's release", lambda t: t.get("ownership") is None)
    finally:
        kill_group(leader)

    tab, shell = lane.bash_tab()
    wrapper = int(lane.stamped_job(tab, shell, "sh -c 'sleep 302; exit 0'")["pid"])
    try:
        lane.client._wait(lambda: children_of(wrapper) != [], 15.0, "the wrapper's agent")
        os.kill(children_of(wrapper)[0], signal.SIGKILL)
        listed = lane.wait(tab, "the release", lambda t: t.get("ownership") is None)
        assert "process" not in listed, listed
    finally:
        kill_group(wrapper)
