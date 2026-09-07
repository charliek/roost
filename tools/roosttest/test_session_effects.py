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
* **`session.set_focus`** — the attached client's real focus, so the
  session suppresses notifications for the tab the user is actually
  looking at instead of for whichever tab its headless workspace
  defaulted to. Forgotten when the lease turns over, and when the
  connection that reported it (or the lease's last one) closes: a focus
  is a statement about a window that may no longer exist.
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


def connect_lease(client: Roost, takeover: bool = False, label: str | None = None) -> str:
    """`session.connect` — the lease every lease-gated op presents. A
    bearer credential: returned, never logged, never interpolated into an
    assertion message. `label` is what the claimant reports itself as,
    which is the only thing a deposed stream is told about it."""
    params: dict = {"takeover": takeover}
    if label is not None:
        params["client_label"] = label
    return client.call("session.connect", params)["lease"]


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
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket, lease=lease) as stream:
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
    """Every batch up to and including `revision`, contiguity checked.

    A `session.driver_changed` on the way is read and skipped — it is
    not a batch and carries no revision — so this doubles as "the stream
    survived the takeover".
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


def test_an_observer_stream_never_sees_an_effect_and_the_privilege_moves(env):
    """Plan 049 §3.7 + §3.8, over a real daemon and a real PTY drain.

    Effects are the *driving* client's side-channel (DL-18): a bell is a
    notification only the attached window can ring, and an OSC 52 write
    is somebody's clipboard. Reading a session is free now, so what used
    to be enforced by refusing the subscribe is enforced by the
    projection instead — and the revision still ships, as an **empty
    batch**, because a commit silently dropped is indistinguishable from
    loss to a client whose whole gap check is the revision sequence.

    Then the privilege moves. A takeover demotes the old driver's
    stream in place, and nothing that follows the
    `session.driver_changed` envelope on that stream carries an effect.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket, lease=lease) as driver:
            with EventStream(env.socket) as watcher:
                fence = driver.subscribe()
                watch_fence = watcher.subscribe()
                assert watch_fence >= fence

                client.tab_feed_pty_bytes(tab, osc52(b"a secret"))
                data, effect_revision = next_effect(driver, fence)
                assert data["effect"] == "clipboard-write", data

                # The same revision, on the leaseless stream: present,
                # in sequence, and carrying nothing.
                seen = batches_through(watcher, effect_revision)
                watcher.expect_contiguous(seen, watch_fence)
                filtered = [b for b in seen if int(b["revision"]) == effect_revision]
                assert filtered and not filtered[0]["events"], (
                    f"an observer must never see a tab.effect: {filtered}"
                )

                # The takeover. Both streams are told, neither is cut.
                with env.client() as interloper:
                    taker = connect_lease(interloper, takeover=True, label="a phone")
                    assert driver.recv_driver_changed() == "a phone"
                    assert watcher.recv_driver_changed() == "a phone"

                    with EventStream(env.socket, lease=taker) as promoted:
                        promoted_fence = promoted.subscribe()

                        interloper.tab_feed_pty_bytes(tab, b"\x07")
                        data, moved = next_effect(promoted, promoted_fence)
                        assert data["effect"] == "bell", data

                        # And the deposed stream, read to that same
                        # revision, carries no effect after the envelope.
                        after = batches_through(driver, moved)
                        assert all(
                            envelope["event"] != "tab.effect"
                            for batch in after
                            for envelope in batch["events"]
                        ), f"an effect arrived after session.driver_changed: {after}"

                    interloper.call("session.stop")


def test_a_notification_reaches_an_observer(env):
    """`notification.fired` is transient like an effect and crosses
    anyway (plan 049 §3.7).

    Routing notifications for AI coding agents is the point of watching
    a session at all — a phone that could see every title change but no
    notification would be watching the wrong half. Only `tab.effect`,
    which carries clipboard payloads and rings somebody else's bell,
    stays the driver's alone.
    """
    started(env)

    with env.client() as client:
        connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)
        # A second tab takes the selection: a focused, *active* tab
        # suppresses its own notification, so leaving `tab` selected
        # would make this pass or fail on the selection rather than on
        # routing.
        quiet_tab(client, project, env.launch_cwd)

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
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket, lease=lease) as stream:
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
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        # The headless default is white on black.
        expect_background(client, tab, "0000/0000/0000")

        result = client.call("session.set_theme", {"lease": lease, "osc_colors": theme()})
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
            {"lease": lease, "osc_colors": theme(background="#0a0b0c")},
        )
        expect_background(client, tab, "0a0a/0b0b/0c0c")

        client.call("session.stop")


def test_set_theme_is_lease_gated_and_validates_its_palette(env):
    """Interactive authority, same as every other lease-gated op: a
    client that does not drive the session does not get to recolor it.
    And a palette that is not 256 entries is refused whole rather than
    applied halfway."""
    started(env)

    with env.client() as stranger:
        with pytest.raises(RoostError) as refused:
            stranger.call("session.set_theme", {"lease": "0" * 32, "osc_colors": theme()})
        assert refused.value.code == "connect-required", refused.value

    with env.client() as client:
        lease = connect_lease(client)

        short = theme()
        short["palette"] = short["palette"][:8]
        with pytest.raises(RoostError) as bad:
            client.call("session.set_theme", {"lease": lease, "osc_colors": short})
        assert bad.value.code == "invalid-param", bad.value

        malformed = theme(background="rgb:1c/2b/3a")
        with pytest.raises(RoostError) as unparsed:
            client.call("session.set_theme", {"lease": lease, "osc_colors": malformed})
        assert unparsed.value.code == "invalid-param", unparsed.value

        client.call("session.stop")


# ---------------------------------------------------------------------------
# 2b. tab.write is the driver's
# ---------------------------------------------------------------------------


def test_a_session_write_needs_the_driver_lease(env):
    """Writing into a tab is an interactive act, so on a session socket
    it belongs to whoever holds the lease.

    The child copies its first bytes to a file: that file is the only
    honest proof the leased write reached the terminal rather than
    merely being admitted. `stty raw -echo` so the six bytes arrive
    unlineated and nothing is echoed back.
    """
    started(env)
    sink = env.launch_cwd / "written"

    with env.client() as client:
        project = first_project(client)
        tab = client.open_tab(
            project,
            cwd=str(env.launch_cwd),
            cols=COLS,
            rows=ROWS,
            argv=[
                "/bin/sh",
                "-c",
                f"stty raw -echo; dd bs=1 count=6 of='{sink}' 2>/dev/null; exec sleep 300",
            ],
        )

        # No lease at all, and an empty one: both are "this client never
        # connected", and neither may move a byte.
        for presented in (None, ""):
            with pytest.raises(RoostError) as refused:
                client.send(tab, b"NOPE!!", lease=presented)
            assert refused.value.code == "connect-required", refused.value

        lease = connect_lease(client)
        client.send(tab, b"LEASED", lease=lease)
        sessionlib.wait_until(
            lambda: sink.is_file() and sink.stat().st_size == 6,
            30.0,
            "the child to receive the leased write",
        )
        assert sink.read_bytes() == b"LEASED"

    # And the displaced lease buys nothing once someone else drives. The
    # check runs on a fresh connection because the takeover closes every
    # connection registered under the lease it displaces — including the
    # one that just wrote.
    with env.client() as taker:
        connect_lease(taker, takeover=True, label="taker")
        with env.client() as stranger:
            with pytest.raises(RoostError) as stale:
                stranger.send(tab, b"STALE!", lease=lease)
            assert stale.value.code == "taken-over", stale.value

    env.stop_over_the_wire()


# ---------------------------------------------------------------------------
# 3. The build-mismatch seam
# ---------------------------------------------------------------------------


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

        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        def attach(build: str):
            return client.call(
                "tab.attach",
                {
                    "lease": lease,
                    "tab_id": str(tab),
                    "kinds": ["ghostty-snapshot"],
                    "cols": COLS,
                    "rows": ROWS,
                    "cell_w_px": 0,
                    "cell_h_px": 0,
                    "libghostty_build": build,
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
# 4. session.set_focus (HS-3)
# ---------------------------------------------------------------------------


def set_focus(client: Roost, lease: str, tab: int | None) -> None:
    """State what the attached client is looking at. `None` is an
    explicit JSON null — the field is required, and null is a statement
    ("nothing here") rather than an omission."""
    result = client.call(
        "session.set_focus",
        {"lease": lease, "focused_tab_id": None if tab is None else str(tab)},
    )
    assert result == {}, result


def next_fired(stream: EventStream, timeout: float = 30.0) -> dict:
    """The next `notification.fired` envelope's data."""
    _batches, envelope = stream.recv_until("notification.fired", timeout=timeout)
    return envelope["data"]


def wait_until_fires(stream: EventStream, client: Roost, tab: int) -> None:
    """Raise on `tab` until one gets through.

    The condition wait for a server-side edge nothing on the wire
    announces: a closed socket is noticed by that connection's own task,
    so the focus reset it triggers has no happens-before against the next
    request. Re-raising is free (a suppressed raise emits nothing at all,
    not even a pending bit) and the first one through ends the wait.
    """

    def fired() -> dict | None:
        client.notify(tab, "waiting for the reset")
        try:
            return next_fired(stream, timeout=1.0)
        except TimeoutError:
            return None

    data = sessionlib.wait_until(
        fired, 30.0, "the session to forget the departed client's focus", interval=0.0
    )
    assert data["tab_id"] == str(tab), data


def assert_muted(stream: EventStream, client: Roost, muted: int, heard: int) -> None:
    """`muted` is suppressed and `heard` is not — asserted positively.

    A suppressed raise emits **nothing at all** (no pending bit, no
    events), so the only sound way to see it is to raise on both tabs and
    watch which one arrives: reading until the sentinel is a positive
    assertion about the dropped one rather than a timeout.
    """
    client.notify(muted, "muted")
    client.notify(heard, "heard")
    data = next_fired(stream)
    assert data["tab_id"] == str(heard), (
        f"tab {muted} was expected to be suppressed, but {data} arrived first"
    )


def test_set_focus_moves_which_tab_a_session_mutes(env):
    """The gap HS-2 recorded, closed.

    A session's workspace has no window: it defaults to focused, on
    whichever tab its layout selected, so its suppression predicate reads
    as permanently satisfied for that one tab and its agent can never
    raise anything. The attached client is the only thing that knows
    better, and this is how it says so.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        watched = quiet_tab(client, project, env.launch_cwd)
        other = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket, lease=lease) as stream:
            stream.subscribe()

            set_focus(client, lease, watched)
            assert_muted(stream, client, muted=watched, heard=other)

            # Null is the other half of the statement: the window lost
            # focus, or the selection moved off this session, and the tab
            # that was muted goes back to raising.
            set_focus(client, lease, None)
            client.notify(watched, "unmuted")
            assert next_fired(stream)["tab_id"] == str(watched)

            # The focus follows the selection, tab for tab.
            set_focus(client, lease, other)
            assert_muted(stream, client, muted=other, heard=watched)

        client.call("session.stop")


def test_a_reported_focus_does_not_outlive_the_client_that_reported_it(env):
    """The load-bearing half: focus is forgotten when the lease's last
    connection goes.

    A lease deliberately outlives its connections (reconnecting is a
    takeover), but a *focus* must not — it was a statement about a window
    that no longer exists. Left standing, one `set_focus` would mute a
    tab for every client that comes after, which is exactly the bug this
    op exists to fix, rebuilt out of stale state.
    """
    started(env)

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        watched = quiet_tab(client, project, env.launch_cwd)
        other = quiet_tab(client, project, env.launch_cwd)

        with EventStream(env.socket, lease=lease) as stream:
            stream.subscribe()
            set_focus(client, lease, watched)
            assert_muted(stream, client, muted=watched, heard=other)

    # Both connections registered under the lease are closed now: the
    # control one (it presented the lease at `set_focus`) and the
    # subscriber. Nobody is looking at this session any more.
    with env.client() as client:
        with EventStream(env.socket, lease=lease) as stream:
            stream.subscribe()

            # The reset lands when the server notices the closed sockets,
            # which nothing on this connection can be ordered against —
            # hence the condition wait rather than one raise and a hope.
            wait_until_fires(stream, client, watched)

            # The lease itself survived, and re-asserting on it is what a
            # reconnecting UI does the moment it reaches Connected.
            set_focus(client, lease, watched)
            assert_muted(stream, client, muted=watched, heard=other)

        client.call("session.stop")


def test_set_focus_is_lease_gated_and_needs_a_tab_that_exists(env):
    """Interactive authority, and a required-but-nullable field.

    `focused_tab_id` may be null but may not be missing: an omitted field
    is a client that never said, and answering it with a guess is how the
    mute comes back.
    """
    started(env)

    with env.client() as stranger:
        with pytest.raises(RoostError) as refused:
            stranger.call(
                "session.set_focus", {"lease": "0" * 32, "focused_tab_id": None}
            )
        assert refused.value.code == "connect-required", refused.value

    with env.client() as client:
        lease = connect_lease(client)
        project = first_project(client)
        tab = quiet_tab(client, project, env.launch_cwd)

        with pytest.raises(RoostError) as missing:
            client.call("session.set_focus", {"lease": lease})
        assert missing.value.code == "missing-param", missing.value

        with pytest.raises(RoostError) as gone:
            client.call(
                "session.set_focus", {"lease": lease, "focused_tab_id": str(tab + 9999)}
            )
        assert gone.value.code == "not-found", gone.value

        # And the refusal applied nothing: the tab that was already there
        # still raises, which it would not if the flag had moved ahead of
        # the validation.
        with EventStream(env.socket, lease=lease) as stream:
            stream.subscribe()
            client.notify(tab, "after a refused focus")
            assert next_fired(stream)["tab_id"] == str(tab)

        client.call("session.stop")
