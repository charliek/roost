"""The daemon's client-facing additions, end to end against a real
`roost-session` (plan 037 §3.6 + §3.7, plan 038 C6).

Four things the daemon gained so an attached client can be a faithful
terminal without owning the terminal:

* **`tab.effect` events** — a bell and an OSC 52 clipboard write are
  client-local effects. A session has no view of its own, so it fans
  them out on the event stream instead of dropping them (which is what
  HS-1b did). The clipboard payload is capped at 256 KiB decoded and
  oversized writes are dropped whole, never truncated.
* **`session.set_theme`** — the attached client's palette, applied to
  every tab's server terminal, so the OSC 4 / 10 / 11 / 12 queries the
  terminal answers itself carry the colors the user is actually looking
  at. It applies to the tabs that exist and is remembered for the ones
  opened next.
* **Notifications fan out** (#474) — a session has no window, so it
  suppresses nothing: every raise sets the pending bit and pushes
  `notification.fired` to every subscriber. The client that is showing
  that tab answers for everyone with a generation-checked
  `tab.clear_notification`; a stale acknowledgement is refused, so a
  raise nobody has seen survives an answer to the one before it. The op
  a session used to be told what to mute with, `session.set_focus`, is
  gone.
* **`ROOST_SESSION_FAKE_BUILD`** — the test seam that makes
  `tab.attach`'s build-mismatch refusal reproducible without building a
  second binary against a second Ghostty pin. Strictly test-mode.

Everything here drives a REAL daemon over a real Unix socket (per-test
profile isolation lives in `session.py`) and reads events through
`eventstream.py`. The daemon runs with `ROOST_TEST_MODE=1` because the
seeding is `tab.feed_pty_bytes`: bytes injected into a tab's drain are
indistinguishable from a busy child's, which makes "ring the bell" a
one-line setup rather than a race with a shell.

Condition waits only.
"""

from __future__ import annotations

import base64
import re
from pathlib import Path

import pytest
import session as sessionlib
from client import Roost, RoostError
from eventstream import EventStream
from util import drain, drain_until_match

pytestmark = pytest.mark.session_daemon


# The geometry tabs are opened at. Matches `test_session_attach.py` so an
# attach here needs no resize either.
COLS, ROWS = 80, 24

# Decoded-size cap on a clipboard-write effect
# (`roost_ipc::messages::CLIPBOARD_EFFECT_MAX_BYTES`). Restated rather
# than imported: this harness is a client, and a client's copy of a
# server constant drifting apart is exactly what the cases below catch.
CLIPBOARD_CAP = 256 * 1024

# What `ROOST_SESSION_FAKE_BUILD` puts on the wire. Shaped like a real
# build string but impossible to mistake for one.
FAKE_BUILD = "ghostty-0000000000000000+fake.plan037"


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


def quiet_tab(client: Roost, project: int, cwd) -> int:
    """A tab parked on a child that never writes anything, so every byte
    in its stream is one this test put there."""
    return client.open_tab(
        project,
        cwd=str(cwd),
        cols=COLS,
        rows=ROWS,
        argv=["/bin/sh", "-c", "exec sleep 300"],
    )


def osc52(payload: bytes, selector: str = "c") -> bytes:
    """One OSC 52 clipboard write carrying `payload`."""
    encoded = base64.b64encode(payload).decode("ascii")
    return f"\x1b]52;{selector};{encoded}\x07".encode()


def next_effect(
    stream: EventStream, start: int, timeout: float = 30.0
) -> tuple[dict, int]:
    """The next `tab.effect` envelope's data, plus the fence to continue
    from, with the batches it arrived in checked for holes.

    The contiguity assert is not incidental: an effect is an ordinary
    commit on this stream, so a client's gap check has to keep working
    across it.
    """
    batches, envelope = stream.recv_until("tab.effect", timeout=timeout)
    stream.expect_contiguous(batches, start)
    return envelope["data"], int(batches[-1]["revision"])


# ---------------------------------------------------------------------------
# 1. Effects: bell + OSC 52, and the cap
# ---------------------------------------------------------------------------


def test_a_bell_and_a_clipboard_write_arrive_as_tab_effect_events(env):
    """The two effects HS-2 ships, on the stream a client already reads.

    Both ride inside an ordinary `EventBatch`, which is what makes them
    additive: a client one release behind sees an event name it does not
    know and ignores it, and the revision sequence it fences on is
    unbroken either way.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket) as stream:
            fence = stream.subscribe()

            # A bare BEL rings. The OSC that follows it ends with a BEL
            # too — a terminator, not a bell — so a scanner that counted
            # bytes instead of tracking state would report two.
            client.tab_feed_pty_bytes(tab, b"\x07\x1b]0;titled\x07")
            data, fence = next_effect(stream, fence)
            assert data["effect"] == "bell", data
            assert data["tab_id"] == str(tab), data
            assert "data" not in data, f"a bell carries no payload: {data}"

            client.tab_feed_pty_bytes(tab, osc52(b"hello"))
            data, fence = next_effect(stream, fence)
            assert data["effect"] == "clipboard-write", data
            assert data["tab_id"] == str(tab), data
            assert base64.b64decode(data["data"]) == b"hello", data
            assert data["target"] == "system", data

            # `p` is the primary selection, and it stays distinguishable
            # on the wire: applying it to the system clipboard would let
            # a mouse selection in a host tab clobber what the user
            # copied.
            client.tab_feed_pty_bytes(tab, osc52(b"selected", selector="p"))
            data, fence = next_effect(stream, fence)
            assert data["effect"] == "clipboard-write", data
            assert data["target"] == "selection", data

        client.call("session.stop")


def batches_through(stream: EventStream, revision: int, timeout: float = 30.0) -> list[dict]:
    """Every batch up to and including `revision`.

    A non-batch envelope on the way is read and skipped: it carries no
    revision, so it cannot advance the count.

    Contiguity is the **caller's** check: this returns what it read, and
    a caller hands that to `EventStream.expect_contiguous` with the
    revision it was last fenced at. Doing it here would need that fence,
    which only the caller has.
    """
    seen: list[dict] = []
    while True:
        frame = stream.recv_frame(timeout=timeout)
        if "revision" not in frame:
            assert frame.get("event") != "session.stopping", (
                f"the stream ended ({stream.stopping_reason}) before revision {revision}"
            )
            continue
        seen.append(frame)
        if int(frame["revision"]) >= revision:
            return seen


def test_two_streams_receive_the_same_effect(env):
    """Every subscriber receives every effect, over a real daemon and a
    real PTY drain.

    A session has no view of its own, so it cannot decide whose bell a
    bell is or whose clipboard an OSC 52 write is for: it publishes the
    fact in commit order and each client applies it to the tab it is
    showing (the viewed-tab rule, unit-tested client-side).

    Asserted on the *payload*, not just the event name: a fan-out that
    delivered a different revision, or a different tab, would be a
    different bug wearing the same shape.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket) as first, EventStream(env.socket) as second:
            first_fence = first.subscribe()
            second_fence = second.subscribe()

            client.tab_feed_pty_bytes(tab, osc52(b"a secret"))
            here, revision = next_effect(first, first_fence)
            there, also = next_effect(second, second_fence)
            assert here["effect"] == "clipboard-write", here
            assert here == there, (here, there)
            assert here["tab_id"] == str(tab), here
            assert revision == also, (revision, also)

            # And a bell, so the rule is not one payload kind's.
            client.tab_feed_pty_bytes(tab, b"\x07")
            here, revision = next_effect(first, revision)
            there, also = next_effect(second, also)
            assert here["effect"] == "bell", here
            assert here == there and revision == also, (here, there)

        client.call("session.stop")


def test_a_notification_reaches_a_watching_subscriber(env):
    """`notification.fired` reaches a subscriber that does nothing else
    with the session.

    Routing notifications for AI coding agents is the point of watching
    a session at all — a phone that could see every title change but no
    notification would be watching the wrong half.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket) as watcher:
            fence = watcher.subscribe()
            client.call(
                "notification.create",
                {"tab_id": str(tab), "title": "agent", "body": "needs you"},
            )
            batches, envelope = watcher.recv_until("notification.fired", timeout=30.0)
            watcher.expect_contiguous(batches, fence)
            assert envelope["data"]["tab_id"] == str(tab), envelope
            assert envelope["data"]["title"] == "agent", envelope

        client.call("session.stop")


def test_an_oversized_clipboard_write_produces_no_effect(env):
    """The 256 KiB cap, proven without waiting on a clock.

    An oversized write followed by a small one: if the cap leaked, the
    next effect off the stream would be the big payload. Reading until
    the sentinel arrives is therefore a positive assertion about the
    dropped one, not a timeout.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket) as stream:
            fence = stream.subscribe()

            # One byte over, then the sentinel. The at-cap side of the
            # boundary is pinned in the Rust unit test, where the
            # payload does not have to cross a JSON frame.
            client.tab_feed_pty_bytes(tab, osc52(b"x" * (CLIPBOARD_CAP + 1)))
            client.tab_feed_pty_bytes(tab, osc52(b"sentinel"))

            data, _fence = next_effect(stream, fence)
            assert data["effect"] == "clipboard-write", data
            assert base64.b64decode(data["data"]) == b"sentinel", (
                "the oversized write was fanned out instead of dropped"
            )

        client.call("session.stop")


# ---------------------------------------------------------------------------
# 2. session.set_theme
# ---------------------------------------------------------------------------


def theme(background: str = "#1c2b3a") -> dict:
    """A full palette with a recognizable background. Whole-theme, not a
    diff: the client states what it renders with and the server takes
    it."""
    return {
        "foreground": "#ffffff",
        "background": background,
        "cursor": "#98989d",
        "palette": [f"#{i:02x}{i:02x}{i:02x}" for i in range(256)],
    }


def expect_background(client: Roost, tab: int, expected: str) -> None:
    """Ask the tab's terminal what its background is, the way a program
    in the PTY would, and wait until the answer carries `expected`.

    The reply is libghostty's own (`write_pty`), so this is the whole
    point of reseeding server-side: whatever the terminal holds is what
    a program is told.

    Waits for the colour itself rather than for any bytes at all: a
    capture can return a half-arrived reply, which is the flake
    `drain_until_match` was consolidated to prevent.
    """
    drain(client, tab)
    client.tab_feed_pty_bytes(tab, b"\x1b]11;?\x07")
    drain_until_match(client, tab, re.escape(expected.encode("ascii")), timeout=30.0)


def test_set_theme_changes_what_a_color_query_is_answered_with(env):
    """The reseed reaches the terminal, not just a mirror of it.

    Before HS-2 a host session answered every color query with the
    headless white-on-black default, so a program in a host tab picked
    its colors against a theme nobody was looking at.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        # The headless default is white on black.
        expect_background(client, tab, "0000/0000/0000")

        result = client.call("session.set_theme", {"osc_colors": theme()})
        # Every live tab, not just the one this test opened — the
        # hydrated layout brought its own, and a theme that reached only
        # the newest tab would leave the rest on the headless default.
        assert result["tabs"] == len(client.tabs()), (result, client.tabs())

        expect_background(client, tab, "1c1c/2b2b/3a3a")

        # A tab opened AFTER the theme landed starts on it: the seed is
        # session-wide, so there is no second class of tab rendering the
        # client's colors wrong until the next reseed.
        later = quiet_tab(client, project, env.launch_cwd)
        expect_background(client, later, "1c1c/2b2b/3a3a")

        # Last writer wins, and it is a whole theme each time.
        client.call(
            "session.set_theme",
            {"osc_colors": theme(background="#0a0b0c")},
        )
        expect_background(client, tab, "0a0a/0b0b/0c0c")

        client.call("session.stop")


def test_set_theme_validates_its_palette(env):
    """A palette that is not 256 entries is refused whole rather than
    applied halfway.

    No authority is asked for: `set_theme` is same-UID and
    last-writer-wins.
    """
    started(env)

    with env.client() as client:
        short = theme()
        short["palette"] = short["palette"][:8]
        with pytest.raises(RoostError) as bad:
            client.call("session.set_theme", {"osc_colors": short})
        assert bad.value.code == "invalid-param", bad.value

        malformed = theme(background="rgb:1c/2b/3a")
        with pytest.raises(RoostError) as unparsed:
            client.call("session.set_theme", {"osc_colors": malformed})
        assert unparsed.value.code == "invalid-param", unparsed.value

        client.call("session.stop")


# ---------------------------------------------------------------------------
# 2b. tab.write is nobody's — raw input is open
# ---------------------------------------------------------------------------

#: The one marker every write case sends, and the number of bytes its
#: sink copies before it stops. Six and then done, which is why a write
#: that wants proof needs a tab and a sink of its own.
TYPED = b"TYPED!"


def a_tab_with_a_sink(env, client: Roost, name: str) -> tuple[int, Path]:
    """A tab whose child copies [`TYPED`] into a file and moves on.

    The file is what makes "the write reached the **child**" observable
    from outside: `stty raw -echo` so the bytes arrive unlineated and
    nothing the line discipline does can be mistaken for the child
    reading them.
    """
    sink = env.launch_cwd / name
    tab = client.open_tab(
        first_project(client),
        cwd=str(env.launch_cwd),
        cols=COLS,
        rows=ROWS,
        argv=[
            "/bin/sh",
            "-c",
            f"stty raw -echo; dd bs=1 count={len(TYPED)} of='{sink}' 2>/dev/null; "
            "exec sleep 300",
        ],
    )
    return tab, sink


def wait_for_sink(sink: Path, what: str) -> None:
    sessionlib.wait_until(
        lambda: sink.is_file() and sink.stat().st_size == len(TYPED), 30.0, what
    )


def test_a_session_socket_write_reaches_the_child(env):
    """A session socket serves `tab.write`, over IPC and through the real
    `roostctl`, and the bytes land in the child.

    Two writers because they are two code paths: the harness client
    builds the frame itself, the CLI builds it from argv. Each gets its
    own tab and its own sink — one sink copies [`TYPED`] once and stops,
    so a shared one could only ever prove the first write landed.
    """
    started(env)

    with env.client() as client:
        tab, sink = a_tab_with_a_sink(env, client, "ipc")
        client.send(tab, TYPED)
        wait_for_sink(sink, "the child to receive the IPC write")
        assert sink.read_bytes() == TYPED

        cli_tab, cli_sink = a_tab_with_a_sink(env, client, "roostctl")
        # `--socket` is roostctl's, not `tab send`'s, and `--bytes` is
        # required — the escape-decoding form, which this marker passes
        # through unchanged.
        result = env.roostctl(
            "--socket", str(env.socket),
            "tab", "send",
            "--tab", str(cli_tab),
            "--bytes", TYPED.decode(),
        )
        assert result.returncode == 0, (result.stdout, result.stderr)
        wait_for_sink(cli_sink, "the child to receive the CLI's write")
        assert cli_sink.read_bytes() == TYPED

    env.stop_over_the_wire()


def test_a_fake_build_is_reported_and_enforced_at_attach(env):
    """`ROOST_SESSION_FAKE_BUILD` moves ONE string, and the whole
    negotiation follows it.

    A session and a client whose libghostty pins differ cannot exchange
    a snapshot, and `tab.attach` says so by name rather than letting the
    mismatch surface as a corrupt screen. That refusal drives the
    client's upgrade/restart flow, and reproducing it otherwise takes a
    second binary built against a second Ghostty pin — which no CI lane
    can produce.
    """
    started(env, ROOST_SESSION_FAKE_BUILD=FAKE_BUILD)

    with env.client() as client:
        identity = client.call("session.identify")
        assert identity["libghostty_build"] == FAKE_BUILD, identity

        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        def attach(build: str):
            return client.call(
                "tab.attach",
                {
                    "tab_id": str(tab),
                    "kinds": ["ghostty-snapshot"],
                    "cols": COLS,
                    "rows": ROWS,
                    "cell_w_px": 0,
                    "cell_h_px": 0,
                    "libghostty_build": build,
                    "focus": True,
                },
            )

        with pytest.raises(RoostError) as mismatch:
            attach("ghostty-1111111111111111+snapshot.v1")
        assert mismatch.value.code == "build-mismatch", mismatch.value

        # The same string identify reported is the one attach accepts:
        # the seam moves the negotiation, not just the report, so the
        # two can never disagree.
        assert attach(FAKE_BUILD)["attach_token"], "the fake build negotiates with itself"

        client.call("session.stop")


def test_the_fake_build_override_is_ignored_outside_test_mode(env):
    """The seam is strictly `ROOST_TEST_MODE=1`. A production daemon
    cannot be talked into lying about the pin it can actually decode —
    which would refuse every client that is in fact compatible."""
    launch = env.start_daemonized(
        ROOST_TEST_MODE="0", ROOST_SESSION_FAKE_BUILD=FAKE_BUILD
    )
    assert launch.verdict.kind == "ready", launch.verdict
    env.wait_answering()

    with env.client() as client:
        identity = client.call("session.identify")
        assert identity["libghostty_build"] != FAKE_BUILD, identity
        assert identity["libghostty_build"], "a session always states its build"
        client.call("session.stop")


# ---------------------------------------------------------------------------
# 4. Notifications fan out; each client answers for itself (#474)
# ---------------------------------------------------------------------------


def clear_notification(client: Roost, tab: int, generation: int | None = None) -> bool:
    """Acknowledge a tab's notification, returning whether this call is
    what took it down.

    With `generation` this is the automatic acknowledgement a client
    sends because it is already reading the tab, and the session refuses
    it if a newer raise has replaced the one it names. Without, it is the
    unconditional form a click or `roostctl` sends.
    """
    params: dict = {"tab_id": str(tab)}
    if generation is not None:
        params["generation"] = generation
    result = client.call("tab.clear_notification", params)
    assert set(result) == {"cleared"}, result
    return result["cleared"]


def next_fired(stream: EventStream, timeout: float = 30.0) -> dict:
    """The next `notification.fired` envelope's data."""
    _batches, envelope = stream.recv_until("notification.fired", timeout=timeout)
    return envelope["data"]


def test_the_op_a_session_muted_by_is_gone(env):
    """`session.set_focus` is not served at any version of this wire.

    It is the deletion that makes the rest of this section true: while
    the op existed, one client's statement muted a tab for every other
    client, so a phone left open on a tab silenced the laptop.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        for params in ({"focused_tab_id": str(tab)}, {"focused_tab_id": None}, {}):
            with pytest.raises(RoostError) as refused:
                client.call("session.set_focus", params)
            assert refused.value.code == "unknown-op", refused.value

        client.call("session.stop")


def test_a_session_fires_for_every_client_and_the_viewer_answers(env):
    """The whole of #474, end to end.

    Two clients are watching the same tab. The session fires to **both**
    — it cannot know that either of them is looking — and the one that
    is showing the tab acknowledges the raise it read. That
    acknowledgement clears the bit for everybody, which is the half a
    mute could never do: the other client's dot comes down too, rather
    than its notification never existing.
    """
    started(env)

    with env.client() as viewing, env.client() as watching:
        project = first_project(viewing)
        tab = quiet_tab(viewing, project, env.launch_cwd)

        with EventStream(env.socket) as at_viewer, EventStream(env.socket) as at_watcher:
            at_viewer.subscribe()
            fence = at_watcher.subscribe()

            viewing.notify(tab, "Claude Code", "Turn complete")

            fired = next_fired(at_viewer)
            assert fired["tab_id"] == str(tab), fired
            generation = fired["generation"]
            assert generation >= 1, fired

            # The client that is NOT looking gets the same event, which
            # is what it banners off. Nothing about the session's answer
            # differs between the two.
            batches, envelope = at_watcher.recv_until("notification.fired", timeout=30.0)
            at_watcher.expect_contiguous(batches, fence)
            fence = int(batches[-1]["revision"])
            assert envelope["data"] == fired, (envelope, fired)
            assert watching.has_notification(tab), "and the dot is up for both of them"

            # The viewing client answers, naming the raise it read.
            assert clear_notification(viewing, tab, generation) is True

            # Every client's dot comes down — the edge is committed, so
            # the watcher learns it on its own stream rather than by
            # polling.
            batches, envelope = at_watcher.recv_until("tab.notification", timeout=30.0)
            at_watcher.expect_contiguous(batches, fence)
            assert envelope["data"] == {"tab_id": str(tab), "has_pending": False}, envelope
            assert not watching.has_notification(tab)

        viewing.call("session.stop")


def test_a_stale_acknowledgement_leaves_the_newer_notification_standing(env):
    """The race the generation exists for.

    A fires, the viewing client acknowledges A, B fires, and A's
    acknowledgement lands afterwards — over a round trip that is an
    ordinary interleaving, not a pathological one. B must survive it:
    nobody has seen B.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket) as stream:
            stream.subscribe()

            client.notify(tab, "first", "the one that gets answered")
            stale = next_fired(stream)["generation"]
            assert clear_notification(client, tab, stale) is True

            client.notify(tab, "second", "the one nobody has seen")
            current = next_fired(stream)["generation"]
            assert current != stale, (current, stale)

            assert clear_notification(client, tab, stale) is False
            assert client.has_notification(tab), "the unseen notification is still up"

            # And the acknowledgement that names the current raise works.
            assert clear_notification(client, tab, current) is True
            assert not client.has_notification(tab)

        client.call("session.stop")


def test_a_clear_with_no_generation_answers_the_whole_tab(env):
    """The other caller: a click, or `roostctl tab clear-notification`.

    A person answering a tab is answering all of it, so the
    unconditional form stays exactly what it was — and still reports
    whether it is what took something down.
    """
    started(env)

    with env.client() as client:
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        client.notify(tab, "one")
        client.notify(tab, "two")
        assert clear_notification(client, tab) is True
        assert not client.has_notification(tab)
        assert clear_notification(client, tab) is False, "nothing left to take down"

        with pytest.raises(RoostError) as gone:
            clear_notification(client, tab + 9999)
        assert gone.value.code == "not-found", gone.value

        client.call("session.stop")


def test_a_resumed_subscription_replays_no_banner(env):
    """A banner is a moment; the dot is state.

    Replaying `notification.fired` into a gap raises a banner for
    something whose acknowledgement may be sitting in the same gap. The
    pending bit committed beside it is state and does replay, which is
    what a resuming client rebuilds the dot from — asserted positively,
    by reading through to that event and finding no banner among the
    batches it took.
    """
    started(env)

    with env.client() as client:
        session_id = client.call("session.identify")["session_id"]
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket) as before:
            fence = before.subscribe()
            client.notify(tab, "while you were away")
            # Read it live, so the raise is known to have committed
            # before the resume below cuts its replay.
            assert next_fired(before)["tab_id"] == str(tab)

        with EventStream(env.socket) as resumed:
            assert resumed.subscribe(from_revision=fence, session_id=session_id) == fence

            batches, envelope = resumed.recv_until("tab.notification", timeout=30.0)
            assert envelope["data"] == {"tab_id": str(tab), "has_pending": True}, envelope
            resumed.expect_contiguous(batches, fence)
            replayed = [
                event["event"] for batch in batches for event in batch.get("events", [])
            ]
            assert "notification.fired" not in replayed, replayed

        assert client.has_notification(tab), "and the dot is still up to be rebuilt"

        client.call("session.stop")
