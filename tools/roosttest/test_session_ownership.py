"""Tab size ownership on a host session (plan 084, protocol 7 M3) — the
session half (`S`) of the acceptance rows A1–A21.

One owner per tab, whose grid is the PTY's. An attach takes the tab only
when it says `take` (or nobody owns it); keys, paste, clicks, the wheel,
a focused resize and `TAKE` take it; watching, unflagged input, focus
reports, automatic replies and `tab.write` never do. The owner leaving
hands it on: a `DETACH` at once, a drop after a grace, a release at once
and never back to the releasing client. Every change reaches attached
clients in band (`GRID`, at its sequence position) and every subscriber
as `tab.resized`.

"Child size" is a real child printing its PTY's size with every line it
echoes, so an ordering claim is proven at the PTY rather than at the
enqueue. Where the child cannot tell two orders apart — a click that is
delivered and then followed by the take, both before the child next
runs — the order is read off what the session queued toward the child
(`tab.capture_pty_input`) with libghostty's mode-2048 size report as the
marker of the resize.

Cases that set a timing knob (`ROOST_TEST_OWNER_GRACE_MS`,
`ROOST_TEST_ATTACH_HOLD_MS`, `ROOST_TEST_OWNER_CLOCK`) start a daemon of
their own; every case here does, through the `env` fixture.
"""

from __future__ import annotations

import json
import os
import socket
import struct
import sys
import tempfile
import time

import dataplane
import pytest
import session as sessionlib
from client import Roost, RoostError
from dataplane import DataPlane
from eventstream import EventStream
from test_ssh_transport import BridgeListener

pytestmark = pytest.mark.session_daemon

COLS, ROWS = 80, 24
DESK = (100, 30)
PHONE = (60, 20)

#: A child that reports its own size, as `stty size` does (`rows cols`):
#: once at start, with every line it reads, and on every SIGWINCH.
#: Python, not a shell: macOS's /bin/bash 3.2 rejects a fractional
#: `read -t` and drops a byte of the line it is reading when a trapped
#: SIGWINCH lands, and its zsh leaves a SIGWINCH unanswered. Python runs
#: the handler between reads and retries the interrupted one (PEP 475).
SIZE_CHILD = """
import os, signal

def size():
    s = os.get_terminal_size(0)
    return f"{s.lines} {s.columns}"

def say(text):
    os.write(1, text.encode())

signal.signal(signal.SIGWINCH, lambda *_: say(f"WINCH {size()}\\r\\n"))
say(f"UP {size()}\\r\\n")
pending = b""
while chunk := os.read(0, 4096):
    pending += chunk
    while b"\\n" in pending:
        line, pending = pending.split(b"\\n", 1)
        say(f"LINE {size()} | {line.decode(errors='replace')}\\r\\n")
"""

#: libghostty's mode-2048 in-band size report on, so a resize leaves a
#: `CSI 48 ; rows ; cols ; … t` in what the session queues toward the
#: child — in order with the input around it.
SIZE_REPORTS_ON = b"\x1b[?2048h"


@pytest.fixture
def env():
    made = sessionlib.make_env()
    try:
        yield made
    finally:
        made.teardown()


def started(env, **knobs) -> None:
    launch = env.start_daemonized(ROOST_TEST_MODE="1", **knobs)
    assert launch.returncode == 0, f"start failed: {launch.stdout!r} / {launch.stderr!r}"
    assert launch.verdict.kind == "ready", launch.verdict
    env.wait_answering()


def hello(env, client_id: str, label: str) -> Roost:
    """A control connection that says who it is, first."""
    client = env.client()
    result = client.call("client.hello", {"client_id": client_id, "label": label})
    assert result == {"bridged": False}, result
    return client


def project_of(client: Roost) -> int:
    return int(client.list()[0]["id"])


def size_tab(env, client: Roost) -> int:
    return client.open_tab(
        project_of(client),
        cwd=str(env.launch_cwd),
        cols=COLS,
        rows=ROWS,
        argv=[sys.executable, "-c", SIZE_CHILD],
    )


def quiet_tab(env, client: Roost) -> int:
    return client.open_tab(
        project_of(client),
        cwd=str(env.launch_cwd),
        cols=COLS,
        rows=ROWS,
        argv=["/bin/sh", "-c", "exec sleep 300"],
    )


def attach(
    env,
    client: Roost,
    tab: int,
    client_id: str,
    take: bool,
    grid: tuple[int, int],
    *,
    socket_path=None,
    kind: str = dataplane.GHOSTTY_SNAPSHOT,
    **handshake,
) -> tuple[DataPlane, dataplane.Reply]:
    who = client.call("session.identify")
    conn = DataPlane(env.socket if socket_path is None else socket_path, kind=kind)
    reply = conn.attach(
        tab,
        who["session_id"],
        who["libghostty_build"],
        cols=grid[0],
        rows=grid[1],
        take=take,
        client_id=client_id,
        **handshake,
    )
    return conn, reply


def attached(env, client, tab, client_id, take, grid, **kw) -> tuple[DataPlane, dataplane.Reply]:
    conn, reply = attach(env, client, tab, client_id, take, grid, **kw)
    assert reply.ok, (reply.code, reply.message)
    conn.read_until_finish()
    return conn, reply


def owner_id(owner: dict | None) -> str | None:
    return None if owner is None else owner["client_id"]


def geometry(client: Roost, tab: int) -> tuple[int, int]:
    dumped = client.dump(tab)
    return dumped["cols"], dumped["rows"]


def settles_at(client: Roost, tab: int, grid: tuple[int, int]) -> None:
    sessionlib.wait_until(lambda: geometry(client, tab) == grid, 30.0, f"tab {tab} at {grid}")


def next_grid(conn: DataPlane, timeout: float = 20.0) -> dict:
    """Read until one more `GRID` arrives; PTY frames pass by."""
    seen = len(conn.grids)
    conn.read_frames_until(
        lambda f: f.frame_type == dataplane.FRAME_GRID, timeout=timeout, what="a GRID"
    )
    assert len(conn.grids) == seen + 1
    return conn.grids[-1]


def no_grid_within(conn: DataPlane, seconds: float) -> None:
    """Nothing about the tab's size reaches this connection meanwhile."""
    deadline = time.monotonic() + seconds
    before = len(conn.grids)
    while time.monotonic() < deadline:
        try:
            conn.read_frame(timeout=max(0.05, deadline - time.monotonic()))
        except TimeoutError:
            break
    assert len(conn.grids) == before, conn.grids[before:]


def child_says(conn: DataPlane, seen: bytearray, needle: bytes, timeout: float = 30.0) -> str:
    """Read PTY output until `needle` has been printed, returning the
    line it is on."""

    def landed(frame) -> bool:
        if frame.frame_type == dataplane.FRAME_PTY:
            seen.extend(frame.pty()[1])
        return needle in seen

    if needle not in seen:
        conn.read_frames_until(landed, timeout=timeout, what=repr(needle))
    text = bytes(seen).decode(errors="replace")
    at = text.index(needle.decode())
    start = text.rfind("\n", 0, at) + 1
    end = text.find("\r", at)
    return text[start : end if end != -1 else len(text)]


def typed(conn: DataPlane, seen: bytearray, word: str, take: int) -> str:
    """Type one line and return the child's report of it."""
    conn.send_input(f"{word}\n".encode(), take=take)
    return child_says(conn, seen, f"| {word}".encode())


def size_of(line: str) -> tuple[int, int]:
    """`(cols, rows)` from a child report: `LINE rows cols | …`."""
    fields = line.split()
    return int(fields[2]), int(fields[1])


def captured_until(client: Roost, tab: int, *markers: bytes) -> bytes:
    seen = bytearray()

    def arrived() -> bool:
        seen.extend(client.tab_capture_pty_input(tab))
        return all(marker in seen for marker in markers)

    sessionlib.wait_until(arrived, 30.0, f"{markers!r} queued toward the child")
    return bytes(seen)


def size_report(grid: tuple[int, int]) -> bytes:
    return f"\x1b[48;{grid[1]};{grid[0]};".encode()


def refused_owned(client: Roost, tab: int) -> RoostError:
    with pytest.raises(RoostError) as refused:
        client.resize(tab, 70, 22)
    assert refused.value.code == "owned", refused.value
    return refused.value


# ---------------------------------------------------------------------------
# Attaching: watching never takes, an explicit switch does (A1–A3, A12, A13)
# ---------------------------------------------------------------------------


def test_a1_an_attached_watcher_never_resizes_the_tab(env):
    started(env)
    with env.client() as c:
        tab = size_tab(env, c)
        desk, desk_reply = attached(env, c, tab, "desk", True, (COLS, ROWS))
        on_desk = bytearray()
        assert typed(desk, on_desk, "first", dataplane.TAKE_NONE).startswith(f"LINE {ROWS} {COLS}")

        phone, reply = attached(env, c, tab, "phone", False, DESK)
        assert (reply.snapshot_cols, reply.snapshot_rows) == (COLS, ROWS)
        assert owner_id(reply.owner) == "desk", reply
        assert size_of(typed(desk, on_desk, "watched", dataplane.TAKE_NONE)) == (COLS, ROWS)
        assert geometry(c, tab) == (COLS, ROWS)
        assert phone.grids == [], "watching announced nothing"

        desk.send_resize(90, 26, focused=False)
        grid = next_grid(phone)
        assert (grid["cols"], grid["rows"], owner_id(grid["owner"])) == (90, 26, "desk")
        assert phone.grids == [grid], "the owner's resize is the watcher's first GRID"
        desk.close()
        phone.close()


def test_a2_an_automatic_reattach_never_takes(env):
    started(env)
    with env.client() as c:
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, DESK)
        phone, _ = attached(env, c, tab, "phone", False, PHONE)
        phone.close()
        again, reply = attached(env, c, tab, "phone", False, PHONE)
        assert owner_id(reply.owner) == "desk", reply
        assert geometry(c, tab) == DESK

        # A drop the session makes itself, and the re-attach behind it.
        c.call("tab.drop_data_conns", {"tab_id": str(tab)})
        again.expect_eof()
        desk.expect_eof()
        desk_back, _ = attached(env, c, tab, "desk", False, DESK)
        phone_back, reply = attached(env, c, tab, "phone", False, PHONE)
        assert owner_id(reply.owner) == "desk", "the dropped owner reclaimed; the watcher did not take"
        assert geometry(c, tab) == DESK
        desk_back.close()
        phone_back.close()


def test_a3_an_explicit_switch_takes_before_the_snapshot(env):
    started(env)
    with env.client() as c:
        tab = size_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        on_desk = bytearray()
        child_says(desk, on_desk, b"UP")

        phone, reply = attach(env, c, tab, "phone", True, DESK, kind=dataplane.VT)
        assert reply.ok, (reply.code, reply.message)
        assert (reply.snapshot_cols, reply.snapshot_rows) == DESK
        assert owner_id(reply.owner) == "phone"
        phone.read_until_finish()
        dumped = c.dump(tab)
        assert len(phone.snap.plain_rows()) == dumped["scrollback_rows"] + DESK[1], (
            "the snapshot's own layout is the phone's"
        )

        c.tab_feed_pty_bytes(tab, b"AFTER_THE_TAKE\r\n")
        at: list[int] = []

        def landed(frame) -> bool:
            if frame.frame_type == dataplane.FRAME_PTY:
                on_desk.extend(frame.pty()[1])
                if b"AFTER_THE_TAKE" in on_desk and not at:
                    at.append(desk.pty_frames)
            return bool(at)

        desk.read_frames_until(landed, timeout=20.0, what="output written after the take")
        assert desk.grids, "the take reached the old owner"
        grid = desk.grids[-1]
        assert (grid["cols"], grid["rows"], owner_id(grid["owner"])) == (*DESK, "phone")
        assert grid["after_pty"] < at[0], "no post-resize byte overtook the GRID"
        on_phone = bytearray()
        assert size_of(typed(phone, on_phone, "sized", dataplane.TAKE_NONE)) == DESK
        desk.close()
        phone.close()


def handshake(c: Roost, conn: DataPlane, tab: int, client_id: str, take: bool, grid) -> dict:
    who = c.call("session.identify")
    return conn.handshake_request(
        tab,
        who["session_id"],
        who["libghostty_build"],
        cols=grid[0],
        rows=grid[1],
        take=take,
        client_id=client_id,
    )


def test_a12_two_attaches_at_the_same_moment_settle_on_one_owner(env):
    started(env, ROOST_TEST_ATTACH_HOLD_MS="1500")
    with env.client() as c:
        for take in (True, False):
            tab = quiet_tab(env, c)
            a, b = DataPlane(env.socket), DataPlane(env.socket)
            # Both lines are on the wire before either reply can be: the
            # hold keeps each attach between its decision and its reply.
            a.write_handshake(handshake(c, a, tab, "a", take, DESK))
            b.write_handshake(handshake(c, b, tab, "b", take, PHONE))
            replies = [conn.read_reply() for conn in (a, b)]
            for conn, reply in zip((a, b), replies):
                assert reply.ok, (reply.code, reply.message)
                conn.read_until_finish()
            final = geometry(c, tab)
            final_owner = {DESK: "a", PHONE: "b"}[final]
            for conn, reply in zip((a, b), replies):
                said = ((reply.snapshot_cols, reply.snapshot_rows), owner_id(reply.owner))
                if said != (final, final_owner):
                    grid = conn.grids[-1] if conn.grids else next_grid(conn)
                    assert ((grid["cols"], grid["rows"]), owner_id(grid["owner"])) == (
                        final,
                        final_owner,
                    ), f"take={take}: a reply that disagrees is followed by the GRID that reaches it"
            a.close()
            b.close()


def cancelled(c: Roost, tab: int, desk: DataPlane, socket_path, how: str) -> None:
    """A taking attach that leaves during its hold never ends up owning."""
    phone = DataPlane(socket_path)
    phone.write_handshake(handshake(c, phone, tab, "phone", True, DESK))
    took = next_grid(desk)
    assert owner_id(took["owner"]) == "phone", took
    if how == "close":
        phone.half_close()
    else:
        phone.send_detach()
    back = next_grid(desk)
    assert ((back["cols"], back["rows"]), owner_id(back["owner"])) == ((COLS, ROWS), "desk")
    phone.expect_no_reply()
    phone.close()


def test_a13_a_cancelled_attach_never_owns_the_tab(env):
    started(env, ROOST_TEST_ATTACH_HOLD_MS="2000")
    with env.client() as c, BridgeListener(env) as bridge:
        tab = size_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        on_desk = bytearray()
        for socket_path, how in (
            (env.socket, "close"),
            (bridge.path, "close"),
            (env.socket, "detach"),
        ):
            cancelled(c, tab, desk, socket_path, how)
            assert size_of(typed(desk, on_desk, f"after-{how}", dataplane.TAKE_NONE)) == (COLS, ROWS)
        desk.send_resize(90, 26, focused=False)
        grid = next_grid(desk)
        assert owner_id(grid["owner"]) == "desk", "the cancelled client never owns afterwards"
        desk.close()


# ---------------------------------------------------------------------------
# Deliberate interaction takes; nothing else does (A4, A5, A8–A11)
# ---------------------------------------------------------------------------


def test_a4_keys_and_paste_take_with_the_resize_applied_first(env):
    """Two observables. The child reads the line at the phone's size; and
    the session queued the resize ahead of the bytes — libghostty's
    mode-2048 report marks the resize in what reaches the child, so it
    lands at the front of the very line the key ends."""
    started(env)
    with env.client() as c:
        tab = size_tab(env, c)
        # Cell metrics, so the size reports carry pixels the way a real
        # client's do.
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS), cell_w_px=8, cell_h_px=16)
        phone, _ = attached(env, c, tab, "phone", False, DESK, cell_w_px=8, cell_h_px=16)
        on_desk, on_phone = bytearray(), bytearray()
        child_says(desk, on_desk, b"UP")
        c.tab_feed_pty_bytes(tab, SIZE_REPORTS_ON)
        c.dump(tab)
        c.tab_capture_pty_input(tab)

        for what, typed_bytes, marker in (
            ("key", b"key\n", b"key"),
            ("paste", b"\x1b[200~pasted\x1b[201~\n", b"\x1b[200~pasted"),
        ):
            phone.send_input(typed_bytes, take=dataplane.TAKE_BEFORE)
            line = child_says(phone, on_phone, b"| " + size_report(DESK))
            assert size_of(line) == DESK, f"the {what} was read at the phone's size: {line!r}"
            queued = captured_until(c, tab, marker, size_report(DESK))
            assert queued.index(size_report(DESK)) < queued.index(marker), (
                f"the resize was applied before the {what} reached the child: {queued!r}"
            )
            desk.send_input(b"\n", take=dataplane.TAKE_BEFORE)
            settles_at(c, tab, (COLS, ROWS))
            c.tab_capture_pty_input(tab)
            on_phone.clear()
        desk.close()
        phone.close()


def test_a5_a_click_and_a_wheel_are_delivered_then_take(env):
    started(env)
    with env.client() as c:
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS), cell_w_px=8, cell_h_px=16)
        phone, _ = attached(env, c, tab, "phone", False, DESK, cell_w_px=8, cell_h_px=16)
        c.tab_feed_pty_bytes(tab, SIZE_REPORTS_ON)
        # One channel, so a dump behind the feed is a fence on it.
        c.dump(tab)
        c.tab_capture_pty_input(tab)

        for what, report in (("click", b"\x1b[<0;5;5M"), ("wheel", b"\x1b[<64;5;5M")):
            phone.send_input(report, take=dataplane.TAKE_AFTER)
            queued = captured_until(c, tab, report, size_report(DESK))
            assert queued.index(report) < queued.index(size_report(DESK)), (
                f"the {what} was delivered at the old size, then the tab was taken: {queued!r}"
            )
            grid = next_grid(desk)
            assert (grid["cols"], grid["rows"], owner_id(grid["owner"])) == (*DESK, "phone")
            desk.send_input(b"x", take=dataplane.TAKE_BEFORE)
            captured_until(c, tab, size_report((COLS, ROWS)))
            next_grid(phone)
            assert owner_id(next_grid(desk)["owner"]) == "desk", "the owner hears its own take"
        desk.close()
        phone.close()


def test_a8_a_focused_resize_takes_and_an_unfocused_one_only_records_the_grid(env):
    started(env)
    with env.client() as c:
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, PHONE)
        tablet, _ = attached(env, c, tab, "tablet", False, PHONE)

        phone.send_resize(70, 22, focused=False)
        phone.send_input(b"FENCE_ONE", take=dataplane.TAKE_NONE)
        captured_until(c, tab, b"FENCE_ONE")
        assert geometry(c, tab) == (COLS, ROWS), "an unfocused resize took the tab"
        phone.send_resize(70, 22, focused=True)
        settles_at(c, tab, (70, 22))
        assert owner_id(next_grid(desk)["owner"]) == "phone"

        tablet.send_resize(64, 21, focused=False)
        tablet.send_input(b"FENCE_TWO", take=dataplane.TAKE_NONE)
        captured_until(c, tab, b"FENCE_TWO")
        assert geometry(c, tab) == (70, 22)
        phone.detach()
        settles_at(c, tab, (64, 21))
        grid = next_grid(desk)
        assert owner_id(grid["owner"]) == "tablet", "the handoff applied the unfocused grid"
        desk.close()
        tablet.close()


def test_a9_focus_reports_and_automatic_replies_never_take(env):
    started(env)
    with env.client() as c:
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, DESK)
        phone.send_input(b"\x1b[I", take=dataplane.TAKE_NONE)
        phone.send_input(b"\x1b[O", take=dataplane.TAKE_NONE)
        # A device query the session's own terminal answers: the reply is
        # nobody's input, so it takes nothing.
        c.tab_feed_pty_bytes(tab, b"\x1b[c")
        queued = captured_until(c, tab, b"\x1b[O", b"\x1b[?")
        assert b"\x1b[I" in queued
        assert geometry(c, tab) == (COLS, ROWS)
        no_grid_within(desk, 0.5)
        desk.close()
        phone.close()


def test_a10_tab_write_changes_nothing(env):
    started(env)
    with env.client() as c, env.client() as third:
        tab = size_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, DESK)
        on_desk = bytearray()
        child_says(desk, on_desk, b"UP")
        third.send(tab, b"written\n")
        assert size_of(child_says(desk, on_desk, b"| written")) == (COLS, ROWS)
        assert geometry(c, tab) == (COLS, ROWS)
        no_grid_within(desk, 0.3)
        desk.close()
        phone.close()


def test_a11_a_failed_apply_takes_nothing_and_still_delivers(env):
    started(env)
    with env.client() as c:
        tab = size_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, DESK)
        on_desk = bytearray()
        child_says(desk, on_desk, b"UP")
        c.call("tab.fail_next_resize", {"tab_id": str(tab)})
        phone.send_input(b"delivered\n", take=dataplane.TAKE_BEFORE)
        line = child_says(desk, on_desk, b"| delivered")
        assert size_of(line) == (COLS, ROWS), f"the bytes went at the current size: {line}"
        assert geometry(c, tab) == (COLS, ROWS)
        no_grid_within(desk, 0.3)

        unowned = quiet_tab(env, c)
        c.call("tab.fail_next_resize", {"tab_id": str(unowned)})
        with pytest.raises(RoostError) as refused:
            c.resize(unowned, 100, 30)
        assert refused.value.code == "internal", refused.value
        assert geometry(c, unowned) == (COLS, ROWS)
        desk.close()
        phone.close()


# ---------------------------------------------------------------------------
# The owner leaving (A14–A16), background resizes (A17), Take size (A18)
# ---------------------------------------------------------------------------


def three_on(env, c: Roost, tab: int) -> tuple[DataPlane, DataPlane, DataPlane]:
    """An owner and two watchers. An attachment's recency is stamped in
    milliseconds, so the watchers are a stamp apart: two attaches inside
    one millisecond tie, and a tie goes to the earlier."""
    owner, _ = attached(env, c, tab, "owner", True, (COLS, ROWS))
    early, _ = attached(env, c, tab, "early", False, (90, 25))
    time.sleep(0.02)
    late, _ = attached(env, c, tab, "late", False, DESK)
    return owner, early, late


def test_a14_a_dropped_owner_keeps_the_tab_through_its_grace_then_hands_off(env):
    started(env, ROOST_TEST_OWNER_GRACE_MS="1500")
    with env.client() as c:
        tab = quiet_tab(env, c)
        owner, early, late = three_on(env, c, tab)
        time.sleep(0.02)
        early.send_resize(95, 26, focused=False)
        early.send_input(b"FENCE", take=dataplane.TAKE_NONE)
        captured_until(c, tab, b"FENCE")
        owner.close()
        refused_owned(c, tab)
        grid = next_grid(late, timeout=30.0)
        assert ((grid["cols"], grid["rows"]), owner_id(grid["owner"])) == ((95, 26), "early"), (
            "the newest advisory grid wins once the grace runs out"
        )
        early.close()
        late.close()

        tab = quiet_tab(env, c)
        owner, early, late = three_on(env, c, tab)
        owner.detach()
        grid = next_grid(late)
        assert owner_id(grid["owner"]) == "late", "a DETACH hands off at once, to the latest"


def test_a14_equal_timestamps_go_to_the_earliest_attachment(env):
    started(env, ROOST_TEST_OWNER_CLOCK="frozen")
    with env.client() as c:
        tab = quiet_tab(env, c)
        owner, early, late = three_on(env, c, tab)
        late.send_resize(110, 35, focused=False)
        late.send_input(b"FENCE", take=dataplane.TAKE_NONE)
        captured_until(c, tab, b"FENCE")
        owner.detach()
        grid = next_grid(early)
        assert ((grid["cols"], grid["rows"]), owner_id(grid["owner"])) == ((90, 25), "early")
        early.close()
        late.close()


def test_a15_release_hands_off_never_back_and_is_a_no_op_for_a_non_owner(env):
    started(env)
    with env.client() as c:
        desk_ctl = hello(env, "desk", "desk-label")
        phone_ctl = hello(env, "phone", "phone-label")
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, DESK)

        assert phone_ctl.call("tab.release_size", {"tab_id": str(tab)}) == {}
        assert geometry(c, tab) == (COLS, ROWS), "a non-owner's release is a no-op"
        assert desk_ctl.call("tab.release_size", {"tab_id": str(tab)}) == {}
        grid = next_grid(phone)
        assert (grid["cols"], grid["rows"], owner_id(grid["owner"])) == (*DESK, "phone")
        assert grid["owner"]["label"] == "phone-label"

        alone = quiet_tab(env, c)
        only, _ = attached(env, c, alone, "desk", True, DESK)
        desk_ctl.call("tab.release_size", {"tab_id": str(alone)})
        grid = next_grid(only)
        assert (grid["cols"], grid["rows"], grid["owner"]) == (*DESK, None), "unowned, size kept"
        c.resize(alone, 70, 22)
        settles_at(c, alone, (70, 22))
        for conn in (desk, phone, only):
            conn.close()
        desk_ctl.close()
        phone_ctl.close()


def test_a16_a_reconnecting_owner_reclaims_within_its_grace(env):
    started(env, ROOST_TEST_OWNER_GRACE_MS="60000")
    with env.client() as c:
        tab = size_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, DESK)
        # A stamp apart (see `three_on`), so the newest attachment is the
        # tablet's.
        time.sleep(0.02)
        tablet, _ = attached(env, c, tab, "tablet", False, PHONE)
        desk.close()
        refused_owned(c, tab)

        back, reply = attached(env, c, tab, "desk", False, (COLS, ROWS))
        assert owner_id(reply.owner) == "desk", reply
        on_back = bytearray()
        assert size_of(typed(back, on_back, "reclaimed", dataplane.TAKE_NONE)) == (COLS, ROWS), (
            "no reflow"
        )
        no_grid_within(phone, 0.3)

        # A reconnect whose attach beats the old connection's EOF.
        again, reply = attached(env, c, tab, "desk", False, (COLS, ROWS))
        assert owner_id(reply.owner) == "desk"
        back.close()
        refused_owned(c, tab)
        # The reconnect owns the tab live: its own deliberate detach hands
        # it on at once, which a grace record never does.
        again.detach()
        grid = next_grid(phone)
        assert owner_id(grid["owner"]) == "tablet", "handed to the newest attachment"

        # Another client's take during a grace takes at once.
        tablet.close()
        refused_owned(c, tab)
        phone.send_take()
        grid = next_grid(phone)
        assert ((grid["cols"], grid["rows"]), owner_id(grid["owner"])) == (DESK, "phone")
        phone.close()


def test_a17_a_background_resize_of_an_owned_tab_is_refused_naming_the_owner(env):
    started(env)
    with env.client() as c:
        desk_ctl = hello(env, "desk", "charlie-desk")
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, DESK)
        error = refused_owned(c, tab)
        assert f"tab {tab} is sized by charlie-desk" in error.message, error.message
        assert geometry(c, tab) == DESK
        desk.close()
        desk_ctl.close()


def test_a18_take_size(env):
    started(env)
    with env.client() as c:
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, DESK)

        with pytest.raises(RoostError) as no_hello:
            c.call("tab.take_size", {"tab_id": str(tab)})
        assert no_hello.value.code == "invalid-param", no_hello.value
        stranger = hello(env, "stranger", "elsewhere")
        with pytest.raises(RoostError) as no_attachment:
            stranger.call("tab.take_size", {"tab_id": str(tab)})
        assert no_attachment.value.code == "not-found", no_attachment.value

        phone_ctl = hello(env, "phone", "phone")
        assert phone_ctl.call("tab.take_size", {"tab_id": str(tab)}) == {
            "cols": DESK[0],
            "rows": DESK[1],
        }
        grid = next_grid(desk)
        assert owner_id(grid["owner"]) == "phone"
        for closing in (desk, phone, stranger, phone_ctl):
            closing.close()


# ---------------------------------------------------------------------------
# Ordering with output, and every subscriber (A19–A21)
# ---------------------------------------------------------------------------


def resized_events(stream: EventStream, tab: int, count: int) -> list[dict]:
    seen: list[dict] = []
    while len(seen) < count:
        _batches, envelope = stream.recv_until("tab.resized", timeout=20.0)
        if envelope["data"]["tab_id"] == str(tab):
            seen.append(envelope["data"])
    return seen


def test_a19_a_grid_sits_at_its_sequence_position_whatever_the_delivery_order(env):
    started(env)
    with env.client() as c:
        tab = size_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        on_desk = bytearray()
        child_says(desk, on_desk, b"UP")
        for events_first in (True, False):
            with EventStream(env.socket) as stream:
                stream.subscribe()
                phone, _ = attached(env, c, tab, f"phone-{events_first}", False, DESK)
                phone.send_take()
                if events_first:
                    event = resized_events(stream, tab, 1)[0]
                    time.sleep(0.3)
                    grid = next_grid(desk)
                else:
                    grid = next_grid(desk)
                    time.sleep(0.3)
                    event = resized_events(stream, tab, 1)[0]
                assert (event["cols"], event["rows"], event["owner"]) == (
                    grid["cols"],
                    grid["rows"],
                    grid["owner"],
                ), "tab.resized and GRID agree"
                assert grid["seq"] == (desk.last_seq or 0), (
                    "the GRID names the last PTY seq before it"
                )
                assert b"WINCH 30 100" not in on_desk, "the SIGWINCH marker came before the GRID"
                child_says(desk, on_desk, b"WINCH 30 100")
                phone.detach()
                next_grid(desk)
                # The child has answered the handoff's own SIGWINCH before
                # the next take sends another: two inside one trap run can
                # reach a shell as one.
                on_desk.clear()
                child_says(desk, on_desk, f"WINCH {ROWS} {COLS}".encode())
                on_desk.clear()


def test_a20_tab_resized_reaches_every_subscriber(env):
    started(env)
    with env.client() as c:
        tab = quiet_tab(env, c)
        streams = [EventStream(env.socket) for _ in range(3)]
        for stream in streams:
            stream.subscribe()
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, _ = attached(env, c, tab, "phone", False, DESK)
        phone.send_take()
        for stream in streams:
            events = resized_events(stream, tab, 2)
            assert [owner_id(e["owner"]) for e in events] == ["desk", "phone"], events
            assert (events[-1]["cols"], events[-1]["rows"]) == DESK
            stream.close()
        desk.close()
        phone.close()


def test_a21_resume_and_hydration_races(env):
    started(env, ROOST_TEST_OWNER_GRACE_MS="60000")
    with env.client() as c:
        # A resume across a grid change falls back to a snapshot at the
        # new grid.
        tab = quiet_tab(env, c)
        desk, _ = attached(env, c, tab, "desk", True, (COLS, ROWS))
        phone, reply = attached(env, c, tab, "phone", False, DESK)
        fence = phone.last_seq or reply.seq
        identity = (reply.server_epoch, reply.tab_generation)
        phone.close()
        desk.send_resize(90, 26, focused=False)
        sessionlib.wait_until(lambda: geometry(c, tab) == (90, 26), 30.0, "the owner's resize")
        # Written after the change, so a stream that left before it is the
        # only one the change refuses.
        c.tab_feed_pty_bytes(tab, b"AT_NINETY\r\n")
        again, resumed = attach(
            env,
            c,
            tab,
            "phone",
            False,
            DESK,
            resume_from_seq=fence + 1,
            server_epoch=identity[0],
            tab_generation=identity[1],
        )
        assert resumed.ok and resumed.mode == "snapshot", resumed
        assert (resumed.snapshot_cols, resumed.snapshot_rows) == (90, 26)
        again.read_until_finish()

        # An owner-only change during a resume gap arrives in the reply.
        fence = again.last_seq or resumed.seq
        identity = (resumed.server_epoch, resumed.tab_generation)
        again.close()
        with hello(env, "tablet", "tablet") as tablet_ctl:
            tablet, tablet_reply = attached(env, c, tab, "tablet", False, (90, 26))
            tablet_ctl.call("tab.take_size", {"tab_id": str(tab)})
            assert geometry(c, tab) == (90, 26), "the same grid: an owner-only change"
            back, reply = attach(
                env,
                c,
                tab,
                "phone",
                False,
                DESK,
                resume_from_seq=fence + 1,
                server_epoch=identity[0],
                tab_generation=identity[1],
            )
            assert reply.ok and reply.mode == "resume", reply
            assert owner_id(reply.owner) == "tablet", reply
            back.close()

            # An owner resuming within its grace reclaims.
            fence = tablet.last_seq or tablet_reply.seq
            tablet.close()
            returned, reply = attach(
                env,
                c,
                tab,
                "tablet",
                False,
                (90, 26),
                resume_from_seq=fence + 1,
                server_epoch=tablet_reply.server_epoch,
                tab_generation=tablet_reply.tab_generation,
            )
            assert reply.ok and reply.mode == "resume", reply
            assert owner_id(reply.owner) == "tablet", reply
            assert geometry(c, tab) == (90, 26)
            returned.close()
        desk.close()

        # A GRID during a watcher's hydration lands after READY, in order.
        wide = c.open_tab(
            project_of(c),
            cwd=str(env.launch_cwd),
            cols=560,
            rows=ROWS,
            argv=["/bin/sh", "-c", "exec sleep 300"],
        )
        filler = ("".join(f"{i:08d}" for i in range(70)) + "\r\n").encode()
        c.tab_feed_pty_bytes(wide, filler * 2000)
        owner, _ = attached(env, c, wide, "owner", True, (560, ROWS))
        watcher, reply = attach(env, c, wide, "watcher", False, (560, ROWS))
        assert reply.ok, reply
        watcher.read_until_ready()
        owner.send_resize(500, ROWS, focused=False)
        watcher.read_until_finish()
        grid = watcher.grids[0] if watcher.grids else next_grid(watcher)
        assert (grid["cols"], grid["rows"]) == (500, ROWS)
        assert geometry(c, wide) == (500, ROWS), "the final mirror equals the PTY"
        owner.close()
        watcher.close()


# ---------------------------------------------------------------------------
# The harness's own GRID rule
# ---------------------------------------------------------------------------


def _scripted(frames: list[tuple[int, bytes]]) -> DataPlane:
    """A data connection whose server is this test: an accepted resume at
    fence 7, then `frames`, then a close."""
    directory = tempfile.mkdtemp(prefix="r084-", dir="/tmp")
    path = os.path.join(directory, "s.sock")
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        listener.bind(path)
        listener.listen(1)
        conn = DataPlane(path)
        server, _ = listener.accept()
    finally:
        listener.close()
        os.unlink(path)
        os.rmdir(directory)
    reply = {
        "ok": True,
        "kind": dataplane.GHOSTTY_SNAPSHOT,
        "mode": "resume",
        "seq": 7,
        "server_epoch": 1,
        "tab_generation": 1,
        "snapshot_cols": COLS,
        "snapshot_rows": ROWS,
        "owner": None,
    }
    out = json.dumps(reply).encode() + b"\n" + dataplane.PREAMBLE
    for frame_type, payload in frames:
        out += struct.pack("<IB", len(payload), frame_type) + payload
    with server:
        server.sendall(out)
    conn.read_reply()
    return conn


def _pty(seq: int) -> tuple[int, bytes]:
    return dataplane.FRAME_PTY, struct.pack("<Q", seq) + b"x"


def _grid(seq: int) -> tuple[int, bytes]:
    body = {"seq": seq, "cols": COLS, "rows": ROWS, "owner": None}
    return dataplane.FRAME_GRID, json.dumps(body).encode()


def test_the_harness_holds_a_grid_to_the_record_it_names():
    """Every ordering assertion on GRID rests on this observer, so it is
    checked against streams no session would write: a GRID names the
    last PTY record read before it, and the reply's fence before any."""
    in_place = _scripted([_grid(7), _pty(8), _grid(8), _pty(9)])
    for _ in range(4):
        in_place.read_frame()
    assert [grid["seq"] for grid in in_place.grids] == [7, 8]

    ahead = _scripted([_pty(8), _grid(9), _pty(9)])
    ahead.read_frame()
    with pytest.raises(AssertionError, match="GRID named seq 9"):
        ahead.read_frame()

    behind = _scripted([_grid(3)])
    with pytest.raises(AssertionError, match="GRID named seq 3"):
        behind.read_frame()
