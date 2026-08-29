"""The attach data plane, end to end against a real `roost-session`.

`test_session.py` proves the daemon serves the control plane headlessly.
This module proves the other half: that a client with no UI can take a
lease, negotiate an attach, and receive one tab's terminal over the
binary data plane — snapshot, live PTY frames, exit — with the fence,
lease, and resume rules the protocol promises.

Everything here drives a REAL daemon over a real Unix socket (see
`session.py` for the per-test profile isolation) and speaks the wire
through `dataplane.py`. Nothing decodes a GHOSTSNP snapshot: record tags
are scanned for ordering, and the semantic decode proof lives in the
Rust integration client (`crates/roost-session/tests/
attach_stream_test.rs`) so there is exactly one implementation of the
format.

The daemon runs with `ROOST_TEST_MODE=1` throughout, because the seeding
the fence and perf cases need is `tab.feed_pty_bytes` — bytes injected
into a tab's drain are indistinguishable from a busy child's, and they
make "2000 lines of scrollback" a deterministic setup instead of a race
with a shell.

Condition waits only. The two exceptions are deliberate and marked at
their call sites: an attach token's TTL and a soak's non-reading window
are both *durations under test*, not synchronization.
"""

from __future__ import annotations

import os
import re
import signal
import statistics
import subprocess
import threading
import time

import dataplane
import pytest
import session as sessionlib
from client import Roost, RoostError, scaled_timeout
from dataplane import DataPlane
from eventstream import EventStream

pytestmark = pytest.mark.session_daemon


# The attach geometry every test uses. Tabs are opened at this size so
# `tab.attach`'s resize is a no-op and a snapshot is encoded at the same
# geometry its content was laid out at.
COLS, ROWS = 80, 24

# `ROOST_TEST_TIMEOUT_SCALE`-relative budgets from plan 036 D9. The
# concurrent budget is the single-attach one doubled — the plan's
# "budget ×2" — stated once here rather than multiplied at the assertion.
READY_BUDGET = 0.5
CONCURRENT_READY_BUDGET = READY_BUDGET * 2
RSS_CEILING_MB = 500.0
# The dump-hammer thread has to actually be hammering for the "under
# load" samples to mean anything.
MIN_LOADER_DUMPS = 16


# ---------------------------------------------------------------------------
# Fixtures + the common prologue
# ---------------------------------------------------------------------------


@pytest.fixture
def env():
    made = sessionlib.make_env()
    try:
        yield made
    finally:
        made.teardown()


def started(env, **overrides) -> sessionlib.Launch:
    """Daemonize a session in test mode and assert it came up."""
    launch = env.start_daemonized(ROOST_TEST_MODE="1", **overrides)
    assert launch.returncode == 0, f"start failed: {launch.stdout!r} / {launch.stderr!r}"
    assert launch.verdict.kind == "ready", launch.verdict
    env.wait_answering()
    return launch


def first_project(client: Roost) -> int:
    return int(client.list()[0]["id"])


# ---------------------------------------------------------------------------
# Lease + ticket helpers
# ---------------------------------------------------------------------------


def connect_lease(client: Roost, takeover: bool = False) -> str:
    """`session.connect` — the lease every lease-gated op presents.

    The lease is a bearer credential: it is returned, never logged, and
    never interpolated into an assertion message.
    """
    return client.call("session.connect", {"takeover": takeover})["lease"]


def attach_ticket(
    client: Roost,
    lease: str,
    tab_id: int,
    cols: int = COLS,
    rows: int = ROWS,
    *,
    kinds: list[str] | None = None,
    libghostty_build: str | None = None,
) -> dict:
    """`tab.attach` — a single-use ticket for one data connection.

    `libghostty_build` defaults to whatever the session says it is:
    the pin identity is the server's to state, and a literal here would
    have to be updated every time `third_party/ghostty` moves. The
    mismatch case mutates this value on purpose.
    """
    if libghostty_build is None:
        libghostty_build = client.call("session.identify")["libghostty_build"]
    return client.call(
        "tab.attach",
        {
            "lease": lease,
            "tab_id": str(tab_id),
            "kinds": kinds if kinds is not None else [dataplane.GHOSTTY_SNAPSHOT],
            "cols": cols,
            "rows": rows,
            "cell_w_px": 0,
            "cell_h_px": 0,
            "libghostty_build": libghostty_build,
        },
    )


def dial(env, ticket: dict, **handshake) -> tuple[DataPlane, dataplane.Reply]:
    """Open a data connection and run the handshake with `ticket`'s token."""
    conn = DataPlane(env.socket)
    reply = conn.handshake(ticket["attach_token"], **handshake)
    return conn, reply


def attached(
    env, client: Roost, lease: str, tab_id: int
) -> tuple[DataPlane, dataplane.Reply, dict]:
    """The whole happy prologue: ticket, dial, accepted handshake."""
    ticket = attach_ticket(client, lease, tab_id)
    conn, reply = dial(env, ticket)
    assert reply.ok, (reply.code, reply.message)
    assert reply.kind == dataplane.GHOSTTY_SNAPSHOT
    return conn, reply, ticket


# ---------------------------------------------------------------------------
# Tabs
# ---------------------------------------------------------------------------


def open_tab(client: Roost, project: int, cwd, argv: list[str], title: str = "") -> int:
    return client.open_tab(
        project, cwd=str(cwd), title=title, cols=COLS, rows=ROWS, argv=argv
    )


def quiet_tab(client: Roost, project: int, cwd) -> int:
    """A tab parked on a child that never writes anything. `exec` keeps
    the pid so the session's reap accounts for exactly one process, and
    `sleep` takes the default SIGHUP action."""
    return open_tab(client, project, cwd, ["/bin/sh", "-c", "exec sleep 300"])


def flooding_tab(client: Roost, project: int, cwd) -> int:
    """A `yes`-style producer: a builtin `echo` in a shell loop, so it
    forks nothing and floods at memory speed."""
    return open_tab(client, project, cwd, ["/bin/sh", "-c", "while :; do echo spam; done"])


def seed_bytes(lines: int, prefix: str = "seed") -> bytes:
    body = "x" * 40
    return "".join(f"{prefix}-{i:04d} {body}\r\n" for i in range(lines)).encode()


def seed(client: Roost, tab_id: int, data: bytes, chunk: int = 32 * 1024) -> None:
    """Inject bytes into the tab's drain, split into JSON-comfortable
    chunks (the payload is base64, so a whole 2000-line seed in one
    request would be a needlessly large frame)."""
    for at in range(0, len(data), chunk):
        client.tab_feed_pty_bytes(tab_id, data[at : at + chunk])


def wait_dump_contains(client: Roost, tab_id: int, needle: str, timeout: float = 30.0) -> None:
    """Quiesce on content, not on a clock: the seed has landed when the
    server's own terminal shows its last line."""
    sessionlib.wait_until(
        lambda: needle in client.dump_text(tab_id),
        timeout,
        f"tab {tab_id} to show {needle!r}",
    )


def tab_ids(client: Roost) -> set[int]:
    return {int(tab["id"]) for tab in client.tabs()}


def rss_mb(pid: int) -> float:
    """Resident size of a pid, in MB. `ps -o rss=` reports KiB on both
    macOS and Linux, and psutil is not in the test dependency group."""
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True, check=False
    )
    raw = result.stdout.strip()
    return int(raw) / 1024.0 if raw.isdigit() else 0.0


def pty_payload(frames) -> bytes:
    return b"".join(
        frame.pty()[1] for frame in frames if frame.frame_type == dataplane.FRAME_PTY
    )


# ---------------------------------------------------------------------------
# 1. Leases: takeover closes everything the old client held
# ---------------------------------------------------------------------------


def test_a_takeover_closes_every_connection_the_old_lease_held(env):
    """One takeover, four consequences.

    The lease is what makes a host session single-driver, so the
    interesting assertion is not that a new client gets a lease — it is
    that the old one loses *all three* of its footholds at once: its
    control connection, its event stream, and its live data connection.
    A client left holding any of them would still believe it drives the
    session.

    The tombstone is the fourth: an op presenting the dead lease is told
    `taken-over` (someone else has it) rather than `connect-required`
    (you never connected), because those instruct differently.
    """
    started(env)

    old = env.client()
    old_lease = connect_lease(old)
    project = first_project(old)
    tab = quiet_tab(old, project, env.launch_cwd)

    stream = EventStream(env.socket, lease=old_lease)
    stream.subscribe()

    conn, reply, _ticket = attached(env, old, old_lease, tab)
    conn.read_until_ready()

    new = env.client()
    new_lease = connect_lease(new, takeover=True)
    assert len(new_lease) == 32

    # The data connection is told why it ended. EOF is the contract's
    # fallback only where the peer had stopped reading; this one is
    # draining, so the label is required.
    ending = conn.drain_to_close(timeout=30.0)
    assert ending.kind == "error", ending
    assert ending.code == "taken-over", ending
    conn.close()

    # The event stream gets the terminal control envelope, same reason.
    assert stream.recv_stopping(timeout=30.0) == "taken-over"
    stream.close()

    # And the connection that ran the original `session.connect` is gone.
    with pytest.raises((RoostError, OSError)):
        old.call("tab.list")
    old.close()

    # The dead lease is tombstoned, not forgotten.
    with env.client() as stale:
        with pytest.raises(RoostError) as refused:
            attach_ticket(stale, old_lease, tab)
        assert refused.value.code == "taken-over", refused.value

    # A lease outlives its holder's connection: dropping `new` releases
    # nothing, so the next client still has to say it means it.
    new.close()
    with env.client() as polite:
        with pytest.raises(RoostError) as busy:
            connect_lease(polite, takeover=False)
        assert busy.value.code == "already-connected", busy.value
        assert len(connect_lease(polite, takeover=True)) == 32

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 2. Handshake rejections are a JSON line and a close — never binary
# ---------------------------------------------------------------------------


def test_a_refused_handshake_is_one_json_line_and_a_close(env):
    """Bad token, reused token, wrong protocol version.

    The shared assertion is [`DataPlane.trailing`]: a refusal writes one
    line and closes, so a client that mis-parses the error can never go
    on to read a preamble that is not there. `trailing` is captured by
    the handshake helper itself, which is why every case below can state
    it in one line.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        # A token this session never issued.
        with DataPlane(env.socket) as bogus:
            reply = bogus.handshake("f" * 32)
            assert reply.ok is False, reply
            assert reply.code == "invalid-token", reply
            assert bogus.trailing == b"", bogus.trailing

        # A token that was already spent. Single-use is what keeps a
        # ticket from being a standing invitation.
        ticket = attach_ticket(client, lease, tab)
        with DataPlane(env.socket) as first:
            assert first.handshake(ticket["attach_token"]).ok
            first.read_until_ready()
        with DataPlane(env.socket) as replay:
            reply = replay.handshake(ticket["attach_token"])
            assert reply.ok is False, reply
            assert reply.code == "invalid-token", reply
            assert replay.trailing == b"", replay.trailing

        # Checked BEFORE the token: two ends that disagree about the
        # protocol disagree about what a token even is.
        fresh = attach_ticket(client, lease, tab)
        with DataPlane(env.socket) as ancient:
            reply = ancient.handshake(fresh["attach_token"], protocol_version=1)
            assert reply.ok is False, reply
            assert reply.code == "protocol-mismatch", reply
            assert ancient.trailing == b"", ancient.trailing

    env.stop_over_the_wire()


def test_an_expired_token_is_refused(env):
    """The TTL, tested in milliseconds.

    `ATTACH_TOKEN_TTL` is a 60-second protocol constant — not a test
    wait, so it is deliberately not timeout-scaled — and the only way to
    reach its expiry inside a test is the test-mode override the server
    honors under `ROOST_TEST_MODE=1`.

    The wait below is the one place this module sleeps: the elapsed
    time IS the thing under test, and there is no state to poll for
    "the token has aged out".
    """
    started(env, ROOST_SESSION_ATTACH_TTL_MS="50")

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        ticket = attach_ticket(client, lease, tab)

        time.sleep(scaled_timeout(0.4))

        with DataPlane(env.socket) as stale:
            reply = stale.handshake(ticket["attach_token"])
            assert reply.ok is False, reply
            assert reply.code == "invalid-token", reply
            assert stale.trailing == b"", stale.trailing

    env.stop_over_the_wire()


def test_tab_attach_refuses_what_it_cannot_serve(env):
    """The control-plane half of the validation matrix.

    Each code instructs differently — `unsupported-kind` and
    `build-mismatch` both mean "we cannot talk", `not-found` means "that
    tab is gone" — so each is asserted by code, not merely as a failure.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        build = client.call("session.identify")["libghostty_build"]

        with pytest.raises(RoostError) as unknown:
            attach_ticket(client, lease, tab, kinds=["vt", "sixel-mosaic"])
        assert unknown.value.code == "unsupported-kind", unknown.value

        # A list that MIXES an unknown kind with a servable one is fine:
        # the client states a preference order and the first servable
        # entry wins.
        mixed = attach_ticket(
            client, lease, tab, kinds=["vt", dataplane.GHOSTTY_SNAPSHOT]
        )
        assert mixed["kind"] == dataplane.GHOSTTY_SNAPSHOT

        with pytest.raises(RoostError) as mismatch:
            attach_ticket(client, lease, tab, libghostty_build=build + "-not-this-one")
        assert mismatch.value.code == "build-mismatch", mismatch.value
        # Both strings are named so a client can tell which side to move.
        assert build in mismatch.value.message

        with pytest.raises(RoostError) as zero:
            attach_ticket(client, lease, tab, cols=0, rows=ROWS)
        assert zero.value.code == "invalid-param", zero.value

        client.close_tab(tab)
        sessionlib.wait_until(
            lambda: tab not in tab_ids(client), 20.0, f"tab {tab} to close"
        )
        with pytest.raises(RoostError) as gone:
            attach_ticket(client, lease, tab)
        assert gone.value.code == "not-found", gone.value

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 3. The fence and the ordering it buys
# ---------------------------------------------------------------------------


def test_ready_leads_the_snapshot_and_the_seqs_are_contiguous(env):
    """READY first, then history, then live frames from the fence.

    READY is where a client can first render, so the server sends
    everything through it at full speed before it starts interleaving —
    a history page ahead of it would be catch-up the client cannot place
    yet. The tab is parked on `sleep`, so "no PTY frame before READY" is
    a real observation rather than a lucky one.

    Contiguity is not asserted here because `DataPlane` asserts it at
    every frame: the fence `S` from the reply means the next PTY frame is
    `S+1` and each one after it is exactly one more.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        seed(client, tab, seed_bytes(400))
        wait_dump_contains(client, tab, "seed-0399")

        conn, reply, _ticket = attached(env, client, lease, tab)
        assert reply.mode == "snapshot", reply
        conn.read_until_finish()

        # 400 lines into a 24-row viewport, so the scrollback behind
        # READY is real and its absence would be a regression rather
        # than a property of the tab.
        conn.snap.assert_ready_leads(expect_history=True)
        assert conn.snap.finish_seen
        assert conn.first_pty_at_frame is None, (
            "a quiesced tab produced PTY frames during its own snapshot"
        )

        # Past the fence: live bytes carry `S+1`, and `DataPlane` fails
        # the read itself if they do not.
        client.tab_feed_pty_bytes(tab, b"POST-FENCE-MARKER\r\n")
        frames = conn.read_frames_until(
            lambda f: b"POST-FENCE-MARKER" in pty_payload([f]),
            timeout=20.0,
            what="the post-fence marker",
        )
        live = [f for f in frames if f.frame_type == dataplane.FRAME_PTY]
        assert live[0].pty()[0] == reply.seq + 1, (live[0].pty()[0], reply.seq)
        conn.close()

    env.stop_over_the_wire()


def test_finish_arrives_under_a_flooding_producer(env):
    """A `yes`-style child cannot starve the snapshot.

    Live PTY traffic leads by design — a keystroke's echo must not queue
    behind a scrollback page — so without a floor an endless producer
    would defer FINISH forever. The floor is `PTY_BURST_BYTES` of payload
    or `SNAP_PROGRESS_INTERVAL`, whichever comes first, and this is what
    it buys.

    "Leads" begins at READY, not before it. Against a tab producing at
    memory speed this is the case where that distinction is visible: the
    pump absorbs tee records while the prefix is still going out and
    holds them (§4.3 step 3), so the first frames on the wire are the
    snapshot's, and the flood only starts appearing behind READY. Before
    that hold existed this tab put 40-plus PTY frames on the wire ahead
    of the first SNAP frame — frames a client had no terminal to apply
    yet — which is exactly what the ordering assertion below would have
    caught.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = flooding_tab(client, project, env.launch_cwd)
        # The producer is running before the attach, so the snapshot is
        # taken against a tab that is already spewing.
        wait_dump_contains(client, tab, "spam")

        conn, reply, _ticket = attached(env, client, lease, tab)
        assert reply.mode == "snapshot", reply
        conn.read_frames_until(
            lambda f: conn.snap.finish_seen or f.frame_type == dataplane.FRAME_ERROR,
            timeout=30.0,
            what="FINISH under sustained output",
        )
        assert conn.error is None, conn.error
        assert conn.snap.finish_seen
        assert conn.pty_bytes > 0, "the producer wrote nothing during the attach"
        # The flood reached the wire only behind READY. `DataPlane` fails
        # the read itself on a PTY frame ahead of the prefix; this states
        # the ordering directly, on the one tab busy enough for it to be
        # a real observation rather than a vacuous one.
        assert conn.ready_at_frame is not None
        assert conn.first_pty_at_frame is not None
        assert conn.first_pty_at_frame > conn.ready_at_frame, (
            conn.first_pty_at_frame,
            conn.ready_at_frame,
        )
        # And the prefix was one frame: the server cuts its first SNAP
        # frame exactly on the READY boundary, which is what keeps the
        # boundary observable on the wire instead of buried mid-frame.
        assert conn.snap_frames_at_ready == 1, conn.snap_frames_at_ready
        conn.close()

        # Bounded on purpose: the loop burns a core, so it goes away as
        # soon as the case has its answer.
        client.close_tab(tab)

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 4. Resume
# ---------------------------------------------------------------------------


def test_resume_replays_the_ring_with_no_snapshot(env):
    """The optimization, honored: the client asks for what it missed.

    A resume that is accepted sends no SNAP frames at all — the whole
    point is that the client already has the terminal and needs only the
    bytes it was away for. `mode` is how the server says which it chose,
    and a client never has to handle a resume *failure*.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        seed(client, tab, seed_bytes(100))
        wait_dump_contains(client, tab, "seed-0099")

        first, reply, ticket = attached(env, client, lease, tab)
        first.read_until_finish()
        resume_from = first.next_seq
        first.close()

        # Produced while nobody was attached — exactly what the ring is
        # for.
        client.tab_feed_pty_bytes(tab, b"MISSED-WHILE-AWAY\r\n")
        wait_dump_contains(client, tab, "MISSED-WHILE-AWAY")

        again = attach_ticket(client, lease, tab)
        conn, resumed = dial(
            env,
            again,
            resume_from_seq=resume_from,
            server_epoch=ticket["server_epoch"],
            tab_generation=ticket["tab_generation"],
        )
        assert resumed.ok, (resumed.code, resumed.message)
        assert resumed.mode == "resume", resumed
        assert resumed.seq == resume_from - 1, (resumed.seq, resume_from)

        frames = conn.read_frames_until(
            lambda f: b"MISSED-WHILE-AWAY" in pty_payload([f]),
            timeout=20.0,
            what="the bytes the client missed",
        )
        assert frames[0].pty()[0] == resume_from, (frames[0].pty()[0], resume_from)

        # "No snapshot" is a claim about the WHOLE connection, not about
        # the prefix that happened to reach the marker — a server that
        # started interleaving SNAP frames afterwards would satisfy a
        # mid-stream check and still be wrong. So the tab is ended and
        # the stream drained to its terminal frame before the count is
        # read.
        client.close_tab(tab)
        ending = conn.drain_to_close(timeout=30.0)
        assert ending.kind in {"exit", "eof"}, ending
        assert conn.snap_frames == 0, (
            f"a resume carried {conn.snap_frames} SNAP frames across its whole stream"
        )
        conn.close()

    env.stop_over_the_wire()


def test_a_restarted_daemon_never_resumes_a_stale_stream(env):
    """The restart case (plan 036 AC-4), made deterministic.

    A per-tab counter alone would collide across a restart — a fresh
    process would hand out generation 1 for a different pipeline and
    silently accept a stream from the old one. The `server_epoch` is
    random per process, so an old identity cannot match a new one and the
    server falls back to a full snapshot **in the same reply**, never an
    error.

    The trap this case has to avoid is proving nothing: a snapshot
    fallback is *also* what a seq the ring no longer covers produces, so
    a bare "stale identity → snapshot" assertion passes just as happily
    against a coincidental ring miss. So the same seq is resumed twice on
    the restarted daemon — once with the CORRECT identity, which must
    come back `resume` and thereby proves the ring covers it, and then
    with the stale one, which must come back `snapshot`. Identity is the
    only variable between the two answers.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        seed(client, tab, seed_bytes(50))
        wait_dump_contains(client, tab, "seed-0049")

        conn, _reply, old_ticket = attached(env, client, lease, tab)
        conn.read_until_finish()
        conn.close()

    env.stop_over_the_wire()
    env.wait_socket_gone()
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        # The layout rehydrates as fresh shells, so the ids are new: the
        # tab is found by the cwd it was restored into.
        restored = [t for t in client.tabs() if t["cwd"] == str(env.launch_cwd)]
        assert restored, client.tabs()
        tab = int(restored[0]["id"])

        first, reply, fresh_ticket = attached(env, client, lease, tab)
        first.read_until_finish()
        assert fresh_ticket["server_epoch"] != old_ticket["server_epoch"], (
            "a restarted session reused its predecessor's epoch"
        )
        first.close()

        # Ring content behind the seq both handshakes below will ask for.
        client.tab_feed_pty_bytes(tab, b"AFTER-RESTART\r\n")
        wait_dump_contains(client, tab, "AFTER-RESTART")
        resume_from = first.next_seq

        # Control: the correct identity resumes, which is what makes
        # `resume_from` demonstrably ring-covered. A resume does not
        # consume the ring, so the same seq is still covered below.
        honest, honest_reply = dial(
            env,
            attach_ticket(client, lease, tab),
            resume_from_seq=resume_from,
            server_epoch=fresh_ticket["server_epoch"],
            tab_generation=fresh_ticket["tab_generation"],
        )
        assert honest_reply.ok, (honest_reply.code, honest_reply.message)
        assert honest_reply.mode == "resume", honest_reply
        honest.close()

        # The case: same tab, same seq, same ring — only the identity is
        # the dead process's, and that alone forces the full snapshot.
        stale, stale_reply = dial(
            env,
            attach_ticket(client, lease, tab),
            resume_from_seq=resume_from,
            server_epoch=old_ticket["server_epoch"],
            tab_generation=old_ticket["tab_generation"],
        )
        assert stale_reply.ok, (stale_reply.code, stale_reply.message)
        assert stale_reply.mode == "snapshot", stale_reply
        stale.read_until_ready()
        assert stale.snap_frames > 0
        stale.close()

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 5. Lifecycle: exit, disconnect, stop, and a killed daemon
# ---------------------------------------------------------------------------


def test_exit_is_the_final_frame_on_a_natural_child_exit(env):
    """EXIT closes the connection and nothing follows it.

    The child is held on a `read` so the attach is established *before*
    the exit — the input that releases it goes down the data plane as an
    `INPUT` frame, which is also what proves the client → server
    direction is wired at all.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = open_tab(
            client,
            project,
            env.launch_cwd,
            ["/bin/sh", "-c", "read line; echo done; exit 0"],
        )

        conn, reply, _ticket = attached(env, client, lease, tab)
        conn.read_until_finish()

        conn.send_input(b"go\n")
        ending = conn.drain_to_close(timeout=30.0)
        assert ending.kind == "exit", ending
        assert ending.exit_code == 0, ending
        assert b"done" in pty_payload(ending.frames)
        # `final_seq` is one past the last PTY record, which is what lets
        # a client tell "I have everything" from "I am missing the tail".
        assert ending.final_seq == conn.last_seq + 1, (ending.final_seq, conn.last_seq)
        conn.close()

        sessionlib.wait_until(
            lambda: tab not in tab_ids(client), 20.0, f"tab {tab} to close on its own"
        )

    env.stop_over_the_wire()


def test_a_disconnect_leaves_the_tab_running_but_a_stop_labels_it(env):
    """Detaching is not stopping.

    A data connection is a *view*: dropping it must leave the tab and its
    child exactly where they were, which is the whole premise of a host
    session outliving its client. A `session.stop`, by contrast, is the
    end of the tab — and it says so, with the matching ERROR code, rather
    than dropping the socket and leaving the client to guess.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        conn, _reply, _ticket = attached(env, client, lease, tab)
        conn.read_until_ready()
        conn.close()

        assert tab in tab_ids(client), "closing a data connection closed the tab"
        assert client.dump_text(tab) is not None

        # And the tab is still attachable, so nothing about the detach
        # left the pipeline in a half state.
        conn, _reply, _ticket = attached(env, client, lease, tab)
        conn.read_until_ready()

        # Mid-attach, from another connection: the stop runs while this
        # data connection is live and reading.
        stopped: list[dict] = []
        stopper = threading.Thread(target=lambda: stopped.append(env.stop_over_the_wire()))
        stopper.start()
        try:
            ending = conn.drain_to_close(timeout=60.0)
        finally:
            stopper.join(timeout=scaled_timeout(90.0))
        assert not stopper.is_alive(), "session.stop never returned"
        assert stopped, "the stop produced no reap report"
        assert ending.kind == "error", ending
        assert ending.code == "shutting-down", ending
        conn.close()

    env.wait_socket_gone()


def test_a_sigkilled_daemon_drops_the_attach_and_a_restart_recovers(env):
    """No finalizer runs, so EOF is the only signal — and it is enough.

    The labeled ERROR is best-effort by contract; a killed process cannot
    write one, and a client that treated a bare close as a protocol
    violation would be unable to survive the one failure mode it is
    guaranteed to meet.
    """
    launch = started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        conn, _reply, _ticket = attached(env, client, lease, tab)
        conn.read_until_ready()

    os.kill(launch.verdict.pid, signal.SIGKILL)
    env.wait_pid_gone(launch.verdict.pid)

    ending = conn.drain_to_close(timeout=30.0)
    assert ending.kind == "eof", ending
    conn.close()

    started(env)
    assert env.identify()["session_id"]
    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 6. The server terminal is authoritative with nobody attached
# ---------------------------------------------------------------------------

CPR_PATTERN = re.compile(rb"\x1b\[\d+;\d+R")


def test_a_terminal_query_is_answered_exactly_once_with_no_client(env):
    """A child asks where the cursor is; the *server* answers.

    This is the deviation the architecture left open in HS-1a: the tab's
    terminal is real and running whether or not anyone is looking at it,
    so a device query gets an answer with no client attached at all — and
    exactly one answer, because there is exactly one terminal that may
    speak for the tab.

    The cursor is parked at a fixed cell first so the reply has a known
    length and a known body; `dd` (rather than `cat`) is the reader
    because it writes its 6 bytes and exits, which flushes the file
    instead of leaving them in a block buffer.
    """
    started(env)

    answer = env.root / "cpr.bin"
    script = (
        "stty raw -echo; "
        "printf '\\033[9;9H\\033[6n'; "
        f"dd bs=1 count=6 of='{answer}' 2>/dev/null; "
        "exec sleep 300"
    )

    with env.client() as client:
        project = first_project(client)
        tab = open_tab(client, project, env.launch_cwd, ["/bin/sh", "-c", script])

        sessionlib.wait_until(
            lambda: answer.is_file() and answer.stat().st_size == 6,
            30.0,
            "the child to receive its cursor-position report",
        )
        assert answer.read_bytes() == b"\x1b[9;9R", answer.read_bytes()

        # Exactly once, from the other side: everything the session has
        # queued toward the child carries one reply, not two.
        queued = client.tab_capture_pty_input(tab, drain=False)
        assert len(CPR_PATTERN.findall(queued)) == 1, queued

    env.stop_over_the_wire()


def test_dumps_are_served_headless_from_the_server_terminal(env):
    """`tab.dump` and `tab.dump_resolved` on a session socket.

    Both walk the same server terminal the data plane snapshots, which is
    what makes them usable as the oracle the Rust client compares its
    decode against. The resolved form is the interesting one: it runs the
    production color resolver, so an SGR that never reached a renderer
    still resolves to the colors a renderer would paint.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        client.tab_feed_pty_bytes(tab, b"PLAIN-MARKER\r\n\x1b[31mRED-MARKER\x1b[0m\r\n")
        wait_dump_contains(client, tab, "RED-MARKER")

        dump = client.dump(tab)
        assert (dump["cols"], dump["rows"]) == (COLS, ROWS), dump
        assert any("PLAIN-MARKER" in row for row in dump["rows_text"]), dump["rows_text"]

        resolved = client.tab_dump_resolved(tab)
        assert (resolved["cols"], resolved["rows"]) == (COLS, ROWS), resolved
        cells = {(c["row"], c["col"]): c for c in resolved["cells"]}
        plain_row = next(i for i, row in enumerate(dump["rows_text"]) if "PLAIN-MARKER" in row)
        red_row = next(i for i, row in enumerate(dump["rows_text"]) if "RED-MARKER" in row)
        plain = cells[(plain_row, 0)]
        red = cells[(red_row, 0)]
        assert plain["text"] == "P", plain
        assert red["text"] == "R", red
        assert red["fg"] != plain["fg"], (red, plain)

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 7. Perf budgets (plan 036 D9 / AC-6)
# ---------------------------------------------------------------------------


def measure_ready(env, client: Roost, lease: str, tab: int) -> float:
    """Seconds from handshake-send to the READY tag landing.

    The ticket is minted outside the window on purpose: what a user waits
    for is the stream, and the control round trip that precedes it is
    already covered by every other case here.
    """
    ticket = attach_ticket(client, lease, tab)
    conn = DataPlane(env.socket)
    try:
        started_at = time.monotonic()
        reply = conn.handshake(ticket["attach_token"])
        assert reply.ok, (reply.code, reply.message)
        conn.read_until_ready(timeout=30.0)
        return time.monotonic() - started_at
    finally:
        conn.close()


def test_attaching_to_a_2000_line_tab_reaches_ready_fast(env):
    """The budget that makes an attach feel instant.

    Median of three, not a single sample: the first attach on a fresh
    process pays for page faults and a cold allocator, and a budget that
    a warm system meets is the one a user experiences.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        seed(client, tab, seed_bytes(2000))
        wait_dump_contains(client, tab, "seed-1999", timeout=60.0)

        samples = [measure_ready(env, client, lease, tab) for _ in range(3)]

    median = statistics.median(samples)
    budget = scaled_timeout(READY_BUDGET)
    assert median < budget, (
        f"attach → READY median {median * 1000:.0f} ms over the {budget * 1000:.0f} ms "
        f"budget (samples {[round(s * 1000) for s in samples]} ms)"
    )
    print(f"\nattach→READY (2000 lines): median {median * 1000:.2f} ms, "
          f"samples {[round(s * 1000, 2) for s in samples]} ms")

    env.stop_over_the_wire()


def test_eight_concurrent_attaches_to_distinct_tabs_reach_ready(env):
    """Eight tabs, eight clients, at once.

    Distinct tabs on purpose: the snapshot semaphore bounds how many tabs
    encode at a time session-wide, so eight attaches to one tab would
    measure a queue behind a single encode rather than the concurrency
    the budget is about. The budget is the plan's single-attach 500 ms
    doubled — one whole second, which is what absorbs that semaphore's
    two waves.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tabs = []
        for index in range(8):
            tab = quiet_tab(client, project, env.launch_cwd)
            seed(client, tab, seed_bytes(200, prefix=f"t{index}"))
            tabs.append(tab)
        for index, tab in enumerate(tabs):
            wait_dump_contains(client, tab, f"t{index}-0199", timeout=60.0)

        # Minted up front so the measured window is the data plane alone
        # and eight threads do not contend on one control connection.
        tickets = [attach_ticket(client, lease, tab) for tab in tabs]

    elapsed: dict[int, float] = {}
    failures: list[Exception] = []
    barrier = threading.Barrier(len(tickets))

    def run(index: int, ticket: dict) -> None:
        conn = DataPlane(env.socket)
        try:
            barrier.wait(timeout=scaled_timeout(30.0))
            started_at = time.monotonic()
            reply = conn.handshake(ticket["attach_token"])
            assert reply.ok, (reply.code, reply.message)
            conn.read_until_ready(timeout=60.0)
            elapsed[index] = time.monotonic() - started_at
        except Exception as error:  # noqa: BLE001 — re-raised on the main thread
            failures.append(error)
        finally:
            conn.close()

    threads = [
        threading.Thread(target=run, args=(index, ticket))
        for index, ticket in enumerate(tickets)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=scaled_timeout(120.0))
        assert not thread.is_alive(), "a concurrent attach never finished"

    assert not failures, failures
    assert len(elapsed) == len(tickets), elapsed
    budget = scaled_timeout(CONCURRENT_READY_BUDGET)
    worst = max(elapsed.values())
    assert worst < budget, (
        f"the slowest of {len(elapsed)} concurrent attaches took {worst * 1000:.2f} ms, "
        f"over the {budget * 1000:.0f} ms budget "
        f"({sorted(round(v * 1000, 2) for v in elapsed.values())} ms)"
    )
    print(f"\n8 concurrent attach→READY: max {worst * 1000:.2f} ms, "
          f"all {sorted(round(v * 1000, 2) for v in elapsed.values())} ms")

    env.stop_over_the_wire()


def test_input_latency_holds_up_under_dump_load(env):
    """A keystroke's echo must not queue behind a control-plane read.

    The tab task serves `tab.dump` and the data plane's input from the
    same task, so a client hammering dumps is exactly the contention that
    would show up as typing lag. The pinned formula has an absolute floor
    as well as a ratio: a baseline near zero would otherwise make the
    ratio alone fire on scheduler noise.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = open_tab(client, project, env.launch_cwd, ["/bin/sh", "-c", "exec cat"])
        conn, _reply, _ticket = attached(env, client, lease, tab)
        conn.read_until_finish()
        drain_pending(conn)

        base = [round_trip(conn) for _ in range(32)]

        stop = threading.Event()
        # The load has to have STARTED before the "under load" samples
        # begin: a thread that is still connecting would let the first
        # samples ride an idle tab task and quietly flatter the p50.
        loading = threading.Event()
        hammering: list[Exception] = []
        dumps = 0

        def hammer() -> None:
            nonlocal dumps
            try:
                with env.client() as loader:
                    while not stop.is_set():
                        loader.dump(tab)
                        dumps += 1
                        loading.set()
            except (RoostError, OSError) as error:
                hammering.append(error)
                loading.set()

        loader = threading.Thread(target=hammer)
        loader.start()
        try:
            assert loading.wait(timeout=scaled_timeout(30.0)), (
                "the dump loader never completed its first tab.dump"
            )
            during = [round_trip(conn) for _ in range(32)]
        finally:
            stop.set()
            loader.join(timeout=scaled_timeout(30.0))
        assert not loader.is_alive(), "the dump loader never stopped"
        assert not hammering, hammering
        assert dumps > MIN_LOADER_DUMPS, (
            f"the loader served only {dumps} dumps during the measured window; "
            "the 'under load' samples were not actually under load"
        )
        conn.close()

    p50_base = statistics.median(base)
    p50_during = statistics.median(during)
    ceiling = max(3 * p50_base, p50_base + 0.020)
    print(f"\ninput latency p50: baseline {p50_base * 1000:.2f} ms, "
          f"under tab.dump load {p50_during * 1000:.2f} ms "
          f"(ceiling {ceiling * 1000:.2f} ms, {dumps} dumps served)")
    assert p50_during < ceiling, (
        f"input latency p50 rose to {p50_during * 1000:.2f} ms under load, past the "
        f"{ceiling * 1000:.2f} ms ceiling (baseline {p50_base * 1000:.2f} ms)"
    )

    env.stop_over_the_wire()


def drain_pending(conn: DataPlane, timeout: float = 0.5) -> None:
    """Read whatever is already on the wire, so a latency sample times
    the byte it sent rather than one that was already in flight."""
    while True:
        try:
            conn.read_frame(timeout=timeout)
        except TimeoutError:
            return


def round_trip(conn: DataPlane) -> float:
    """One `INPUT` byte, timed to the PTY frame that echoes it."""
    started_at = time.monotonic()
    conn.send_input(b"a")
    conn.read_frames_until(
        lambda f: f.frame_type == dataplane.FRAME_PTY,
        timeout=20.0,
        what="the echo of one input byte",
    )
    return time.monotonic() - started_at


# ---------------------------------------------------------------------------
# 8. Soak: a producer that floods and a client that stops reading
# ---------------------------------------------------------------------------


def test_a_slow_reader_is_cut_off_and_a_re_attach_succeeds(env):
    """A client that stops reading is severed, and the session shrugs.

    Three endings are legal here and the test accepts all three, because
    which one a run gets is a property of how fast the producer outran
    the socket, not of the server's correctness:

    * ERROR `overflow` — the forwarder was holding more than
      `FORWARDER_QUEUE_BYTES` for a peer that is not reading;
    * ERROR `desync` — the write stalled long enough that the tab's tee
      lapped the forwarder, which the design pins as **fatal** rather
      than skip-and-continue, because a client's terminal would
      otherwise silently diverge (a re-attach is the recovery, and the
      case below performs one);
    * EOF — the labeled final write was itself impossible, which the
      contract accepts wherever the peer's buffer is already full.

    On this hardware the producer is fast enough that `desync` is the
    usual outcome: the tee laps within milliseconds of the first stalled
    write, well before the 8 MiB queue cap can be reached. Both codes
    name the same event — this connection could not keep up and has been
    cut — so pinning only one of them would make the test a hardware
    speed check.

    What must NOT happen is unbounded growth, or a session left too sick
    to serve the next attach — so RSS is sampled through the flood and an
    immediate re-attach has to succeed.

    The non-reading window is the test condition, not synchronization:
    the client is *supposed* to stall, and the RSS sampler is what paces
    it.
    """
    launch = started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = flooding_tab(client, project, env.launch_cwd)
        wait_dump_contains(client, tab, "spam")

        conn, reply, _ticket = attached(env, client, lease, tab)
        assert reply.ok

        # Read nothing while the producer runs. Sampling every 250 ms
        # for a bounded window, exactly as the plan's soak specifies.
        peak = 0.0
        for _ in range(12):
            time.sleep(0.25)
            peak = max(peak, rss_mb(launch.verdict.pid))
        assert 0 < peak < RSS_CEILING_MB, f"session RSS peaked at {peak:.1f} MB"

        ending = conn.drain_to_close(timeout=60.0)
        assert ending.kind in {"error", "eof"}, ending
        if ending.kind == "error":
            assert ending.code in {"overflow", "desync"}, ending
        conn.close()

        # No thrash loop: the very next attach is served normally.
        again, reply, _ticket = attached(env, client, lease, tab)
        assert reply.ok
        again.read_until_ready(timeout=60.0)
        again.close()

        client.close_tab(tab)

    print(
        f"\nsoak: session RSS peak {peak:.1f} MB (ceiling {RSS_CEILING_MB:.0f} MB), "
        f"ended {ending.kind}{'/' + ending.code if ending.code else ''} "
        f"after {conn.pty_bytes} PTY bytes"
    )
    env.stop_over_the_wire()
