"""Program status (OSC 7501) on a session tab, end to end against a real
`roost-session` (plan 082 B-2).

A session tab's terminal is the authoritative one: it answers the
`OSC 7501 ; ?` probe, keeps the tab's program-status records, and the
session puts what they compose to on the wire — `Tab.program_status`,
a composed `Tab.state`, and the same pair on `agent_report.changed`.

Bytes go in through `tab.feed_pty_bytes` (the daemon runs with
`ROOST_TEST_MODE=1`), which takes the same per-chunk path a child's output
does. A `tab.dump` after a feed is the fence: it rides the tab task's
command queue behind the feed, and the workspace commit happens inside the
feed's own turn, so a `tab.list` after the dump reads the result.

Read-boundary independence is pinned in Rust against the real tab task
(`tab_task_test.rs`'s `program_status_does_not_depend_on_read_boundaries`):
the same assertion here would spawn a daemon tab per byte offset.

The Mac app keeps no program-status records, so this module is iced-only,
like `test_agent_hooks_dialog.py`; `e2e-mac` deselects `session_daemon`
too.

Condition waits only.
"""

from __future__ import annotations

import base64

import pytest
import session as sessionlib
from client import Roost
from eventstream import EventStream

pytestmark = pytest.mark.session_daemon

COLS, ROWS = 80, 24

PROBE = b"\x1b]7501;?\x1b\\"
PROBE_REPLY = (
    b"\x1b]7501;?:states=idle,working,done,blocked,error"
    b":kinds=permission,question,auth\x1b\\"
)


@pytest.fixture(autouse=True)
def iced_only(target):
    if target != "iced":
        pytest.skip("the Mac app keeps no program-status records (plan 082 D4.2)")


@pytest.fixture
def env():
    made = sessionlib.make_env()
    try:
        yield made
    finally:
        made.teardown()


def started(env) -> None:
    launch = env.start_daemonized(ROOST_TEST_MODE="1")
    assert launch.returncode == 0, f"start failed: {launch.stdout!r} / {launch.stderr!r}"
    assert launch.verdict.kind == "ready", launch.verdict
    env.wait_answering()


def quiet_tab(client: Roost, env) -> int:
    """A tab parked on a child that never writes, so every byte in its
    stream is one this test put there."""
    project = int(client.list()[0]["id"])
    return client.open_tab(
        project,
        cwd=str(env.launch_cwd),
        cols=COLS,
        rows=ROWS,
        argv=["/bin/sh", "-c", "exec sleep 300"],
    )


def osc7501(**fields: str) -> bytes:
    """One OSC 7501 report. `msg` and `title` are base64 on the wire."""
    parts = []
    for key, value in fields.items():
        if key in ("msg", "title"):
            value = base64.b64encode(value.encode()).decode("ascii")
        parts.append(f"{key}={value}")
    return f"\x1b]7501;{':'.join(parts)}\x1b\\".encode()


def fed(client: Roost, tab: int, data: bytes) -> dict:
    """Feed `data`, fence on the tab task, and return the tab as listed."""
    client.tab_feed_pty_bytes(tab, data)
    client.dump(tab)
    listed = client.tab(tab)
    assert listed is not None, f"tab {tab} is gone"
    return listed


def next_agent_change(stream: EventStream, fence: int) -> tuple[dict, int, list[dict]]:
    """The next `agent_report.changed` envelope's data, the fence to
    continue from, and every batch read on the way."""
    batches, envelope = stream.recv_until("agent_report.changed", timeout=30.0)
    stream.expect_contiguous(batches, fence)
    return envelope["data"], int(batches[-1]["revision"]), batches


def test_the_probe_is_answered_once_byte_for_byte(env):
    started(env)
    with env.client() as client:
        tab = quiet_tab(client, env)
        client.tab_capture_pty_input(tab, drain=True)
        fed(client, tab, PROBE)
        assert client.tab_capture_pty_input(tab, drain=True) == PROBE_REPLY
        listed = client.tab(tab)
        assert "program_status" not in listed, "a probe is not a report"
        client.call("session.stop")


def test_each_state_composes_onto_the_tab(env):
    started(env)
    with env.client() as client:
        tab = quiet_tab(client, env)
        for state, lifecycle, tab_state in [
            ("working", "working", "running"),
            ("blocked", "waiting", "needs_input"),
            ("error", "failed", "needs_input"),
            ("done", "finished", "idle"),
            ("idle", "inactive", "none"),
        ]:
            listed = fed(client, tab, osc7501(state=state, app="claude-code", msg=state))
            status = listed.get("program_status")
            assert status is not None, (state, listed)
            assert status["effective_lifecycle"] == lifecycle, (state, status)
            assert status["decided_by"] == "records", (state, status)
            assert status["record"]["state"] == state, (state, status)
            assert status["record"]["msg"] == state, (state, status)
            assert status["record"]["app"] == "claude-code", (state, status)
            assert listed["state"] == tab_state, (state, listed)
            # The hook layer is untouched: nothing reported through hooks.
            assert listed["agent_lifecycle"] == "inactive", (state, listed)
        client.call("session.stop")


def test_a_blocked_child_outranks_a_working_root(env):
    started(env)
    with env.client() as client:
        tab = quiet_tab(client, env)
        listed = fed(
            client,
            tab,
            osc7501(state="working", app="claude-code", msg="thinking")
            + osc7501(state="blocked", id="build/test", kind="permission", msg="approve Bash"),
        )
        status = listed["program_status"]
        assert status["effective_lifecycle"] == "waiting", status
        assert status["children"] == 1, status
        record = status["record"]
        assert record["id"] == "build/test", record
        assert record["kind"] == "permission", record
        assert record["msg"] == "approve Bash", record
        assert record["app"] == "claude-code", "inherited from the root"
        assert listed["state"] == "needs_input", listed

        # A connection that was never subscribed reads the same summary.
        with env.client() as fresh:
            again = fresh.tab(tab)
            assert again["program_status"] == status, (again, status)
            assert again["state"] == "needs_input", again
        client.call("session.stop")


def test_a_prompt_mark_drops_working(env):
    started(env)
    with env.client() as client:
        tab = quiet_tab(client, env)
        listed = fed(client, tab, osc7501(state="working", app="claude-code"))
        assert listed["state"] == "running", listed
        listed = fed(client, tab, b"\x1b]133;D;0\x07")
        assert "program_status" not in listed, listed
        assert listed["state"] == "none", listed
        client.call("session.stop")


def test_clear_keeps_the_hook_owner(env):
    started(env)
    with env.client() as client:
        tab = quiet_tab(client, env)
        client.agent_report(tab, "claude", "claim", session_id="s1", lifecycle="working")
        listed = fed(client, tab, osc7501(state="blocked", app="claude-code"))
        assert listed["program_status"]["decided_by"] == "records", listed
        assert listed["ownership"]["source"] == "claude", listed

        listed = fed(client, tab, osc7501(state="clear"))
        assert "program_status" not in listed, listed
        assert listed["ownership"]["source"] == "claude", "clear never releases a hook owner"
        assert listed["ownership"]["session_id"] == "s1", listed
        assert listed["hook_active"] is True, listed
        # The hooks decide again, from where they stood.
        assert listed["state"] == "running", listed
        client.call("session.stop")


def test_the_last_clear_sends_an_event_without_program_status(env):
    started(env)
    with env.client() as client:
        tab = quiet_tab(client, env)
        with EventStream(env.socket) as stream:
            fence = stream.subscribe()
            client.tab_feed_pty_bytes(tab, osc7501(state="working", id="a", app="pi"))
            data, fence, _ = next_agent_change(stream, fence)
            assert data["tab_id"] == str(tab), data
            assert data["program_status"]["record"]["id"] == "a", data
            assert data["state"] == "running", data

            client.tab_feed_pty_bytes(tab, osc7501(state="clear", id="a"))
            data, fence, _ = next_agent_change(stream, fence)
            assert "program_status" not in data, data
            assert data["state"] == "none", data
        client.call("session.stop")


def test_a_message_change_alone_produces_an_event(env):
    started(env)
    with env.client() as client:
        tab = quiet_tab(client, env)
        fed(client, tab, osc7501(state="working", app="claude-code", msg="reading"))
        with EventStream(env.socket) as stream:
            fence = stream.subscribe()
            client.tab_feed_pty_bytes(
                tab, osc7501(state="working", app="claude-code", msg="writing")
            )
            # A later change that is not message-only, so a server that
            # stayed silent on the first fails here rather than timing out.
            client.tab_feed_pty_bytes(
                tab, osc7501(state="working", app="claude-code", msg="writing", progress="50")
            )
            data, fence, batches = next_agent_change(stream, fence)
            record = data["program_status"]["record"]
            assert record["msg"] == "writing", data
            assert "progress" not in record, f"the message change had no event of its own: {data}"
            assert data["state"] == "running", data
            names = [e["event"] for batch in batches for e in batch.get("events", [])]
            assert "tab.state_changed" not in names, names
        client.call("session.stop")
