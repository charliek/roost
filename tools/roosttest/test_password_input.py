"""`Tab.password_input`: a tab at a password prompt, on the op set (plan 074 §D2, #594).

A tab reads `password_input: true` while its PTY's line discipline is in
canonical mode with echo off (`ICANON && !ECHO`), which the process that
owns the PTY samples off the master every 200 ms. Each case runs a helper
that sets exactly that, prints a marker, and blocks on a line; answering
the prompt restores echo, and the flag clears.

The helper sets the modes itself rather than running `stty -echo` at an
interactive shell: bash's readline and zsh's ZLE run the terminal raw
(canonical mode off) while they edit a line, so `stty -echo` at a prompt
never reads as one.

Unmarked cases drive the harness UI's in-process tabs (and skip on the
Swift target, which never reports the field); `session_daemon` cases drive
a headless `roost-session` — the Makefile's `SESSION_E2E_TESTS` says how
the two lanes split the module. The `local-backend = session` UI restart
is in `test_local_backend.py`.

Condition waits only.
"""

from __future__ import annotations

import shutil
import sys

import pytest
import session as sessionlib
from client import Roost
from eventstream import EventStream
from test_session import event_names, first_project, started

#: Line mode with echo off until a line arrives, then echo back on and a
#: second line read with it — so the tab stays alive, and out of a prompt,
#: for the "cleared" half of every case. The markers are concatenated so
#: nothing but the helper's own output can match them.
PROMPT_HELPER = r"""
import sys, termios
fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
prompt = termios.tcgetattr(fd)
prompt[3] = (prompt[3] | termios.ICANON) & ~termios.ECHO
termios.tcsetattr(fd, termios.TCSANOW, prompt)
print("PW_" + "READY", flush=True)
sys.stdin.readline()
termios.tcsetattr(fd, termios.TCSANOW, saved)
print("PW_" + "DONE", flush=True)
sys.stdin.readline()
"""

READY = "PW_READY"
DONE = "PW_DONE"
#: Typed at the prompt; echo is off, so it must never reach the screen.
SECRET = "hunter2-password-input"


def prompt_argv() -> list[str]:
    return [sys.executable, "-c", PROMPT_HELPER]


def answer_and_clear(roost: Roost, tab: int) -> None:
    """Type the secret at the prompt, then see the flag go down with echo
    back on — and the secret nowhere on screen."""
    roost.send(tab, SECRET + "\n")
    roost.wait_text(tab, DONE)
    roost.wait_password_input(tab, False)
    assert SECRET not in roost.dump_text(tab), "echo was off, yet the secret was drawn"


# ---------------------------------------------------------------------------
# In-process: the harness UI's own tabs
# ---------------------------------------------------------------------------


@pytest.fixture
def iced(target):
    if target != "iced":
        pytest.skip("the Swift app never reports password_input")


def test_a_prompt_with_echo_off_reads_true_until_it_is_answered(iced, roost, project):
    tab = roost.open_tab(project, cwd="/tmp", argv=prompt_argv())
    try:
        roost.wait_text(tab, READY)
        roost.wait_password_input(tab, True)
        answer_and_clear(roost, tab)
    finally:
        roost.close_tab(tab)


def test_read_s_in_bash_reads_true(iced, roost, project):
    """`read -s` is bash's own silent read: it clears echo and leaves
    canonical mode on, so it is a prompt — outside a line editor, which
    is what `-c` runs it in."""
    bash = shutil.which("bash")
    if bash is None:
        pytest.skip("no bash on PATH")
    script = (
        "printf 'PW_%s\\n' READY; read -s line; printf 'PW_%s\\n' DONE; read line"
    )
    tab = roost.open_tab(
        project, cwd="/tmp", argv=[bash, "--norc", "--noprofile", "-c", script]
    )
    try:
        roost.wait_text(tab, READY)
        roost.wait_password_input(tab, True)
        answer_and_clear(roost, tab)
    finally:
        roost.close_tab(tab)


# ---------------------------------------------------------------------------
# Headless: a roost-session daemon and no UI
# ---------------------------------------------------------------------------


@pytest.fixture
def env():
    """A throwaway session profile, torn down (processes first, then the
    directory) whatever the test did to it."""
    made = sessionlib.make_env()
    try:
        started(made)
        yield made
    finally:
        made.teardown()


def prompt_tab(client: Roost) -> int:
    """The helper in a tab of the session's seeded project, at its prompt."""
    tab = client.open_tab(first_project(client), cwd="/tmp", argv=prompt_argv())
    client.wait_text(tab, READY, timeout=30.0)
    return tab


def raised_on(stream: EventStream, tab: int) -> list[dict]:
    """Wait for the stream to announce `tab`'s prompt, and answer the
    batches read to get there. A precondition read off the stream rather
    than `tab.list`, so the snapshot a reattaching client reads is the
    first `tab.list` that sees the flag."""
    batches, raised = stream.recv_until("tab.password_input", timeout=10.0)
    assert raised["data"] == {"tab_id": str(tab), "password_input": True}
    return batches


def assert_no_transition_through_a_marker(
    stream: EventStream, client: Roost, tab: int, fence: int
) -> None:
    """Commit a marker title on `tab`, read every batch up to it, and
    find no `tab.password_input` among them — the bound that turns "no
    transition happened" into a read rather than a sleep."""
    marker = "pw-marker"
    client.set_title(tab, marker)
    batches: list[dict] = []
    while True:
        more, changed = stream.recv_until("tab.title_changed", timeout=10.0)
        batches.extend(more)
        if changed["data"].get("title") == marker:
            break
    stream.expect_contiguous(batches, fence)
    assert "tab.password_input" not in event_names(batches), (
        f"the prompt was announced again: {batches}"
    )


@pytest.mark.session_daemon
def test_a_session_reports_the_prompt_on_tab_list_and_the_event_stream(env):
    with env.client() as client, EventStream(env.socket) as stream:
        fence = stream.subscribe()
        tab = prompt_tab(client)
        client.wait_password_input(tab, True)
        stream.expect_contiguous(raised_on(stream, tab), fence)

        answer_and_clear(client, tab)
        _, cleared = stream.recv_until("tab.password_input", timeout=10.0)
        assert cleared["data"] == {"tab_id": str(tab), "password_input": False}
        client.close_tab(tab)


@pytest.mark.session_daemon
def test_a_client_that_attaches_mid_prompt_reads_it_off_tab_list(env):
    """The reattach half: the flag is the tab's state, so a client that
    connects while the prompt is already up reads it off `tab.list` — with
    no new transition to tell it. The marker commit is what bounds "no
    transition": the stream is read up to it, and nothing else may come."""
    with env.client() as first, EventStream(env.socket) as stream:
        stream.subscribe()
        tab = prompt_tab(first)
        raised_on(stream, tab)

    with env.client() as late, EventStream(env.socket) as stream:
        fence = stream.subscribe()
        assert late.password_input(tab), "a client attaching mid-prompt read it as false"
        assert_no_transition_through_a_marker(stream, late, tab, fence)

        answer_and_clear(late, tab)
        late.close_tab(tab)
