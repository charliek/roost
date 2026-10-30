"""Tab size ownership through the iced UI (plan 084, protocol 7 M3) — the
host-client half (`H`) of the acceptance rows.

The harness UI attaches a session tab the way a person's window does,
beside a raw `DataPlane` client standing in for a second machine (the
"desk"). What the window did is read back three ways:

* `host.size_trace` — the size-ownership actions it took for the tab, in
  order (`attach-sent{take}`, `attach-accepted{resumed}`,
  `copy-captured`, `take-sent`, `detach-sent`, `resize-sent{focused}`,
  `grid-applied`);
* `app.window_metrics` — the mirror's grid (`mirror_grid`, which is the
  PTY's), the row offset it is drawn at, and who sizes it
  (`size_owner`);
* the session itself — `tab.dump`'s grid is the PTY's, and a child that
  prints its own size proves an order at the PTY rather than at the
  enqueue.

The background wave (A17) runs on the local session slot only, so its
iced half lives in `test_local_backend.py`, whose lane owns a slot.

Condition waits only.
"""

from __future__ import annotations

import contextlib
import sys
import time

import dataplane
import pytest
from client import Roost, RoostError
from dataplane import DataPlane
from host_probe import host_key, sibling_key
from relay import DATA, EVENTS, Relay
from test_host_client import (  # noqa: F401  (fixtures)
    dump,
    focus,
    host,
    require_system_clipboard,
    require_test_mode,
    saved_host,
    session_env,
    start_session,
    view_grid,
    wait_until,
    window_restored,
)
from test_session_ownership import (
    COLS,
    ROWS,
    attached,
    captured_until,
    child_says,
    geometry,
    next_grid,
    no_grid_within,
    project_of,
    quiet_tab,
    refused_owned,
    size_of,
    size_report,
    size_tab,
    typed,
)
from util import palette_command

pytestmark = pytest.mark.host_client

#: The desk's grid: smaller than any window the harness opens, so the
#: window draws it whole, with no offset.
DESK = (52, 14)

#: A child that turns SGR mouse reports on and prints every chunk it reads
#: with its size at that moment, and its size on every SIGWINCH — what
#: proves a click was delivered before the take it brought (A5).
MOUSE_CHILD = r"""
import os, signal, tty

# Raw, so a mouse report reaches `read` the moment it is typed rather
# than waiting in the line discipline for a newline it never has.
tty.setraw(0)

def size():
    s = os.get_terminal_size(0)
    return f"{s.lines} {s.columns}"

def say(text):
    os.write(1, text.encode())

signal.signal(signal.SIGWINCH, lambda *_: say(f"WINCH {size()}\r\n"))
say("\x1b[?1000h\x1b[?1006h")
say(f"UP {size()}\r\n")
while chunk := os.read(0, 4096):
    say(f"GOT {size()} {chunk!r}\r\n")
"""

#: Mode 1003 (any-event mouse) and 1004 (focus reports), then quiet.
WATCHING_CHILD = r"printf '\033[?1003h\033[?1006h\033[?1004h'; exec sleep 300"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def actions(roost: Roost, key: str) -> list[dict]:
    return roost.call("host.size_trace", {"tab_id": key})["actions"]


def named(roost: Roost, key: str, action: str) -> list[dict]:
    return [entry for entry in actions(roost, key) if entry["action"] == action]


def accepted_since_last_attach(roost: Roost, key: str) -> dict | None:
    """The accept of the window's latest attach, once the session sent it."""
    trace = actions(roost, key)
    sent = [n for n, entry in enumerate(trace) if entry["action"] == "attach-sent"]
    after = trace[sent[-1] + 1 :] if sent else []
    return next((entry for entry in after if entry["action"] == "attach-accepted"), None)


def mirror(roost: Roost) -> tuple[int, int] | None:
    grid = roost.window_metrics().get("mirror_grid")
    return None if grid is None else (grid["cols"], grid["rows"])


def owner(roost: Roost) -> dict:
    return roost.window_metrics().get("size_owner") or {}


def wait_sized_by(roost: Roost, client_id: str, what: str) -> None:
    wait_until(
        lambda: owner(roost).get("client_id") == client_id,
        30.0,
        f"the window to show the tab sized by {client_id} ({what})",
    )


def wait_mine(roost: Roost, what: str) -> dict:
    return wait_until(
        lambda: owner(roost) if owner(roost).get("is_me") else None,
        30.0,
        f"the window to size the tab itself ({what})",
    )


def wait_mirror(roost: Roost, grid: tuple[int, int], what: str) -> None:
    wait_until(lambda: mirror(roost) == grid, 30.0, f"the mirror at {grid} ({what})")


def wait_geometry(session: Roost, tab: int, grid: tuple[int, int], what: str) -> None:
    wait_until(lambda: geometry(session, tab) == grid, 30.0, f"tab {tab} at {grid} ({what})")


def tab_running(env, session: Roost, argv: list[str]) -> int:
    return session.open_tab(
        project_of(session), cwd=str(env.launch_cwd), cols=COLS, rows=ROWS, argv=argv
    )


def raw(env, session: Roost, tab: int, client_id: str, take: bool, grid) -> DataPlane:
    """The desk's attach, beside the window."""
    return attached(env, session, tab, client_id, take, grid)[0]


@contextlib.contextmanager
def connected(host_under_test):
    host_under_test.connect_and_wait()
    with host_under_test.client() as session:
        yield session


def shown(roost: Roost, tab: int) -> str:
    """Switch the window to `tab` the way a person does — an explicit
    switch, which takes it — and return its key once it sizes it."""
    key = host_key(roost, tab)
    wait_mine(roost, f"the switch to {key}")
    return key


def desk_takes(desk: DataPlane, roost: Roost, what: str) -> None:
    """The desk takes the tab with `TAKE`, and the window follows. The
    desk's own `GRID` for it is read here, so a later "no `GRID`" check
    starts from a clean stream."""
    desk.send_take()
    desk.read_frames_until(
        lambda frame: frame.frame_type == dataplane.FRAME_GRID
        and desk.grids[-1]["owner"]["client_id"] == desk.client_id,
        timeout=20.0,
        what="the desk's own GRID",
    )
    wait_sized_by(roost, desk.client_id, what)
    wait_mirror(roost, DESK, what)


# ---------------------------------------------------------------------------
# A1–A3: watching never takes, an explicit switch does
# ---------------------------------------------------------------------------


def test_a1_an_automatic_attach_leaves_the_owners_size(host, roost):
    """The window's fallback after the tab it showed closes is its own
    doing, not a switch: it attaches `take: false` and draws the desk's
    grid, which nothing resizes."""
    with connected(host) as session:
        # The fallback lands on the closed tab's left-hand neighbour when
        # it was the last of its strip.
        kept, closing = quiet_tab(host.env, session), quiet_tab(host.env, session)
        desk = raw(host.env, session, kept, "desk-a1", True, DESK)
        closing_key = shown(roost, closing)
        key = sibling_key(closing_key, kept)
        session.close_tab(closing)

        wait_sized_by(roost, "desk-a1", "the fallback attach")
        wait_mirror(roost, DESK, "the fallback attach")
        assert named(roost, key, "attach-sent") == [{"action": "attach-sent", "take": False}]
        assert geometry(session, kept) == DESK
        no_grid_within(desk, 1.0)
        desk.close()


def test_a2_a_forced_reattach_never_takes(roost, session_env):
    """`tab.drop_data_conns` ends every data connection of the tab as a
    network drop would. The window's re-attach is its own, `take: false`,
    and the desk — holding its grace — stays the owner."""
    require_test_mode(roost)
    start_session(session_env, ROOST_TEST_OWNER_GRACE_MS="60000")
    with saved_host(roost, session_env) as under_test, connected(under_test) as session:
        tab = quiet_tab(under_test.env, session)
        key = shown(roost, tab)
        desk = raw(under_test.env, session, tab, "desk-a2", False, DESK)
        desk_takes(desk, roost, "before the drop")

        session.call("tab.drop_data_conns", {"tab_id": str(tab)})
        desk.drain_to_close(timeout=10.0)
        wait_until(
            lambda: len(named(roost, key, "attach-sent")) >= 2,
            30.0,
            "the window to attach again",
        )
        takes = [entry["take"] for entry in named(roost, key, "attach-sent")]
        assert takes[0] is True and not any(takes[1:]), takes
        wait_sized_by(roost, "desk-a2", "the re-attach")
        assert geometry(session, tab) == DESK


def test_a3_an_explicit_switch_takes_before_the_snapshot(host, roost):
    """The window switching to a tab the desk sizes takes it at the
    window's grid before the payload is composed: the mirror is built at
    that grid, the desk hears the `GRID` before any byte the child wrote
    after the resize, and the child reports the window's size."""
    with connected(host) as session:
        tab = size_tab(host.env, session)
        desk = raw(host.env, session, tab, "desk-a3", True, DESK)
        on_desk = bytearray()
        child_says(desk, on_desk, b"UP")
        view = view_grid(roost)
        assert view != DESK

        key = shown(roost, tab)
        assert mirror(roost) == view
        assert named(roost, key, "grid-applied") == [], "built at the window's grid"
        winch = f"WINCH {view[1]} {view[0]}".encode()
        at: list[int] = []

        def landed(frame) -> bool:
            if frame.frame_type == dataplane.FRAME_PTY:
                on_desk.extend(frame.pty()[1])
                if winch in on_desk and not at:
                    at.append(desk.pty_frames)
            return bool(at)

        desk.read_frames_until(landed, timeout=20.0, what="the child's SIGWINCH report")
        grid = desk.grids[-1]
        assert (grid["cols"], grid["rows"]) == view
        assert grid["owner"]["client_id"] == owner(roost)["client_id"]
        assert grid["after_pty"] < at[0], "no post-resize byte overtook the GRID"
        assert geometry(session, tab) == view
        desk.close()


# ---------------------------------------------------------------------------
# A4, A5, A8, A9: deliberate interaction takes; nothing else does
# ---------------------------------------------------------------------------


def test_a4_a_key_takes_before_its_bytes(host, roost):
    """A key typed into another client's tab resizes the tab first: the
    child reads the line at the window's size."""
    with connected(host) as session:
        tab = size_tab(host.env, session)
        shown(roost, tab)
        view = view_grid(roost)
        desk = raw(host.env, session, tab, "desk-a4", False, DESK)
        on_desk = bytearray()
        desk_takes(desk, roost, "before the key")

        roost.type_text("k4")
        roost.key_event("Enter")
        line = child_says(desk, on_desk, b"| k4")
        assert size_of(line) == view, line
        wait_mine(roost, "the key")
        desk.close()


def test_a4_a_paste_takes_before_its_bytes(host, roost):
    require_system_clipboard(roost)
    with connected(host) as session:
        tab = size_tab(host.env, session)
        shown(roost, tab)
        view = view_grid(roost)
        desk = raw(host.env, session, tab, "desk-a4p", False, DESK)
        on_desk = bytearray()
        desk_takes(desk, roost, "before the paste")

        roost.clipboard_write("system", "p4\n")
        roost.call("app.keybind_dispatch", {"action": "paste"})
        line = child_says(desk, on_desk, b"| p4")
        assert size_of(line) == view, line
        desk.close()


def test_a5_a_click_and_the_wheel_are_delivered_then_take(host, roost):
    """A click on another client's tab reaches the program at the size it
    was aimed at, in that grid's coordinates, and only then is the tab
    taken; the wheel the same. The child cannot tell the two apart when
    both land before it next runs, so the order is read off what the
    session queued toward it, where libghostty's mode-2048 size report
    marks the resize."""
    with connected(host) as session:
        tab = tab_running(host.env, session, [sys.executable, "-c", MOUSE_CHILD])
        key = shown(roost, tab)
        view = view_grid(roost)
        session.tab_feed_pty_bytes(tab, b"\x1b[?2048h")
        desk = raw(host.env, session, tab, "desk-a5", False, DESK)
        for gesture, events, report in (
            ("click", [("press", "left"), ("release", "left")], b"\x1b[<0;4;3M"),
            ("wheel", [("press", "wheel_up")], b"\x1b[<64;4;3M"),
        ):
            desk_takes(desk, roost, f"before the {gesture}")
            wait_geometry(session, tab, DESK, f"before the {gesture}")
            session.tab_capture_pty_input(tab)
            for kind, button in events:
                roost.tab_dispatch_mouse_event(key, kind, button, 3, 2)
            seen = captured_until(session, tab, report, size_report(view))
            assert seen.index(report) < seen.index(size_report(view)), (gesture, bytes(seen))
            wait_mine(roost, gesture)
        desk.close()


def test_a8_an_unfocused_resize_only_says_what_the_window_would_like(host, roost):
    """An unfocused window's resize leaves the desk the owner, and is
    what the handoff applies once the desk leaves; a focused one takes."""
    with connected(host) as session, window_restored(roost):
        tab = quiet_tab(host.env, session)
        key = shown(roost, tab)
        desk = raw(host.env, session, tab, "desk-a8", False, DESK)
        desk_takes(desk, roost, "before the resizes")
        before = roost.window_metrics()
        try:
            roost.app_set_window_focus(False)
            roost.window_resize(before["window_width"] - 120, before["window_height"] - 80)
            narrower = view_grid(roost)
            wait_until(
                lambda: {"action": "resize-sent", "focused": False}
                in named(roost, key, "resize-sent"),
                30.0,
                "the unfocused RESIZE",
            )
            assert owner(roost)["client_id"] == "desk-a8"
            assert geometry(session, tab) == DESK

            desk.detach()
            wait_geometry(session, tab, narrower, "the handoff to the window's advisory grid")
            wait_mine(roost, "the handoff")

            again = raw(host.env, session, tab, "desk-a8b", True, DESK)
            wait_sized_by(roost, "desk-a8b", "the desk's switch back")
            roost.app_set_window_focus(True)
            roost.window_resize(before["window_width"], before["window_height"])
            wait_mine(roost, "the focused RESIZE")
            wait_geometry(session, tab, view_grid(roost), "the focused RESIZE")
            assert {"action": "resize-sent", "focused": True} in named(roost, key, "resize-sent")
            again.close()
        finally:
            roost.app_set_window_focus(True)


def test_a9_focus_reports_and_a_buttonless_motion_take_nothing(host, roost):
    """Focus out and in, and a pointer moving with no button held under
    mode 1003: every report reaches the child, and the desk keeps the
    tab at its own grid."""
    with connected(host) as session:
        tab = tab_running(host.env, session, ["/bin/sh", "-c", WATCHING_CHILD])
        key = shown(roost, tab)
        desk = raw(host.env, session, tab, "desk-a9", False, DESK)
        desk_takes(desk, roost, "before the reports")
        session.tab_capture_pty_input(tab)
        try:
            roost.app_set_window_focus(False)
            roost.app_set_window_focus(True)
            for col in (3, 9, 15):
                roost.tab_dispatch_mouse_event(key, "motion", "none", col, 2)
        finally:
            roost.app_set_window_focus(True)
        captured_until(session, tab, b"\x1b[O", b"\x1b[I", b"\x1b[<35;")
        no_grid_within(desk, 1.0)
        assert owner(roost)["client_id"] == "desk-a9"
        assert geometry(session, tab) == DESK
        assert named(roost, key, "take-sent") == []
        desk.close()


# ---------------------------------------------------------------------------
# A6, A7: a copy and a settled scroll take
# ---------------------------------------------------------------------------


def test_a6_a_copy_takes_after_the_text_is_captured(host, roost):
    """The selection is copied as the desk's grid wrapped it, and only
    then is the tab taken — the trace says so, in that order."""
    require_system_clipboard(roost)
    with connected(host) as session:
        tab = quiet_tab(host.env, session)
        key = shown(roost, tab)
        desk = raw(host.env, session, tab, "desk-a6", False, DESK)
        desk_takes(desk, roost, "before the copy")
        line = "Q" * (DESK[0] + 20)
        session.tab_feed_pty_bytes(tab, f"\x1b[H\x1b[2J{line}\r\n".encode())
        wait_until(
            lambda: dump(roost, key)["rows_text"][0].strip()
            == "Q" * DESK[0],
            30.0,
            "the line wrapped at the desk's width in the window",
        )
        roost.clipboard_write("selection", "")
        roost.tab_dispatch_mouse_event(key, "press", "left", 0, 0)
        roost.tab_dispatch_mouse_event(key, "motion", "left", DESK[0] - 1, 0)
        roost.tab_dispatch_mouse_event(key, "release", "left", DESK[0] - 1, 0)

        wait_mine(roost, "the copy")
        order = [
            entry["action"]
            for entry in actions(roost, key)
            if entry["action"] in ("copy-captured", "take-sent")
        ]
        assert order == ["copy-captured", "take-sent"], order
        assert roost.clipboard_dump("selection") == "Q" * DESK[0], "the old layout's text"
        desk.close()


def displayed_top(roost: Roost, key: str) -> str:
    """The line drawn at the top of the view: the mirror's row at the
    window's offset."""
    offset = roost.window_metrics()["grid_offset_rows"]
    return dump(roost, key)["rows_text"][offset]


def test_a7_a_settled_scroll_takes_and_keeps_the_top_line(host, roost):
    """The desk's grid is taller than the window's view, so the window
    draws it from a row offset. Page Up every 100 ms for a second takes
    nothing; 300 ms after the last one the tab is taken, and the line
    drawn at the top of the view is the same one once the mirror has the
    window's grid and no offset."""
    with connected(host) as session:
        tab = quiet_tab(host.env, session)
        key = shown(roost, tab)
        view = view_grid(roost)
        tall = (view[0], view[1] + 8)
        desk = raw(host.env, session, tab, "desk-a7", False, tall)
        desk.send_take()
        wait_sized_by(roost, "desk-a7", "before the scroll")
        wait_mirror(roost, tall, "before the scroll")
        history = "".join(f"line {n:03}\r\n" for n in range(400))
        session.tab_feed_pty_bytes(tab, history.encode())
        wait_until(
            lambda: "line 399" in "\n".join(dump(roost, key)["rows_text"]),
            30.0,
            "the history in the window",
        )
        assert roost.window_metrics()["grid_offset_rows"] == 8
        for _ in range(10):
            roost.key_event("PageUp")
            time.sleep(0.1)
            assert named(roost, key, "take-sent") == [], "taken while still scrolling"
        top = displayed_top(roost, key)
        assert top.startswith("line"), top
        wait_until(lambda: named(roost, key, "take-sent"), 30.0, "the settled scroll's TAKE")
        wait_mirror(roost, view, "the settled scroll's take")
        assert roost.window_metrics()["grid_offset_rows"] == 0
        assert displayed_top(roost, key) == top
        desk.close()


# ---------------------------------------------------------------------------
# A16, A21: reconnects and resumes
# ---------------------------------------------------------------------------


def test_a16_a_dropped_owner_reclaims_within_its_grace_without_a_reflow(roost, session_env):
    require_test_mode(roost)
    start_session(session_env, ROOST_TEST_OWNER_GRACE_MS="60000")
    with saved_host(roost, session_env) as under_test, connected(under_test) as session:
        tab = size_tab(under_test.env, session)
        key = shown(roost, tab)
        view = view_grid(roost)
        watcher = raw(under_test.env, session, tab, "desk-a16", False, DESK)
        on_watcher = bytearray()

        session.call("tab.drop_data_conns", {"tab_id": str(tab)})
        watcher.drain_to_close(timeout=10.0)
        refused_owned(session, tab)
        wait_until(
            lambda: len(named(roost, key, "attach-sent")) >= 2
            and accepted_since_last_attach(roost, key),
            30.0,
            "the session to accept the window's attach again",
        )
        wait_mine(roost, "the reclaim")
        assert geometry(session, tab) == view, "no reflow"
        assert named(roost, key, "grid-applied") == []
        again = raw(under_test.env, session, tab, "desk-a16b", False, DESK)
        assert size_of(typed(again, on_watcher, "after-reclaim", dataplane.TAKE_NONE)) == view
        again.close()


def test_a21_an_owner_change_in_a_resume_gap_arrives_with_the_accept(host, roost):
    """The window leaves the tab and the desk takes it at the window's own
    grid — an owner change and no geometry change. The window's fallback
    attach resumes, and learns the new owner from the accepted reply: no
    `GRID` ever tells it."""
    require_test_mode(roost)
    with connected(host) as session:
        tab, other = quiet_tab(host.env, session), quiet_tab(host.env, session)
        key = shown(roost, tab)
        view = view_grid(roost)
        # Output behind the switch's own `GRID`: a resume point at that
        # `GRID`'s position is one the session cannot tell was seen, and
        # it answers it with a snapshot.
        wait_mirror(roost, view, "the switch's GRID")
        session.tab_feed_pty_bytes(tab, b"past-the-switch\r\n")
        wait_until(
            lambda: any("past-the-switch" in row for row in dump(roost, key)["rows_text"]),
            30.0,
            "the window to show output past the switch",
        )
        other_key = sibling_key(key, other)
        focus(roost, other_key)
        wait_mine(roost, "the other tab")
        desk = raw(host.env, session, tab, "desk-a21", True, view)
        session.close_tab(other)

        wait_sized_by(roost, "desk-a21", "the resumed attach")
        assert named(roost, key, "attach-sent")[-1] == {"action": "attach-sent", "take": False}
        assert accepted_since_last_attach(roost, key) == {
            "action": "attach-accepted",
            "resumed": True,
        }, actions(roost, key)
        assert named(roost, key, "grid-applied") == []
        assert geometry(session, tab) == view
        desk.close()


def test_a21_a_grid_behind_a_held_payload_lands_in_order(roost, session_env):
    """A deep history makes the window's payload long, and its data
    connection is held while the desk resizes the tab: the `GRID` waits
    in the stream behind the payload it follows. Released, the mirror
    ends at the PTY's grid, showing what the session shows."""
    require_test_mode(roost)
    start_session(session_env)
    path = session_env.socket.parent / "own.sock"
    with Relay(session_env.socket, path) as relay, saved_host(
        roost, session_env, target=path
    ) as under_test, connected(under_test) as session:
        tab, other = quiet_tab(under_test.env, session), quiet_tab(under_test.env, session)
        key = shown(roost, tab)
        history = "".join(f"history row {n:05} {'x' * 60}\r\n" for n in range(6000))
        session.tab_feed_pty_bytes(tab, history.encode())
        focus(roost, sibling_key(key, other))
        wait_mine(roost, "the other tab")
        desk = raw(under_test.env, session, tab, "desk-a21h", True, DESK)
        release = relay.hold(DATA)
        try:
            focus(roost, key)
            took = next_grid(desk)
            assert took["owner"]["client_id"] != "desk-a21h", took
            desk.send_take()
            desk.send_resize(DESK[0] + 3, DESK[1] + 2, focused=True)
            next_grid(desk)
        finally:
            release.set()
        final = (DESK[0] + 3, DESK[1] + 2)
        wait_mirror(roost, final, "the held GRID applied")
        assert geometry(session, tab) == final
        wait_until(
            lambda: dump(roost, key)["rows_text"]
            == session.dump(tab)["rows_text"],
            30.0,
            "the window to show the session's screen",
        )
        desk.close()


# ---------------------------------------------------------------------------
# A18: Take size
# ---------------------------------------------------------------------------


def test_a18_take_size_from_the_ui_socket_the_palette_and_roostctl(host, roost):
    with connected(host) as session:
        tab = quiet_tab(host.env, session)
        key = shown(roost, tab)
        view = view_grid(roost)
        desk = raw(host.env, session, tab, "desk-a18", False, DESK)

        desk_takes(desk, roost, "before tab.take_size")
        taken = roost.call("tab.take_size", {"tab_id": key})
        assert (taken["cols"], taken["rows"]) == view, taken
        wait_mine(roost, "tab.take_size")

        desk_takes(desk, roost, "before the palette")
        palette_command(roost, "take_size")
        wait_mine(roost, "the palette's Take Size")
        wait_geometry(session, tab, view, "the palette's Take Size")

        desk_takes(desk, roost, "before roostctl")
        done = host.env.roostctl("--socket", str(roost.path), "tab", "take-size", "--tab", key)
        assert done.returncode == 0, done.stderr
        assert done.stdout.strip() == f"{view[0]}x{view[1]}"
        wait_mine(roost, "roostctl tab take-size")
        desk.close()


def test_a18_take_size_of_an_in_process_tab_answers_its_own_grid(roost, project):
    """An in-process tab always follows the window, so it is sized by it
    already: the op answers that grid, in the shape a host tab's has."""
    tab = roost.open_tab(project, cwd="/tmp")

    def answers_its_grid():
        taken = roost.call("tab.take_size", {"tab_id": str(tab)})
        shown = dump(roost, str(tab))
        return taken == {"cols": shown["cols"], "rows": shown["rows"]}

    # Polled: the window may still be laying the new tab out.
    wait_until(answers_its_grid, 30.0, "tab.take_size to answer the tab's own grid")


def test_a18_take_size_of_a_tab_that_is_not_there_is_not_found(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    roost.close_tab(tab)
    with pytest.raises(RoostError) as refused:
        roost.call("tab.take_size", {"tab_id": str(tab)})
    assert refused.value.code == "not-found", refused.value


# ---------------------------------------------------------------------------
# A19: the frame at its place, whichever leg is late
# ---------------------------------------------------------------------------


def test_a19_the_mirror_follows_the_data_plane_not_the_event_stream(roost, session_env):
    """With the window's event stream held the mirror still follows the
    `GRID` on its data connection; with the data connection held, the
    `tab.resized` the event stream delivers moves nothing until the data
    plane catches up."""
    require_test_mode(roost)
    start_session(session_env)
    path = session_env.socket.parent / "own.sock"
    with Relay(session_env.socket, path) as relay, saved_host(
        roost, session_env, target=path
    ) as under_test, connected(under_test) as session:
        tab = quiet_tab(under_test.env, session)
        key = shown(roost, tab)
        view = view_grid(roost)
        desk = raw(under_test.env, session, tab, "desk-a19", False, DESK)

        desk_resized = (
            b'"tab.resized"',
            f'"cols":{DESK[0]},'.encode(),
            f'"rows":{DESK[1]},'.encode(),
        )
        held = relay.hold(EVENTS)
        try:
            desk.send_take()
            wait_mirror(roost, DESK, "GRID with the event stream held")
        finally:
            held.set()
        # The first half's own `tab.resized` names the same grid as the
        # second's: it is passed back before the mark below, so only the
        # second's can satisfy the wait after it.
        relay.wait_pushed(*desk_resized)

        roost.call("tab.take_size", {"tab_id": key})
        wait_mirror(roost, view, "the take back")
        held = relay.hold(DATA)
        try:
            since = len(relay.pushed)
            desk.send_take()
            relay.wait_pushed(*desk_resized, since=since)
            no_change_until = time.monotonic() + 0.5
            while time.monotonic() < no_change_until:
                assert mirror(roost) == view, "tab.resized alone moved the mirror"
                time.sleep(0.02)
        finally:
            held.set()
        wait_mirror(roost, DESK, "the released data plane")
        desk.close()


# ---------------------------------------------------------------------------
# A22: the offset view
# ---------------------------------------------------------------------------


def test_a22_a_grid_taller_than_the_view_is_drawn_and_addressed_through_its_offset(
    host, roost
):
    """The desk's grid is wider and taller than the window's view, with
    the cursor on its last row: the window draws it from the offset that
    keeps that row on screen, and a click at a grid cell reports that
    cell — the owner's coordinates, not the view's."""
    with connected(host) as session:
        tab = tab_running(host.env, session, [sys.executable, "-c", MOUSE_CHILD])
        key = shown(roost, tab)
        view = view_grid(roost)
        big = (view[0] + 20, view[1] + 10)
        desk = raw(host.env, session, tab, "desk-a22", True, big)
        on_desk = bytearray()
        wait_mirror(roost, big, "the desk's bigger grid")
        session.tab_feed_pty_bytes(tab, f"\x1b[{big[1]};1Hbottom".encode())
        wait_until(
            lambda: roost.window_metrics().get("grid_offset_rows") == big[1] - view[1],
            30.0,
            "the offset that keeps the cursor's row in view",
        )
        row = big[1] - 3
        roost.tab_dispatch_mouse_event(key, "press", "left", 5, row)
        roost.tab_dispatch_mouse_event(key, "release", "left", 5, row)
        child_says(desk, on_desk, f";6;{row + 1}M".encode())
        desk.close()
