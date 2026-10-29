"""Program status (OSC 7501) end to end, on both terminals that can be a
tab's authoritative one (plan 082 B-2, B-3, B-8).

A tab's authoritative terminal answers the `OSC 7501 ; ?` probe, keeps the
tab's program-status records, and puts what they compose to on the wire —
`Tab.program_status`, a composed `Tab.state`, and the same pair on
`agent_report.changed`. There are two such terminals, and every case
below runs against each, as one lane of the `lane` fixture:

* `session` — a session tab's, inside a real `roost-session` (marked
  `session_daemon`, so it runs in `make e2e-session`);
* `iced` — an in-process tab's, inside the harness UI (unmarked, so it
  runs in the iced lanes, which deselect the daemon markers).

Bytes go in through `tab.feed_pty_bytes` (`ROOST_TEST_MODE=1` on both),
which takes the same per-chunk path a child's output does. A `tab.dump`
after a feed is the fence: on a session it rides the tab task's command
queue behind the feed, and the workspace commit happens inside the
feed's own turn on either terminal, so a `tab.list` after the dump reads
the result.

The Agents-panel cases at the end (B-8) are the iced UI's: rows for a
tab whose only agent is a 7501 record, and the record's banner.

The OSC 133 cases (plan 083 B-1, B-3, B-4) run on both lanes too: on
either terminal the hook layer reads its shell marks from the same
libghostty stream as the records, so a mark's form decides what it does
the way libghostty reads it.

The Mac app keeps no program-status records, so every case is iced-only,
like `test_agent_hooks_dialog.py`; `e2e-mac` deselects `session_daemon`
too.

Condition waits only.
"""

from __future__ import annotations

import base64
import random
from dataclasses import dataclass
from typing import Callable

import pytest
import session as sessionlib
import ui
from client import Roost
from eventstream import EventStream
from util import wait_tab_attached

COLS, ROWS = 80, 24
QUIET_ARGV = ["/bin/sh", "-c", "exec sleep 300"]

PROBE = b"\x1b]7501;?\x1b\\"
PROBE_REPLY = (
    b"\x1b]7501;?:states=idle,working,done,blocked,error"
    b":kinds=permission,question,auth\x1b\\"
)


@pytest.fixture(autouse=True)
def iced_only(target):
    if target != "iced":
        pytest.skip("the Mac app keeps no program-status records (plan 082 D4.2)")


def osc7501(**fields: str) -> bytes:
    """One OSC 7501 report. `msg` and `title` are base64 on the wire."""
    parts = []
    for key, value in fields.items():
        if key in ("msg", "title"):
            value = base64.b64encode(value.encode()).decode("ascii")
        parts.append(f"{key}={value}")
    return f"\x1b]7501;{':'.join(parts)}\x1b\\".encode()


@dataclass
class Lane:
    """One authoritative terminal's side of the op set: a client on its
    socket, the socket its events stream from, and how to open a tab
    parked on a child that never writes, so every byte in its stream is
    one the test put there."""

    name: str
    client: Roost
    events_socket: str
    quiet_tab: Callable[[], int]

    def fed(self, tab: int, data: bytes) -> dict:
        """Feed `data`, fence on the tab, and return the tab as listed."""
        self.client.tab_feed_pty_bytes(tab, data)
        self.client.dump(tab)
        listed = self.client.tab(tab)
        assert listed is not None, f"tab {tab} is gone"
        return listed


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

            def open_quiet() -> int:
                return client.open_tab(
                    project, cwd=str(env.launch_cwd), cols=COLS, rows=ROWS, argv=QUIET_ARGV
                )

            yield Lane("session", client, str(env.socket), open_quiet)
            client.call("session.stop")
    finally:
        env.teardown()


def _iced_lane(roost: Roost, project: int):
    if roost.identify().get("local_backend", "in-process") != "in-process":
        pytest.skip("an in-process tab needs a UI on `local-backend = in-process`")

    def open_quiet() -> int:
        tab = roost.open_tab(project, cwd="/tmp", cols=COLS, rows=ROWS, argv=QUIET_ARGV)
        wait_tab_attached(roost, tab)
        return tab

    yield Lane("iced", roost, ui.socket_path("iced"), open_quiet)


def next_for_tab(
    stream: EventStream, event: str, tab: int, fence: int
) -> tuple[dict, int, list[dict]]:
    """The next `event` for `tab`, the fence to continue from, and every
    batch read on the way. A UI socket may carry other tabs' events too;
    they are read past."""
    seen: list[dict] = []
    while True:
        batches, envelope = stream.recv_until(event, timeout=30.0)
        stream.expect_contiguous(batches, fence)
        fence = int(batches[-1]["revision"])
        seen += batches
        if envelope["data"]["tab_id"] == str(tab):
            return envelope["data"], fence, seen


def events_for(batches: list[dict], tab: int) -> list[str]:
    return [
        event["event"]
        for batch in batches
        for event in batch.get("events", [])
        if event.get("data", {}).get("tab_id") == str(tab)
    ]


# ---------------------------------------------------------------------------
# B-2 / B-3: the composed state on the wire, on either terminal
# ---------------------------------------------------------------------------


def test_the_probe_is_answered_once_byte_for_byte(lane):
    tab = lane.quiet_tab()
    lane.client.tab_capture_pty_input(tab, drain=True)
    lane.fed(tab, PROBE)
    assert lane.client.tab_capture_pty_input(tab, drain=True) == PROBE_REPLY
    listed = lane.client.tab(tab)
    assert "program_status" not in listed, "a probe is not a report"


def test_each_state_composes_onto_the_tab(lane):
    tab = lane.quiet_tab()
    for state, lifecycle, tab_state in [
        ("working", "working", "running"),
        ("blocked", "waiting", "needs_input"),
        ("error", "failed", "needs_input"),
        ("done", "finished", "idle"),
        ("idle", "inactive", "none"),
    ]:
        listed = lane.fed(tab, osc7501(state=state, app="claude-code", msg=state))
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


def test_a_blocked_child_outranks_a_working_root(lane):
    tab = lane.quiet_tab()
    listed = lane.fed(
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
    with Roost(lane.events_socket) as fresh:
        again = fresh.tab(tab)
        assert again["program_status"] == status, (again, status)
        assert again["state"] == "needs_input", again


def test_a_prompt_mark_drops_working(lane):
    tab = lane.quiet_tab()
    listed = lane.fed(tab, osc7501(state="working", app="claude-code"))
    assert listed["state"] == "running", listed
    listed = lane.fed(tab, b"\x1b]133;D;0\x07")
    assert "program_status" not in listed, listed
    assert listed["state"] == "none", listed


def test_clear_keeps_the_hook_owner(lane):
    tab = lane.quiet_tab()
    lane.client.agent_report(tab, "claude", "claim", session_id="s1", lifecycle="working")
    listed = lane.fed(tab, osc7501(state="blocked", app="claude-code"))
    assert listed["program_status"]["decided_by"] == "records", listed
    assert listed["ownership"]["source"] == "claude", listed

    listed = lane.fed(tab, osc7501(state="clear"))
    assert "program_status" not in listed, listed
    assert listed["ownership"]["source"] == "claude", "clear never releases a hook owner"
    assert listed["ownership"]["session_id"] == "s1", listed
    assert listed["hook_active"] is True, listed
    # The hooks decide again, from where they stood.
    assert listed["state"] == "running", listed


def test_the_last_clear_sends_an_event_without_program_status(lane):
    tab = lane.quiet_tab()
    with EventStream(lane.events_socket) as stream:
        fence = stream.subscribe()
        lane.client.tab_feed_pty_bytes(tab, osc7501(state="working", id="a", app="pi"))
        data, fence, _ = next_for_tab(stream, "agent_report.changed", tab, fence)
        assert data["program_status"]["record"]["id"] == "a", data
        assert data["state"] == "running", data

        lane.client.tab_feed_pty_bytes(tab, osc7501(state="clear", id="a"))
        data, fence, _ = next_for_tab(stream, "agent_report.changed", tab, fence)
        assert "program_status" not in data, data
        assert data["state"] == "none", data


def test_a_message_change_alone_produces_an_event(lane):
    tab = lane.quiet_tab()
    lane.fed(tab, osc7501(state="working", app="claude-code", msg="reading"))
    with EventStream(lane.events_socket) as stream:
        fence = stream.subscribe()
        lane.client.tab_feed_pty_bytes(
            tab, osc7501(state="working", app="claude-code", msg="writing")
        )
        # A later change that is not message-only, so a terminal that
        # stayed silent on the first fails here rather than timing out.
        lane.client.tab_feed_pty_bytes(
            tab, osc7501(state="working", app="claude-code", msg="writing", progress="50")
        )
        data, fence, batches = next_for_tab(stream, "agent_report.changed", tab, fence)
        record = data["program_status"]["record"]
        assert record["msg"] == "writing", data
        assert "progress" not in record, f"the message change had no event of its own: {data}"
        assert data["state"] == "running", data
        assert "tab.state_changed" not in events_for(batches, tab), batches


def test_a_chunks_records_land_before_its_osc_actions(lane):
    """D4.3's order inside one chunk: the records first, then the OSC
    actions. A program's own OSC 9 right after its `working` is then
    suppressed by the record it just set (D5.8) rather than doubling the
    record's banner."""
    tab = lane.quiet_tab()
    control = lane.quiet_tab()
    lane.quiet_tab()  # the active one, so neither of the two is watched
    turn_over = b"\x1b]9;turn over\x07"

    listed = lane.fed(control, turn_over)
    assert listed["has_notification"] is True, "a raw OSC 9 on an unwatched tab raises"

    listed = lane.fed(tab, osc7501(state="working", app="pi") + turn_over)
    assert listed["state"] == "running", listed
    assert listed["has_notification"] is False, listed


# The B3 review's sequence (`tab_task_test.rs`'s partition test): a working
# root, a prompt mark, a blocked child, output, a command start, clears, a
# kept `done` — both axes move, under a hook owner the first mark ends.
PARTITIONED = b"".join(
    [
        osc7501(state="working", app="claude-code", msg="thinking"),
        b"\x1b]133;A\x07",
        osc7501(state="blocked", id="build/test", kind="permission", msg="approve Bash"),
        b"some output\r\n",
        b"\x1b]133;C\x07",
        osc7501(state="clear", id="build/test"),
        osc7501(state="working", app="claude-code"),
        osc7501(state="done", app="claude-code", title="Tests"),
        b"\x1b]133;D;0\x07\x1b]133;A\x07",
    ]
)


def _composed(listed: dict) -> dict:
    """What the stream decides about a tab, without what differs between
    two tabs that saw the same bytes (ids, times, which one is focused)."""
    kept = {
        key: listed.get(key)
        for key in (
            "state",
            "program_status",
            "agent_lifecycle",
            "shell_state",
            "ownership",
            "hook_active",
            "dropped_reports",
            "has_notification",
        )
    }
    record = (kept["program_status"] or {}).get("record")
    if record is not None:
        record.pop("updated_at")
    if kept["ownership"] is not None:
        kept["ownership"].pop("last_event_at")
    return kept


def test_program_status_does_not_depend_on_read_boundaries(lane):
    """B-4's partition invariance through the terminal's own feed path: the
    stream fed whole, a byte at a time, and in random chunks leaves the
    same tab. Each feed is one chunk (a session tab task's ingest turn, an
    in-process tab's batch-tail entry). Each tab is left inactive, so the
    banners its stream raises are pending on it, and compared too."""
    stream = PARTITIONED
    rng = random.Random(0x7501)
    cuts = sorted(rng.sample(range(1, len(stream)), 12))
    partitions = {
        "whole": [stream],
        "every byte": [stream[i : i + 1] for i in range(len(stream))],
        "random": [stream[a:b] for a, b in zip([0, *cuts], [*cuts, len(stream)])],
    }
    results = {}
    for name, chunks in partitions.items():
        tab = lane.quiet_tab()
        lane.quiet_tab()
        lane.client.agent_report(tab, "claude", "claim", session_id="s1", lifecycle="working")
        for chunk in chunks:
            lane.client.tab_feed_pty_bytes(tab, chunk)
        lane.client.dump(tab)
        results[name] = _composed(lane.client.tab(tab))

    for name in ("every byte", "random"):
        assert results[name] == results["whole"], (name, results[name], results["whole"])
    status = results["whole"]["program_status"]
    assert status is not None and status["decided_by"] == "kept_record", results["whole"]
    assert status["effective_lifecycle"] == "finished", results["whole"]
    assert results["whole"]["has_notification"] is True, "the blocked child announced"


# The OSC terminators: BEL, and ST (ESC \), which libghostty acts on at
# its ESC and roost-osc at its backslash, so one-byte feeds split it.
TERMINATORS = [b"\x07", b"\x1b\\"]


def osc(body: bytes, terminator: bytes) -> bytes:
    return b"\x1b]" + body + terminator


# The branch review's sequence (plan 082 D5.8's amendment): a `blocked`
# record, the program's own OSC 9, then the `A` that ends the record's
# authority. The OSC 9 falls inside the authority, so it is suppressed
# whichever read the `A` arrives in.
def raw_osc_inside(terminator: bytes) -> bytes:
    return (
        osc7501(state="blocked", app="pi")
        + osc(b"9;turn over", terminator)
        + b"\x1b]133;A\x07"
    )


SENTINEL = b"\x1b]9;sentinel\x07"


def _banners(lane, chunks: list[bytes], owner: str | None = None) -> list[tuple[str, str]]:
    """The banners `chunks` raise on an unwatched tab, each chunk one feed:
    a tab no hook owns, or with `owner`, one a hook owner holds in that
    lifecycle. A raw OSC 9 fed after them, once nothing suppresses it,
    marks the end of the stream's banners."""
    tab = lane.quiet_tab()
    if owner is not None:
        lane.client.agent_report(tab, "claude", "claim", session_id="s1", lifecycle=owner)
    lane.quiet_tab()
    with EventStream(lane.events_socket) as stream:
        fence = stream.subscribe()
        for chunk in chunks:
            lane.client.tab_feed_pty_bytes(tab, chunk)
        lane.client.tab_feed_pty_bytes(tab, SENTINEL)
        banners = []
        while True:
            banner, fence, _ = next_for_tab(stream, "notification.fired", tab, fence)
            if banner["title"] == "sentinel":
                return banners
            banners.append((banner["title"], banner["body"]))


def _partitions(stream: bytes) -> dict[str, list[bytes]]:
    """`stream` whole, cut before its `133;A`, one byte per feed, and cut
    inside each ST (between its ESC and backslash), so the `A` lands in
    the same feed as the backslash."""
    cut = stream.index(b"\x1b]133;A")
    partitions = {
        "whole": [stream],
        "before the A": [stream[:cut], stream[cut:]],
        "every byte": [stream[i : i + 1] for i in range(len(stream))],
    }
    at = stream.find(b"\x1b\\")
    while at != -1:
        partitions[f"inside the ST at {at}"] = [stream[: at + 1], stream[at + 1 :]]
        at = stream.find(b"\x1b\\", at + 1)
    return partitions


@pytest.mark.parametrize("terminator", TERMINATORS, ids=["bel", "st"])
def test_a_raw_notification_is_judged_where_it_falls(lane, terminator):
    for name, chunks in _partitions(raw_osc_inside(terminator)).items():
        assert _banners(lane, chunks) == [("Pi", "Needs your input")], name


# ---------------------------------------------------------------------------
# Plan 083: one OSC 133 parser per sink terminal
# ---------------------------------------------------------------------------

# libghostty's OSC data buffer at the pinned SHA (`Parser.MAX_BUF`): a
# sequence whose data past `133;` overflows it is dropped whole.
OSC_BUFFER = 2048


def osc133(body: bytes) -> bytes:
    return b"\x1b]133;" + body + b"\x07"


def _cmdline(length: int) -> bytes:
    return osc133(b"C;cmdline_url=".ljust(length, b"a"))


# Plan 083 D-B3: each form, and what it does to the hook layer — a prompt
# start, input start or command end ("prompt"), a command start
# ("command"), or nothing libghostty reports ("ignored").
MARK_ROWS = [
    (osc133(b"A"), "prompt"),
    (b"\x1b]133;A;k=i\x1b\\", "prompt"),
    (osc133(b"A;redraw=0"), "prompt"),
    (osc133(b"P;k=i"), "prompt"),
    (osc133(b"N"), "prompt"),
    (osc133(b"A;k=s"), "ignored"),
    (osc133(b"A;k=r"), "ignored"),
    (osc133(b"P;k=c"), "ignored"),
    (osc133(b"B"), "prompt"),
    (osc133(b"I"), "prompt"),
    (osc133(b"C"), "command"),
    (osc133(b"C;cmdline_url=ls%20-la"), "command"),
    (_cmdline(OSC_BUFFER), "command"),
    (_cmdline(OSC_BUFFER + 1), "ignored"),
    (osc133(b"D"), "prompt"),
    (osc133(b"D;127"), "prompt"),
    (osc133(b"Axyz"), "ignored"),
    (osc133(b"Cxyz"), "ignored"),
    (osc133(b"D0"), "ignored"),
    (osc133(b"L"), "ignored"),
    (osc133(b""), "ignored"),
    (b"\x1b]133\x07", "ignored"),
]


def test_each_osc_133_form_moves_the_hook_layer_as_libghostty_reads_it(lane):
    """B-1 through the terminal's own feed path. Each row starts from a
    tab a hook owner holds, Waiting, at a prompt: a `D` and a fresh claim
    put it back between rows, so a row that moved nothing leaves exactly
    that."""
    tab = lane.quiet_tab()
    want = {
        "prompt": ("at_prompt", "inactive"),
        "command": ("foreground_process", "waiting"),
        "ignored": ("at_prompt", "waiting"),
    }
    for data, effect in MARK_ROWS:
        lane.fed(tab, osc133(b"D"))
        lane.client.agent_report(tab, "claude", "claim", session_id="s1", lifecycle="waiting")
        listed = lane.fed(tab, data)
        shown = data[:24].decode(errors="replace")
        assert (listed["shell_state"], listed["agent_lifecycle"]) == want[effect], (
            shown,
            len(data),
            listed,
        )
        assert listed["hook_active"] is True, (shown, listed)


def test_a_lone_prompt_mark_is_an_event_and_a_repeat_is_not(lane):
    """B-3 through a subscription: a prompt mark alone sends
    `agent_report.changed` with the new shell axis, and `tab.state_changed`
    when the composed state moved; the same mark again sends nothing. An
    OSC 0 title after the mark, in the same feed, is the fence: everything
    the mark sent comes before it."""
    tab = lane.quiet_tab()

    def fed_then_titled(mark: bytes, title: str, fence: int) -> tuple[list[dict], int]:
        lane.client.tab_feed_pty_bytes(tab, osc133(mark) + f"\x1b]0;{title}\x07".encode())
        data, fence, batches = next_for_tab(stream, "tab.title_changed", tab, fence)
        assert data["title"] == title, data
        return batches, fence

    with EventStream(lane.events_socket) as stream:
        fence = stream.subscribe()
        for mark, shell, state in [
            (b"C", "foreground_process", "running"),
            (b"D;0", "at_prompt", "none"),
        ]:
            batches, fence = fed_then_titled(mark, f"after {mark.decode()}", fence)
            assert events_for(batches, tab) == [
                "tab.state_changed",
                "agent_report.changed",
                "tab.title_changed",
            ], (mark, batches)
            changed = next(
                event["data"]
                for batch in batches
                for event in batch.get("events", [])
                if event["event"] == "agent_report.changed"
                and event["data"].get("tab_id") == str(tab)
            )
            assert (changed["shell_state"], changed["state"]) == (shell, state), (mark, changed)

            batches, fence = fed_then_titled(mark, f"after a second {mark.decode()}", fence)
            assert events_for(batches, tab) == ["tab.title_changed"], (mark, batches)


@pytest.mark.parametrize("terminator", TERMINATORS, ids=["bel", "st"])
def test_raw_notifications_around_a_prompt_mark_are_judged_where_they_fall(lane, terminator):
    """B-4: on a tab a hook owns and no record decides, an `OSC 9` and an
    `OSC 777` before the `A` that ends the agent's turn are suppressed, and
    the pair after it is not, whichever read the `A` arrives in, and when
    one-byte feeds split an ST between its ESC and its backslash."""
    stream = (
        osc(b"9;nine before", terminator)
        + osc(b"777;notify;seven;before", terminator)
        + osc133(b"A")
        + osc(b"9;nine after", terminator)
        + osc(b"777;notify;seven;after", terminator)
    )
    for name, chunks in _partitions(stream).items():
        assert _banners(lane, chunks, owner="working") == [
            ("nine after", ""),
            ("seven", "after"),
        ], name


# ---------------------------------------------------------------------------
# B-8: the iced Agents panel and banners for a record-only agent
# ---------------------------------------------------------------------------


@pytest.fixture
def iced_lane(roost, project):
    yield from _iced_lane(roost, project)


def _panel_entry(roost: Roost, tab: int) -> dict | None:
    for entry in roost.sidebar_dump()["agent_panel"]["entries"]:
        if entry["kind"] == "agent" and entry["tab"] == str(tab):
            return entry
    return None


def test_a_record_only_agent_gets_a_panel_row_named_by_its_app(iced_lane):
    roost = iced_lane.client
    tab = iced_lane.quiet_tab()
    roost.set_title(tab, "deploy-pane")
    assert _panel_entry(roost, tab) is None, "a quiet tab is no agent"

    iced_lane.fed(tab, osc7501(state="blocked", app="crush", msg="approve Bash"))

    def row() -> dict | None:
        entry = _panel_entry(roost, tab)
        return entry if entry is not None and entry["lifecycle"] == "waiting" else None

    entry = sessionlib.wait_until(row, 10.0, f"tab {tab}'s record in the Agents panel")
    assert entry["agent"] == "Crush", entry
    assert entry["name"] == "deploy-pane", entry
    assert entry["status_text"] == "Waiting · approve Bash", entry
    assert roost.tab(tab).get("ownership") is None, "no hook ownership is made up for the row"

    iced_lane.fed(tab, osc7501(state="clear"))
    sessionlib.wait_until(
        lambda: _panel_entry(roost, tab) is None, 10.0, "the cleared record's row to go"
    )


def test_a_record_banner_carries_its_title_and_message(iced_lane):
    roost = iced_lane.client
    tab = iced_lane.quiet_tab()
    # Not the active tab, so nobody is watching it and its banners fire.
    iced_lane.quiet_tab()
    with EventStream(iced_lane.events_socket) as stream:
        fence = stream.subscribe()
        roost.tab_feed_pty_bytes(
            tab, osc7501(state="blocked", app="claude-code", msg="approve Bash")
        )
        banner, fence, _ = next_for_tab(stream, "notification.fired", tab, fence)
        assert (banner["title"], banner["body"]) == ("Claude Code", "approve Bash"), banner

        roost.tab_feed_pty_bytes(tab, osc7501(state="error", app="claude-code", title="nightly"))
        banner, fence, _ = next_for_tab(stream, "notification.fired", tab, fence)
        assert (banner["title"], banner["body"]) == ("nightly", "Failed"), banner
