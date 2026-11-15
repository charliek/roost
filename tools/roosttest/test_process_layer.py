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

The exit-banner cases (E10–E12) run Roost's own shell integration, which
prints `D;<exit status>`, sourced by hand into a bash or zsh that read no
startup files, and read the banners off the tab's event stream.

Condition waits only.
"""

from __future__ import annotations

import errno
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import pytest
import session as sessionlib
import ui
from client import Roost
from eventstream import EventStream
from test_program_status import next_for_tab, osc7501
from util import BARE_SHELL_ARGV, wait_tab_attached

COLS, ROWS = 80, 24
MARKS = r"""PROMPT_COMMAND='printf "\033]133;D\007"'"""
PID = re.compile(r"SHELL_PID_(\d+)_")
INTEGRATION = Path(__file__).resolve().parents[2] / "crates/roost-engine/resources/shell-integration"


@dataclass
class Lane:
    """One authoritative terminal's side of the op set: a client on its
    socket and how to open a tab there."""

    name: str
    client: Roost
    events_socket: str
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

    def integrated_tab(self, shell: str) -> tuple[int, int]:
        """A bash or zsh tab with Roost's own shell integration sourced, so
        every prompt prints `D;<exit status>`, and its shell's pid. The
        shell read no startup files: the integration's are the only prompt
        hooks."""
        argv = BARE_SHELL_ARGV if shell == "bash" else [shell_binary(shell), "-f"]
        tab = self.open_tab(argv)
        pid = self.shell_pid(tab, f". '{INTEGRATION / f'roost.{shell}'}'; ")
        self.wait(tab, "the integration's first D mark", lambda t: t["shell_state"] == "at_prompt")
        return tab, pid

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

    def stamped_record(
        self, tab: int, shell: int, app: str = "codex", command: str = "sleep 300"
    ) -> dict:
        """`command` in front of `shell` reporting OSC 7501 `working` as
        `app`, with no hooks: the records decide the tab, and are stamped
        with the job. `Tab.process`."""
        job = self.in_front(tab, shell, command)
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

            yield Lane("session", client, str(env.socket), open_tab)
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

    yield Lane("iced", roost, str(ui.socket_path("iced")), open_tab)


def shell_binary(name: str) -> str:
    """`name` on PATH. Missing, it skips locally and fails under CI."""
    path = shutil.which(name)
    if path is None:
        message = f"{name} not found; needed to run Roost's {name} integration"
        if os.environ.get("CI"):
            pytest.fail(message)
        pytest.skip(message)
    return path


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


def tmux_client_in_front(shell: int) -> bool:
    job = job_in_front(shell)
    return job is not None and ps_field(job, "comm").startswith("tmux")


def test_an_agent_behind_tmux_keeps_its_owner_across_a_detach(lane):
    """An agent inside a real tmux claims while the tmux client has the
    terminal, so nothing is stamped. A detach ends the client and the
    shell takes the terminal back with the agent alive behind the tmux
    server: the shell's `D` does what it always did, and the owner stays,
    so what the agent reports after a reattach lands.

    A bash tab opened after it, with a stamped job, is the fence that the
    claim was read before the detach: the sampler applies what it read in
    tab id order. Linux only, and tmux is optional: CI installs none, and
    the PTY test with a process named `tmux` covers the rule there."""
    if not sys.platform.startswith("linux"):
        pytest.skip("the real-tmux case runs on Linux")
    tmux = shutil.which("tmux")
    if tmux is None:
        pytest.skip("tmux not found")
    tab, shell = lane.integrated_tab("bash")
    fence, fence_shell = lane.bash_tab()
    assert tab < fence, "the precondition: tab order is id order"
    home = tempfile.mkdtemp(prefix="rtmux-", dir="/tmp")
    server = [tmux, "-S", os.path.join(home, "s")]
    session = uuid.uuid4().hex
    fenced = None
    try:
        lane.client.send(
            tab, f"'{tmux}' -S '{home}/s' -f /dev/null new-session -s agent 'sleep 600'\n"
        )
        lane.client._wait(lambda: tmux_client_in_front(shell), 15.0, "the tmux client in front")
        lane.client.agent_report(tab, "claude", "claim", session_id=session, lifecycle="working")
        fenced = int(lane.stamped_job(fence, fence_shell)["pid"])
        listed = lane.client.tab(tab)
        assert "process" not in listed, listed
        assert listed["ownership"]["session_id"] == session, listed

        subprocess.run([*server, "detach-client", "-s", "agent"], check=True, timeout=15)
        listed = lane.wait(
            tab, "the shell's D after the detach", lambda t: t["shell_state"] == "at_prompt"
        )
        assert (listed.get("ownership") or {}).get("session_id") == session, listed
        assert listed["agent_lifecycle"] == "inactive", listed

        lane.client.send(tab, f"'{tmux}' -S '{home}/s' attach -t agent\n")
        lane.client._wait(lambda: tmux_client_in_front(shell), 15.0, "the tmux client back")
        reply = lane.client.agent_report(
            tab, "claude", "preserve", session_id=session, lifecycle="working"
        )
        assert reply["accepted"] is True, reply
        assert reply["tab"]["agent_lifecycle"] == "working", reply
    finally:
        subprocess.run([*server, "kill-server"], capture_output=True, timeout=15)
        shutil.rmtree(home, ignore_errors=True)
        if fenced is not None:
            kill_group(fenced)


SLEEPER = "#include <unistd.h>\nint main(void) { for (;;) pause(); }\n"


def named_agent(directory: str, name: str) -> str:
    """A command running something Roost names by its process, made in
    `directory`: a sleeper called `name`, built with `cc`. A copied `sleep`
    won't do: macOS kills a copied system binary, and a multicall coreutils
    (uutils, Ubuntu 26.04) picks its tool by the name it ran as. Without a
    compiler, Linux takes a shebang script, which it names after the file;
    macOS takes `node` running `<name>/index.js`, as cursor-agent's own
    launcher runs its script."""
    agent = os.path.join(directory, name)
    cc = shutil.which("cc")
    if cc is not None:
        subprocess.run([cc, "-x", "c", "-o", agent, "-"], input=SLEEPER, text=True, check=True)
        return f"'{agent}'"
    if sys.platform.startswith("linux"):
        with open(agent, "w") as script:
            script.write("#!/bin/sh\nwhile :; do sleep 300; done\n")
        os.chmod(agent, 0o755)
        return f"'{agent}'"
    node = shutil.which("node")
    if node is None:
        message = "neither cc nor node found; macOS names a process after its binary"
        if os.environ.get("CI"):
            pytest.fail(message)
        pytest.skip(message)
    script = os.path.join(directory, name, "index.js")
    os.makedirs(os.path.dirname(script))
    with open(script, "w") as handle:
        handle.write("setTimeout(() => {}, 300000);\n")
    return f"'{node}' '{script}'"


def exec_d(pid: int) -> bool:
    """`pid` runs its own program, not the shell it was forked from."""
    return os.path.basename(ps_field(pid, "comm")) not in ("", "bash", "sh")


def test_an_agent_named_by_its_process_alone_shows_running_and_goes_with_it(lane, tmp_path):
    """E13: an agent no hook claims and no record names — cursor-agent
    with no hooks, Codex at its sign-in screen — is named by its process
    once someone looks at its tab: here, the focus moving off it. The tab
    gains `process` with that name and an Agents row reading Running, with
    no owner made up; once the process is killed and the shell has the
    terminal back, both go."""
    command = named_agent(str(tmp_path), "cursor-agent")
    tab, shell = lane.bash_tab()
    other = lane.open_tab(BARE_SHELL_ARGV)
    lane.client.focus(tab)
    job = lane.in_front(tab, shell, command)
    lane.client._wait(lambda: exec_d(job), 15.0, f"job {job} to exec")

    lane.client.focus(other)
    listed = lane.wait(tab, "the agent named", lambda t: "process" in t)
    process = listed["process"]
    assert (process["agent"], process["state"], int(process["pid"])) == (
        "cursor",
        "running",
        job,
    ), process
    assert listed.get("ownership") is None, listed
    if lane.name == "iced":
        row = lane.client.wait_palette_row_field(
            "agents", f"agent:{tab}", "agent.status_text", "Running"
        )
        assert row["agent"]["agent"] == "Cursor", row
        assert row["agent"]["effective_lifecycle"] == "inactive", row

    os.kill(job, signal.SIGKILL)
    listed = lane.wait(tab, "the agent gone", lambda t: "process" not in t)
    assert listed.get("ownership") is None, listed
    if lane.name == "iced":
        lane.client._wait(
            lambda: agent_row(lane, tab) is None, 5.0, f"tab {tab}'s Agents row to go"
        )


def bash_prints_c() -> bool:
    """Whether the tabs' bash sends `C`: `PS0` needs bash 4.4."""
    out = subprocess.run(
        [BARE_SHELL_ARGV[0], "-c", 'echo "${BASH_VERSINFO[0]} ${BASH_VERSINFO[1]}"'],
        capture_output=True,
        text=True,
    ).stdout.split()
    return tuple(int(part) for part in out) >= (4, 4)


@pytest.mark.parametrize("shell", ["bash", "zsh"])
def test_an_agent_started_at_the_prompt_is_named_with_no_focus_change(lane, shell, tmp_path):
    """E13 at a command's start: the shell's `C` asks for a look, so an
    agent no hook or record names is named while its tab keeps the focus.
    The shell prints the `C` before it forks the command, and on macOS
    that look nearly always finds the shell; the next tick looks again."""
    if shell == "bash" and not bash_prints_c():
        pytest.skip("this bash is older than 4.4 and prints no C (macOS's /bin/bash)")
    command = named_agent(str(tmp_path), "cursor-agent")
    tab, shell_pid = lane.integrated_tab(shell)
    lane.client.focus(tab)
    job = lane.in_front(tab, shell_pid, command)

    listed = lane.wait(tab, "the agent named", lambda t: "process" in t)
    process = listed["process"]
    assert (process["agent"], int(process["pid"])) == ("cursor", job), process
    assert listed.get("ownership") is None, listed

    os.kill(job, signal.SIGKILL)
    lane.wait(tab, "the agent gone", lambda t: "process" not in t)


def test_a_7501_agent_that_clears_as_it_stops_reads_suspended_by_its_process(lane, tmp_path):
    """Codex clears its own OSC 7501 status as it stops, so a Ctrl-Z
    leaves its records nothing to set aside; its process's name still
    names it. Once the shell's `D` has the terminal back, the read it asks
    for finds the process stopped and the tab reads `codex` suspended,
    with the Agents row Suspended; after `fg` the tick finds it running,
    and once it is killed, `process` goes."""
    tab, shell = lane.bash_tab()
    lane.open_tab(BARE_SHELL_ARGV)  # the active one, so `tab` is not watched
    job = lane.in_front(tab, shell, named_agent(str(tmp_path), "codex"))
    lane.client._wait(lambda: exec_d(job), 15.0, f"job {job} to exec")
    lane.client.tab_feed_pty_bytes(tab, osc7501(state="working", app="codex"))
    listed = lane.wait(tab, "the record's stamp", lambda t: "process" in t)
    assert int(listed["process"]["pid"]) == job, listed

    lane.client.tab_feed_pty_bytes(tab, osc7501(state="clear", app="codex"))
    lane.client.send(tab, b"\x1a")
    listed = lane.wait(
        tab,
        "the agent suspended, named by its process",
        lambda t: t.get("process", {}).get("state") == "suspended",
    )
    assert (listed["process"]["agent"], int(listed["process"]["pid"])) == ("codex", job), listed
    assert listed["state"] == "none", listed
    assert "program_status" not in listed, listed
    if lane.name == "iced":
        row = lane.client.wait_palette_row_field(
            "agents", f"agent:{tab}", "agent.status_text", "Suspended"
        )
        assert row["agent"]["agent"] == "Codex", row

    lane.client.send(tab, "fg\n")
    lane.wait(
        tab,
        "the agent running again",
        lambda t: t.get("process", {}).get("state") == "running",
    )

    os.kill(job, signal.SIGKILL)
    lane.wait(tab, "the agent gone", lambda t: "process" not in t)


# ---------------------------------------------------------------------------
# Exit codes and the exit banner
# ---------------------------------------------------------------------------


@pytest.fixture
def fifo():
    """A FIFO a job blocks reading until the test answers it."""
    directory = tempfile.mkdtemp(prefix="rpl-")
    path = os.path.join(directory, "code")
    os.mkfifo(path)
    try:
        yield path
    finally:
        os.unlink(path)
        os.rmdir(directory)


def exits_with_what_it_reads(fifo: str) -> str:
    """A job that exits with the code the test writes to `fifo`."""
    return f"""sh -c 'read code < {fifo}; exit "$code"'"""


def answer(lane: Lane, fifo: str, code: int) -> None:
    """Write `code` to the job blocked reading `fifo`, once it has the
    FIFO open."""

    def written() -> bool:
        try:
            fd = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as error:
            if error.errno == errno.ENXIO:
                return False
            raise
        with os.fdopen(fd, "w") as writer:
            writer.write(f"{code}\n")
        return True

    lane.client._wait(written, 15.0, f"the job reading {fifo}")


def ended(listed: dict) -> bool:
    """The stamped agent is gone and the shell is back at its prompt: its
    `D`, and with it any banner the end owed, has been applied."""
    return (
        "process" not in listed
        and listed.get("ownership") is None
        and listed["state"] == "none"
        and listed["shell_state"] == "at_prompt"
    )


def banners_until(
    lane: Lane, stream: EventStream, tab: int, fence: int, sentinel: str
) -> tuple[list[tuple[str, str]], int]:
    """The banners `tab` raised since `fence`, up to a raw OSC 9 `sentinel`
    the tab's shell prints now, and the fence after it. Nothing mutes the
    sentinel once the agent is gone or suspended."""
    lane.client.send(tab, f"printf '\\033]9;{sentinel}\\007'\n")
    banners = []
    while True:
        banner, fence, _ = next_for_tab(stream, "notification.fired", tab, fence)
        if sentinel in (banner["title"], banner["body"]):
            return banners, fence
        banners.append((banner["title"], banner["body"]))


@pytest.mark.parametrize("shell", ["bash", "zsh"])
def test_an_agent_that_exits_with_an_error_raises_one_banner(lane, shell, fifo):
    """E10: an agent's job exits by itself, first with 0 and then with 3,
    in a tab nobody is watching. The integration's `D` reports each code,
    and only the failure raises a banner: `exited (3)`, under the agent's
    name. In bash the agent is a hook owner (Claude), in zsh a 7501
    record (Codex)."""
    tab, pid = lane.integrated_tab(shell)
    lane.open_tab(BARE_SHELL_ARGV)  # the active one, so `tab` is not watched
    with EventStream(lane.events_socket) as stream:
        fence = stream.subscribe()
        for code in (0, 3):
            job = exits_with_what_it_reads(fifo)
            if shell == "bash":
                lane.stamped_job(tab, pid, job)
            else:
                lane.stamped_record(tab, pid, "codex", job)
            answer(lane, fifo, code)
            lane.wait(tab, f"the end of the job that exited {code}", ended)
        banners, _ = banners_until(lane, stream, tab, fence, "e10-done")
    name = "Claude Code" if shell == "bash" else "Codex"
    assert banners == [(name, "exited (3)")], banners


def test_a_killed_agent_banners_once_and_a_suspended_one_never(lane):
    """E11: Ctrl-Z stops the agent, and its shell's `D` reports 128 +
    `SIGTSTP`: a stop, not an exit. Killed while stopped (`kill -9 %1`),
    the agent ends with no banner, since no `D` reported how it died. An
    agent stopped, resumed with `fg` and then SIGKILLed ends with the one
    banner its `D`'s 137 calls for, `killed (SIGKILL)`."""
    tab, pid = lane.integrated_tab("bash")
    lane.open_tab(BARE_SHELL_ARGV)  # the active one, so `tab` is not watched

    def suspend() -> None:
        lane.client.send(tab, b"\x1a")
        lane.wait(
            tab,
            "the suspended agent",
            lambda t: t.get("process", {}).get("state") == "suspended",
        )

    with EventStream(lane.events_socket) as stream:
        fence = stream.subscribe()
        lane.stamped_job(tab, pid)
        suspend()
        lane.client.send(tab, "kill -9 %1\n")
        lane.wait(tab, "the end of the stopped agent", ended)
        stopped, fence = banners_until(lane, stream, tab, fence, "e11-stopped")
        assert stopped == [], stopped

        agent = int(lane.stamped_job(tab, pid)["pid"])
        suspend()
        lane.client.send(tab, "fg\n")
        lane.wait(tab, "the agent working again", lambda t: t["agent_lifecycle"] == "working")
        os.kill(agent, signal.SIGKILL)
        lane.wait(tab, "the end of the killed agent", ended)
        killed, _ = banners_until(lane, stream, tab, fence, "e11-killed")
    assert killed == [("Claude Code", "killed (SIGKILL)")], killed


def test_without_exit_codes_an_end_raises_no_banner(lane):
    """E12: `/bin/sh` prints no marks, so no code says how the agent
    ended. The tick still ends it, and raises nothing: without a code,
    Roost cannot tell a crash from a normal exit."""
    tab, shell = lane.sh_tab()
    lane.open_tab(BARE_SHELL_ARGV)  # the active one, so `tab` is not watched
    with EventStream(lane.events_socket) as stream:
        fence = stream.subscribe()
        agent = int(lane.stamped_job(tab, shell)["pid"])
        os.kill(agent, signal.SIGKILL)
        lane.wait(
            tab,
            "the tick's end",
            lambda t: t.get("ownership") is None and "process" not in t,
        )
        banners, _ = banners_until(lane, stream, tab, fence, "e12-done")
    assert banners == [], banners
