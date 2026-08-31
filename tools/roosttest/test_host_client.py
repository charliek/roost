"""HS-2 end to end: a real `roost-session` beside a real UI (plan 037 §7).

Every other lane in this directory drives one process. This one drives
two and the wire between them, because that wire *is* the feature: a
host session owns the shells, the UI owns the window, and HS-2 is the
claim that a client can render, drive and survive the loss of a terminal
it does not own.

# The shape of a case here

Three parties, none of them simulated:

* the **session** — a `roost-session` daemon on its own throwaway
  profile (`session.py`), spawned per test. `ROOST_TEST_MODE=1`, so
  `tab.feed_pty_bytes` can put bytes on a tab's drain that are
  indistinguishable from a busy child's. Its socket is the host's
  `target`.
* the **client** — the ordinary harness UI (`--roost-target iced`),
  driven over its own socket. `host.add` + `host.connect` are the only
  setup it needs; there are no keystrokes in this file.
* occasionally a **second client** — a scripted wire client (a
  `session.connect` on a plain socket plus `eventstream.py`), which is
  what a takeover and the lease-holder-only effects rule need. The wire
  cannot tell a second Roost window from a Python socket, which is
  exactly why the scripted one is the honest stand-in, and why plan 037
  §9 makes a real second window the stretch variant rather than the
  default.

# How a host tab is observed

Two dumps of the same terminal. The **session** socket answers
`tab.dump` / `tab.dump_resolved` for a bare id and walks the server's
terminal; the **UI** socket answers the same ops for the `h<host>.<id>`
wire spelling and walks the *client's* hydrated terminal (§3.4). AC6 is
literally the assertion that those two agree.

`h<host>.<id>`'s host component is an **incarnation**, minted afresh on
every connect attempt (`keys.rs`), so it is discovered rather than
assumed — [`host_key`] focuses upward until one answers, which is both
the discovery and the attach.

# What is deliberately not here

The upgrade dialog's "Restart session" button and the takeover banner
are view state with no op behind them; their logic is unit-tested in
`roost-iced`. What this lane proves is the *state* the button acts on
(a host really does reach `needs-restart` against a mismatched build)
and the *composition* the button runs (stop → gone → relaunch really
does bring the layout back). See [`test_a_build_mismatch_reaches_needs_restart_and_a_restart_restores_the_layout`].

Condition waits only.
"""

from __future__ import annotations

import base64
import contextlib
import json
import time
import uuid
from dataclasses import dataclass

import pytest
import session as sessionlib
import ui
from client import Roost, RoostError, scaled_timeout
from eventstream import EventStream

pytestmark = pytest.mark.host_client


# The geometry the session opens its tabs at. The client resizes them to
# its own window at attach, which is the point of AC6's "geometry
# aligned by the attach params" — so this is a starting value, not an
# expectation.
COLS, ROWS = 80, 24

# How many consecutive incarnations [`host_key`] scans before concluding
# the host is not connected. Generous because the minter is the *app's*,
# not this run's: a UI that a developer has been driving all afternoon is
# already hundreds of connects in, and a ceiling tuned to one pytest run
# would turn "connected, incarnation 300" into a timeout. The scan is a
# one-off — [`_incarnation_floor`] makes every probe after the first
# start where the last one landed.
INCARNATION_SCAN_SPAN = 4096

# Plan 037 §7.10: a focused-tab attach on localhost reaches a rendered
# frame in under half a second, end to end through the UI. Scaled like
# every other budget here, so a loaded CI runner widens it.
ATTACH_BUDGET_S = 0.5

# What `ROOST_SESSION_FAKE_BUILD` puts on the wire — shaped like a real
# build string, impossible to mistake for one. Same value the session
# lane uses; restated because this file is the client side of that seam.
FAKE_BUILD = "ghostty-0000000000000000+fake.plan037"

# The palette subtitle the Connect verb carries per host state
# (`host_verbs::connect_subtitle`). Restated rather than imported: the
# palette row is the only place a client's connection state is readable
# over IPC, so a wording change that silently broke that read is exactly
# what these tests must catch.
SUBTITLE_NEEDS_RESTART = "build mismatch — offers a restart"
SUBTITLE_TAKEN_OVER = "take the session back"


# ---------------------------------------------------------------------------
# The two processes
# ---------------------------------------------------------------------------


@pytest.fixture
def session_env():
    """One throwaway `roost-session` profile, torn down whatever the test
    did to it."""
    made = sessionlib.make_env()
    try:
        yield made
    finally:
        made.teardown()


def start_session(env, **overrides) -> None:
    """Daemonize a session in test mode and prove it is answering."""
    launch = env.start_daemonized(ROOST_TEST_MODE="1", **overrides)
    assert launch.returncode == 0, f"start failed: {launch.stdout!r} / {launch.stderr!r}"
    assert launch.verdict.kind == "ready", launch.verdict
    env.wait_answering()


def require_test_mode(roost: Roost) -> None:
    """Fail early, and by name, on a UI launched without
    `ROOST_TEST_MODE=1`.

    This lane reads what the UI queued toward a host
    (`tab.capture_pty_input`), which is one of the test-gated ops. The
    probe has to use a **live** tab: the UI answers the same
    "ROOST_TEST_MODE=1 is required or tab is missing" for a disabled op
    and for an unknown id, so probing a made-up id would report every
    healthy UI as misconfigured. With no local tab to probe there is
    nothing to conclude — leave it to the one case that needs the op.
    """
    live = [int(row["id"]) for row in roost.tabs()]
    if not live:
        return
    try:
        roost.tab_capture_pty_input(live[0], drain=False)
    except RoostError as error:
        if error.code == "not-enabled":
            pytest.skip(
                "the UI under test was launched without ROOST_TEST_MODE=1; "
                "run this lane via `make e2e-host-client`"
            )
        raise


# ---------------------------------------------------------------------------
# The host under test
# ---------------------------------------------------------------------------


@dataclass
class HostUnderTest:
    """One saved host, its session, and the client driving it.

    A test asks this for states and keys rather than for connections:
    `connect()` starts an attempt (the op answers `connecting` by
    contract — §3.5), and every wait below settles on what the *palette*
    offers, which is the client's own read of its connection state.
    """

    roost: Roost
    env: sessionlib.SessionEnv
    saved_id: str
    label: str

    # -- registry + connection ------------------------------------------
    def connect(self) -> dict:
        return self.roost.call("host.connect", {"id": self.saved_id})

    def disconnect(self) -> dict:
        return self.roost.call("host.disconnect", {"id": self.saved_id})

    def remove(self) -> None:
        self.roost.call("host.remove", {"id": self.saved_id})

    def connect_and_wait(self, timeout: float = 30.0) -> None:
        result = self.connect()
        # The op reports what was asked for, never the far end's verdict:
        # waiting for a dial, an identify and a lease before replying
        # would block the caller on a round trip it can watch instead.
        assert result["state"] in ("connecting", "connected"), result
        self.wait_connected(timeout)

    # -- state, as the palette shows it ---------------------------------
    def wait_connected(self, timeout: float = 30.0) -> None:
        wait_until(
            lambda: f"host:disconnect:{self.saved_id}" in host_row_ids(self.roost),
            timeout,
            f"{self.label} to reach connected",
        )

    def wait_not_connected(self, timeout: float = 30.0) -> None:
        wait_until(
            lambda: f"host:connect:{self.saved_id}" in host_row_ids(self.roost),
            timeout,
            f"{self.label} to leave connected",
        )

    def wait_connect_subtitle(self, subtitle: str, timeout: float = 60.0) -> None:
        """Wait for the Connect verb to describe a specific state.

        `connect_subtitle` is per-state (`host_verbs.rs`), so this is the
        one read that distinguishes `needs-restart` from `taken-over`
        from a plain drop without a state-reporting op."""

        def settled() -> bool:
            row = host_row(self.roost, f"host:connect:{self.saved_id}")
            return row is not None and row.get("subtitle") == subtitle

        wait_until(settled, timeout, f"{self.label} to offer Connect: {subtitle!r}")

    # -- the session behind it ------------------------------------------
    def client(self, timeout: float = 30.0) -> Roost:
        return self.env.client(timeout=timeout)

    @staticmethod
    def lease(client: Roost, takeover: bool = False) -> str:
        """A `session.connect` lease. A bearer credential: returned,
        never logged, never interpolated into an assertion message."""
        return client.call("session.connect", {"takeover": takeover})["lease"]


@contextlib.contextmanager
def saved_host(roost: Roost, env: sessionlib.SessionEnv):
    """Register `env`'s socket as a host, and forget it afterwards.

    Registry-first on purpose: `host.add` is documented as registry-only
    (`roostctl host add`'s semantics), so a test that wants a connection
    asks for one and a test about the zero-connection state does not have
    to unpick one.

    The removal happens **before** the session dies, so the client is
    never left dialing a socket the fixture is about to delete.
    """
    label = f"hs-{uuid.uuid4().hex[:8]}"
    added = roost.call("host.add", {"label": label, "target": str(env.socket)})["host"]
    under_test = HostUnderTest(roost=roost, env=env, saved_id=added["id"], label=label)
    try:
        # [`host_key`] finds a tab by the number it has, and two connected
        # hosts can both have a tab `4` — so a second live connection
        # makes the probe ambiguous and would land this test on someone
        # else's terminal. Other *saved* hosts are harmless (a developer's
        # registry usually has some); only another connected one is not.
        # Every CI lane runs `--roost-fresh`, where this cannot happen.
        others = {
            row.removeprefix("host:disconnect:")
            for row in host_row_ids(roost)
            if row.startswith("host:disconnect:")
        } - {under_test.saved_id}
        if others:
            pytest.skip(
                f"another host is already connected ({sorted(others)}); the "
                "incarnation probe cannot tell two connected hosts' tabs apart"
            )
        yield under_test
    finally:
        roost.palette_dismiss()
        try:
            under_test.remove()
        except RoostError:
            pass  # a case may have removed it itself


@pytest.fixture
def host(roost, session_env):
    """A running session, saved as a host, not yet connected."""
    require_test_mode(roost)
    start_session(session_env)
    with saved_host(roost, session_env) as under_test:
        yield under_test


# ---------------------------------------------------------------------------
# Reading the client's state over IPC
# ---------------------------------------------------------------------------


def wait_until(pred, timeout: float, what: str, interval: float = 0.05):
    eff = scaled_timeout(timeout)
    deadline = time.monotonic() + eff
    while True:
        value = pred()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(f"timed out after {eff:.1f}s waiting for {what}")
        time.sleep(interval)


def host_rows(roost: Roost) -> list[dict]:
    """The command palette's host verb rows.

    The palette is where every host verb lives (§3.1), and — because
    verbs appear only when they apply — it is also the only op-reachable
    read of a host's connection state: `host:disconnect:<id>` is offered
    exactly while connected, `host:connect:<id>` exactly while not.
    Opened and dismissed around each read so a failed case cannot leave
    the overlay up for the next one.
    """
    state = roost.palette_open("commands")
    try:
        return [item for item in state.get("items", []) if item["id"].startswith("host:")]
    finally:
        roost.palette_dismiss()


def host_row_ids(roost: Roost) -> set[str]:
    return {item["id"] for item in host_rows(roost)}


def host_row(roost: Roost, row_id: str) -> dict | None:
    return next((item for item in host_rows(roost) if item["id"] == row_id), None)


def inbox_ids(roost: Roost) -> set[str]:
    """The notification inbox's row ids — `notif:<TabKey>`, so a host
    row's id carries its incarnation and a local row's does not.

    The inbox is a frame the command palette drills into rather than a
    palette kind of its own, and a frame fixes its rows when it is
    pushed — so re-pushing is how the *current* inbox is read
    (`test_notifications.py` reads it the same way).
    """
    roost.palette_open("commands")
    try:
        state = roost.palette_activate("view_notifications")
        return {item["id"] for item in state.get("items", [])}
    finally:
        roost.palette_dismiss()


# ---------------------------------------------------------------------------
# Host tabs, by their wire spelling
# ---------------------------------------------------------------------------


# Where the next scan starts. Incarnations only ever go up (`keys.rs`
# mints, never reuses), so the last one that answered is a valid floor
# for the next probe — that is what keeps a 4096-wide scan a once-per-run
# cost instead of a per-call one.
_incarnation_floor = 1


def host_key(roost: Roost, tab_id: int, timeout: float = 30.0) -> str:
    """Focus a host tab and return the `h<host>.<id>` spelling that
    reached it.

    The incarnation is minted per connect attempt and is not reported by
    any op, so it is *discovered*: probe upward until one answers. A
    stale incarnation names nothing in the connection set (that is the
    whole point of minting a fresh one), so a wrong guess is a clean
    `not-found` rather than a wrong tab — which is what makes probing
    safe rather than merely convenient.

    Focusing is the discovery because focusing is also the attach
    (§3.4's attach-on-focus): the same call a sidebar click makes.
    """
    global _incarnation_floor

    def probe() -> str | None:
        global _incarnation_floor
        for incarnation in range(
            _incarnation_floor, _incarnation_floor + INCARNATION_SCAN_SPAN
        ):
            key = f"h{incarnation}.{tab_id}"
            try:
                roost.call("tab.focus", {"tab_id": key})
            except RoostError:
                continue
            _incarnation_floor = incarnation
            return key
        return None

    return wait_until(probe, timeout, f"a connected host to list tab {tab_id}")


def sibling_key(key: str, tab_id: int) -> str:
    """Another tab of the same connection, without a second probe.

    An incarnation belongs to the *connection*, not to a tab, so once one
    of a host's tabs has been reached the rest are addressable by
    substitution. Needed for tabs a test must NOT focus — focusing is
    what attaches, and attaching is what a case about a background tab is
    trying to avoid.
    """
    return f"{key.split('.', 1)[0]}.{tab_id}"


def focus(roost: Roost, key: str) -> None:
    roost.call("tab.focus", {"tab_id": key})


def dump(roost: Roost, key: str) -> dict:
    return roost.call("tab.dump", {"tab_id": key})


def dump_text(roost: Roost, key: str) -> str:
    return "\n".join(dump(roost, key)["rows_text"])


def try_dump_text(roost: Roost, key: str) -> str | None:
    """The client's view of a host tab, or None while it has no terminal
    for it. `None` is a real answer — an unattached tab — and is what
    lets the never-blank assertions distinguish "no frame yet" from "a
    frame, and it is empty"."""
    try:
        return dump_text(roost, key)
    except RoostError:
        return None


def wait_dump_contains(roost: Roost, key: str, needle: str, timeout: float = 30.0) -> None:
    wait_until(
        lambda: needle in (try_dump_text(roost, key) or ""),
        timeout,
        f"{needle!r} in the client's terminal for {key}",
    )


def wait_session_dump_contains(
    client: Roost, tab: int, needle: str, timeout: float = 30.0
) -> None:
    wait_until(
        lambda: needle in "\n".join(client.dump(tab)["rows_text"]),
        timeout,
        f"{needle!r} in the session's terminal for tab {tab}",
    )


def quiet_tab(client: Roost, project: int, cwd) -> int:
    """A tab parked on a child that never writes, so every byte in its
    stream is one the test put there."""
    return client.open_tab(
        project,
        cwd=str(cwd),
        cols=COLS,
        rows=ROWS,
        argv=["/bin/sh", "-c", "exec sleep 300"],
    )


def first_project(client: Roost) -> int:
    return int(client.list()[0]["id"])


def marker(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def osc52(payload: bytes, selector: str = "c") -> bytes:
    encoded = base64.b64encode(payload).decode("ascii")
    return f"\x1b]52;{selector};{encoded}\x07".encode()


# ---------------------------------------------------------------------------
# 1. AC1 — the zero-host baseline
# ---------------------------------------------------------------------------


def test_a_client_with_no_saved_hosts_offers_no_host_rows(roost):
    """Roadmap D8's zero-change rule, stated as an assertion.

    The whole Hosts surface is additive: with an empty registry the
    palette gains exactly one row that was not there before HS-2 (`Add
    Host…`, plus the seeded localhost Connect where a client could reach
    a local session at all), and no per-host row exists to change the
    sidebar. Cheap, and it is the fence that catches a verb builder that
    started emitting rows for a registry that has none.
    """
    if roost.call("host.list", {})["hosts"]:
        # A developer's own UI may legitimately have hosts saved. There
        # is no zero-host baseline to read there, and forcing one would
        # mean deleting their registry — so say why rather than fail.
        # Every CI lane runs `--roost-fresh`, where this always runs.
        pytest.skip("the client under test already has saved hosts (needs --roost-fresh)")
    rows = host_row_ids(roost)
    assert "host:add" in rows, rows
    per_host = {row for row in rows if row.startswith(("host:connect:", "host:disconnect:"))}
    assert per_host == set(), per_host
    assert "host:new_project_on" not in rows, (
        "the picker row is offered only once there is somewhere else to create"
    )


# ---------------------------------------------------------------------------
# 2. AC2 — the roadmap's acceptance: the session outlives the connection
# ---------------------------------------------------------------------------


def test_a_marker_written_before_a_disconnect_survives_the_reconnect(host, roost):
    """The feature's whole promise, in one case.

    Disconnect is not stop (D8): the session keeps its shells and its
    scrollback, and a client that comes back finds the terminal where it
    left it. The proof is a content probe rather than a liveness check —
    a session that had silently restarted would still answer `tab.list`,
    but the marker would be gone.

    The reconnect deliberately goes through a *fresh* incarnation: the
    key that read the terminal before the disconnect is dead by
    contract, so [`host_key`] discovers the new one, which is also how
    this case proves the staleness contract does not strand the tab.
    """
    host.connect_and_wait()
    with host.client() as session:
        tab = quiet_tab(session, first_project(session), host.env.launch_cwd)
        key = host_key(roost, tab)

        line = marker("SURVIVES")
        session.tab_feed_pty_bytes(tab, f"{line}\r\n".encode())
        wait_dump_contains(roost, key, line)

        host.disconnect()
        host.wait_not_connected()
        # The session is untouched by a client leaving: its own terminal
        # still holds the line, and its shells are still running.
        assert line in "\n".join(session.dump(tab)["rows_text"])

        host.connect_and_wait()
        again = host_key(roost, tab)
        assert again != key, (
            "a reconnect must mint a fresh incarnation, or a delayed message from "
            "the dead one could land on the new connection's tab"
        )
        wait_dump_contains(roost, again, line)


# ---------------------------------------------------------------------------
# 3. AC6 — attach fidelity: two terminals, one truth
# ---------------------------------------------------------------------------


def resolved_grid(dumped: dict) -> dict:
    return {
        (cell["row"], cell["col"]): (
            cell["text"],
            cell["fg"],
            cell["bg"],
            cell["has_explicit_bg"],
            cell["bold"],
            cell["italic"],
            cell["inverse"],
        )
        for cell in dumped["cells"]
    }


def test_the_clients_terminal_matches_the_sessions_for_a_scripted_workload(host, roost):
    """AC6, as an equality rather than a sample.

    Every cell, through the production color resolver on both sides. The
    resolved form is the interesting one: it is where a client that
    decoded a snapshot into the wrong palette, or attached at the wrong
    geometry, stops agreeing with the terminal it is supposedly showing.
    The colors line up because the client seeds the session with its own
    theme before it ever attaches (`session.set_theme`, §3.6).

    The workload is fed *after* the attach so both terminals are already
    at the geometry the attach negotiated — comparing across a resize
    would compare two different screens and call the difference a bug.
    """
    host.connect_and_wait()
    with host.client() as session:
        tab = quiet_tab(session, first_project(session), host.env.launch_cwd)
        key = host_key(roost, tab)
        # Wait for hydration to finish before writing: an empty client
        # terminal and an empty server one agree trivially.
        settle = marker("ATTACHED")
        session.tab_feed_pty_bytes(tab, f"{settle}\r\n".encode())
        wait_dump_contains(roost, key, settle)

        payload = marker("PARITY")
        session.tab_feed_pty_bytes(
            tab,
            b"".join(
                [
                    f"{payload}\r\n".encode(),
                    b"\x1b[31mRED\x1b[0m \x1b[1mBOLD\x1b[0m \x1b[3mITAL\x1b[0m\r\n",
                    b"\x1b[7mINVERSE\x1b[0m \x1b[44mONBLUE\x1b[0m\r\n",
                ]
            ),
        )
        wait_dump_contains(roost, key, "ONBLUE")
        wait_session_dump_contains(session, tab, "ONBLUE")

        served = session.tab_dump_resolved(tab)
        client_side = roost.call("tab.dump_resolved", {"tab_id": key})
        assert (client_side["cols"], client_side["rows"]) == (
            served["cols"],
            served["rows"],
        ), (client_side["cols"], client_side["rows"], served["cols"], served["rows"])
        assert resolved_grid(client_side) == resolved_grid(served)


def test_a_refocus_resumes_without_blanking_or_duplicating(host, roost):
    """The two ways a re-attach can be wrong, watched for while it runs.

    A retry loop that dropped the rendered terminal before the new
    stream was ready would blank the tab (architecture §4.3's
    keep-old-until-READY rule); a resume that replayed bytes the client
    already had would double them. Both are invisible to an
    after-the-fact assertion, so this samples the client's terminal
    continuously across the window instead of once at the end.
    """
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        other = quiet_tab(session, project, host.env.launch_cwd)
        key = host_key(roost, tab)

        line = marker("ONCE")
        session.tab_feed_pty_bytes(tab, f"{line}\r\n".encode())
        wait_dump_contains(roost, key, line)
        other_key = host_key(roost, other)

        # Away and back — the same pair of clicks a user makes, and the
        # detach/re-attach the focused-tab attach policy runs under it.
        focus(roost, other_key)
        focus(roost, key)

        deadline = time.monotonic() + scaled_timeout(30.0)
        frames = 0
        while True:
            text = try_dump_text(roost, key)
            if text is not None:
                frames += 1
                # A frame the client is *showing* must never be an empty
                # screen. "No terminal yet" is None above, and is a
                # different thing entirely.
                assert text.strip() != "", (
                    "the client published a blank frame during a re-attach"
                )
                if line in text:
                    break
            assert time.monotonic() < deadline, "the re-attach never produced the old frame"
        assert frames > 0

        text = dump_text(roost, key)
        assert text.count(line) == 1, (
            f"the resume replayed {line!r} {text.count(line)} times:\n{text}"
        )


# ---------------------------------------------------------------------------
# 4. AC4 — Disconnect is not Stop
# ---------------------------------------------------------------------------


def test_disconnect_leaves_the_shells_running_and_stop_reaps_them(host, roost):
    """The distinction D8 exists to protect, from both sides.

    Disconnecting is a client-side act: the session never hears about it
    beyond a dropped connection, so `roostctl session status` still
    counts the same tabs. Stopping is the opposite — the shells end, and
    what survives is the *layout*, which the session rebuilds from its
    own `state.json` on the next start.
    """
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        host_key(roost, tab)
        before = [(row["cwd"], row["title"]) for row in session.tabs()]

    host.disconnect()
    host.wait_not_connected()

    status = host.env.roostctl("session", "status")
    assert status.returncode == 0, status.stdout + status.stderr
    assert f"tabs={len(before)}" in status.stdout, status.stdout
    with host.client() as session:
        assert tab in [int(row["id"]) for row in session.tabs()], (
            "disconnecting a host must not close a single one of its shells"
        )

    # Stop: the shells go, the file stays.
    host.env.stop_over_the_wire()
    host.env.wait_socket_gone()
    saved = host.env.state()
    assert [
        (t["cwd"], t["title"]) for p in saved["projects"] for t in p["tabs"]
    ] == before, saved

    start_session(host.env)
    with host.client() as session:
        restored = session.tabs()
        assert [(row["cwd"], row["title"]) for row in restored] == before, restored
        assert tab not in [int(row["id"]) for row in restored], (
            "a restart reopens the layout as FRESH shells, never the old processes"
        )

    # And the client can pick the restarted session back up.
    host.connect_and_wait()
    with host.client() as session:
        host_key(roost, int(session.tabs()[0]["id"]))


# ---------------------------------------------------------------------------
# 5. AC5 — the upgrade flow
# ---------------------------------------------------------------------------


def test_a_build_mismatch_reaches_needs_restart_and_a_restart_restores_the_layout(
    roost, session_env
):
    """The feature's most common failure mode, end to end.

    A session started by another Roost cannot exchange a snapshot with
    this one, and `ROOST_SESSION_FAKE_BUILD` reproduces that without a
    second binary built against a second Ghostty pin. The client is
    supposed to *name* it rather than show a corrupt screen: the host
    settles in `needs-restart`, which the palette reports as the Connect
    verb's subtitle.

    The second half runs the restart the dialog's button runs — stop,
    wait for the socket to really go, start again — and asserts the
    thing that makes the warning honest: the layout comes back, as fresh
    shells, and the client attaches to it.
    """
    require_test_mode(roost)
    start_session(session_env, ROOST_SESSION_FAKE_BUILD=FAKE_BUILD)
    identity = session_env.identify()
    assert identity["libghostty_build"] == FAKE_BUILD, identity

    with session_env.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, session_env.launch_cwd)
        before = [(row["cwd"], row["title"]) for row in session.tabs()]

    with saved_host(roost, session_env) as under_test:
        under_test.connect()
        under_test.wait_connect_subtitle(SUBTITLE_NEEDS_RESTART)

        # The restart, composed exactly as `host_conn::restart` composes
        # it. There is no `session.restart` op and deliberately never
        # will be — the order is the contract.
        session_env.stop_over_the_wire()
        session_env.wait_socket_gone()
        start_session(session_env)

        under_test.connect_and_wait()
        with session_env.client() as session:
            restored = session.tabs()
            assert [(row["cwd"], row["title"]) for row in restored] == before, restored
            assert tab not in [int(row["id"]) for row in restored], (
                "the dialog promises fresh shells; a restored process would make it a lie"
            )
            host_key(roost, int(restored[0]["id"]))


# ---------------------------------------------------------------------------
# 6. AC3 — takeover, and what the displaced window keeps
# ---------------------------------------------------------------------------


def test_a_takeover_freezes_the_frame_without_losing_it_and_connect_takes_it_back(
    host, roost
):
    """The displaced window is dimmed, not emptied (§3.1).

    A takeover revokes the lease, so the data plane goes and no input
    can reach the session — but the last frame is still the truth about
    what that terminal said, and throwing it away would turn a
    recoverable interruption into a lost screen. So: the client's
    terminal still answers, it still carries the marker, nothing was
    queued toward the session while frozen, and the server's own
    terminal is byte-identical to what it was before the takeover.

    "Reconnect here" is `host.connect`, which is unconditional takeover
    by contract — the displaced client takes the session straight back,
    and the interloper is told why its stream ended.
    """
    host.connect_and_wait()
    with host.client() as session:
        tab = quiet_tab(session, first_project(session), host.env.launch_cwd)
        key = host_key(roost, tab)
        line = marker("FROZEN")
        session.tab_feed_pty_bytes(tab, f"{line}\r\n".encode())
        wait_dump_contains(roost, key, line)
        roost.tab_capture_pty_input(key)  # drain what the attach itself sent
        before = session.tab_dump_resolved(tab)

        # The second party. A scripted wire client, because the wire
        # cannot tell one from a second Roost window — which is exactly
        # the property under test.
        with host.client() as interloper:
            lease = host.lease(interloper, takeover=True)
            with EventStream(host.env.socket, lease=lease) as stream:
                stream.subscribe()
                host.wait_connect_subtitle(SUBTITLE_TAKEN_OVER)

                frozen = try_dump_text(roost, key)
                assert frozen is not None and line in frozen, (
                    "a taken-over host must keep its last frame, not blank the tab"
                )
                assert roost.tab_capture_pty_input(key) == b"", (
                    "a frozen frame must swallow no input"
                )
                assert resolved_grid(session.tab_dump_resolved(tab)) == resolved_grid(
                    before
                ), "the session's own terminal changed while the client was frozen"

                # Reconnect here.
                host.connect_and_wait()
                assert stream.recv_stopping() == "taken-over", stream.stopping_reason

        back = host_key(roost, tab)
        wait_dump_contains(roost, back, line)


# ---------------------------------------------------------------------------
# 7. AC8 — effects reach the lease holder and nobody else
# ---------------------------------------------------------------------------


def test_a_bell_reaches_the_attached_client_and_a_stranger_gets_no_stream(host, roost):
    """Effects are addressed to whoever is driving the session.

    A session has no view of its own, so a bell and an OSC 52 write only
    mean anything to an attached client — and only to the one holding
    the lease, or a second window would silently steal the first's
    clipboard. The stranger's half is proven at the gate rather than by
    watching it receive nothing: without a live lease the events stream
    refuses to subscribe at all, which is the mechanism that makes the
    rule true for every effect rather than for the two tested here.

    The bell's row has to **persist**, not merely appear. The inbox is
    derived on every reconcile from what the mirrors report as pending,
    so an effect-sourced row that was not part of that derivation would
    be created and then pruned microseconds later — visible to nothing
    and to nobody. That is the regression this polls for rather than
    checks once, and it is why the clear-on-focus half is asserted right
    after: a row that survives reconcile must still have a way out.
    """
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        parked = quiet_tab(session, project, host.env.launch_cwd)
        # Ring a tab that is not the one being shown, so the row's
        # arrival cannot be confused with the focused tab's own state.
        parked_key = host_key(roost, parked)
        focus(roost, parked_key)
        key = sibling_key(parked_key, tab)

        # No lease, no stream: the effects a lease holder is reading
        # right now are not on offer to anyone else.
        with EventStream(host.env.socket) as stranger:
            with pytest.raises(RoostError) as refused:
                stranger.subscribe()
            assert refused.value.code == "connect-required", refused.value

        session.tab_feed_pty_bytes(tab, b"\x07")
        wait_until(
            lambda: f"notif:{key}" in inbox_ids(roost),
            30.0,
            "a bell from a host tab to reach the client's inbox",
        )

        # Still there several reconciles later — including reconciles
        # driven by churn that has nothing to do with this tab, which is
        # what a derived row has to survive.
        deadline = time.monotonic() + scaled_timeout(2.0)
        session.set_title(parked, marker("CHURN"))
        while time.monotonic() < deadline:
            assert f"notif:{key}" in inbox_ids(roost), (
                "the bell's inbox row was retired by a reconcile"
            )

        # And it clears the way every other attention marker clears.
        focus(roost, key)
        wait_until(
            lambda: f"notif:{key}" not in inbox_ids(roost),
            30.0,
            "focusing the rung tab to clear its bell",
        )


def test_an_osc52_write_in_a_host_tab_reaches_the_clients_clipboard(host, roost):
    """OSC 52 crosses the wire as an effect and lands on the *client's*
    clipboard — the machine with the user on it, not the one with the
    shell.

    Seeded with a baseline first: a clipboard that already held the
    payload would pass this without the effect ever arriving. Skipped
    where the platform has no usable clipboard (headless Wayland refuses
    ownership without a focused seat), which is the same boundary
    `test_osc52.py` is scoped by.
    """
    baseline = marker("baseline")
    try:
        roost.clipboard_write("system", baseline)
        usable = roost.clipboard_dump("system") == baseline
    except RoostError:
        usable = False
    if not usable:
        pytest.skip("no usable system clipboard on this display (see test_osc52.py)")

    host.connect_and_wait()
    with host.client() as session:
        tab = quiet_tab(session, first_project(session), host.env.launch_cwd)
        host_key(roost, tab)
        payload = marker("HOSTCLIP")
        session.tab_feed_pty_bytes(tab, osc52(payload.encode()))
        wait_until(
            lambda: roost.clipboard_dump("system") == payload,
            30.0,
            "the host's OSC 52 write to reach this client's clipboard",
        )


# ---------------------------------------------------------------------------
# 8. AC11 — a local tab and a host tab with the same number
# ---------------------------------------------------------------------------


def test_a_local_and_a_host_tab_sharing_an_id_stay_apart(roost, project, session_env):
    """Two id-spaces, one client (architecture §12, deferred to HS-2).

    Engine ids are drawn per host, so the moment a session is connected
    the number `7` names two different tabs. Every keyed surface is
    supposed to be host-qualified; the fence is that an event about one
    of them changes nothing about the other.

    The collision is **forced**, not waited for. Both counters only ever
    go up, so on a workspace that has been running a while they can no
    longer meet by themselves — and a case that quietly skips is a case
    that stopped testing anything. A session hydrates its id counter from
    its own `state.json`, so seeding that file before the daemon's first
    start puts its opening tab on whatever number this test wants:
    `allocate_id` hands out `max(next_id, 1) + 1`, and a first-ever start
    spends its first two ids on the seeded project and its tab.
    """
    require_test_mode(roost)
    shared = roost.open_tab(project, cwd="/tmp", argv=["/bin/sh", "-c", "exec sleep 300"])

    seeded = {"next_id": shared - 2, "projects": []}
    session_env.state_dir.mkdir(parents=True, exist_ok=True)
    session_env.state_json.write_text(json.dumps(seeded))
    start_session(session_env)

    with saved_host(roost, session_env) as host:
        host.connect_and_wait()
        with host.client() as session:
            assert shared in [int(row["id"]) for row in session.tabs()], (
                f"the seeded next_id did not land the session's first tab on {shared}: "
                f"{session.tabs()}"
            )
            key = host_key(roost, shared)
            local_before = roost.tab(shared)["title"]

            renamed = marker("HOSTTITLE")
            session.set_title(shared, renamed)
            wait_until(
                lambda: session.tab(shared)["title"] == renamed,
                15.0,
                "the session to record the new title",
            )
            assert roost.tab(shared)["title"] == local_before, (
                f"a host tab's rename landed on local tab {shared}"
            )

            # The terminals are the sharper half of the same question:
            # `tab.dump` for `h<host>.<id>` and for the bare `<id>` are
            # two different screens, and bytes fed to one must not show
            # up in the other.
            line = marker("HOSTONLY")
            session.tab_feed_pty_bytes(shared, f"{line}\r\n".encode())
            wait_dump_contains(roost, key, line)
            assert line not in roost.dump_text(shared), (
                f"host tab {shared}'s output reached the local tab of the same number"
            )

            # And the other way: a local rename must not reach the host row.
            local_renamed = marker("LOCALTITLE")
            roost.set_title(shared, local_renamed)
            assert roost.tab(shared)["title"] == local_renamed
            assert session.tab(shared)["title"] == renamed, (
                f"a local rename landed on host tab {shared}"
            )

            # An exit is the same question with a bigger blast radius.
            session.close_tab(shared)
            wait_until(lambda: session.tab(shared) is None, 15.0, "the host tab to close")
            assert roost.tab(shared) is not None, (
                f"closing host tab {shared} closed the local tab of the same number"
            )
            assert roost.tab(shared)["title"] == local_renamed


# ---------------------------------------------------------------------------
# 9. AC10 — the attach budget
# ---------------------------------------------------------------------------


def time_to_frame(roost: Roost, key: str, needle: str) -> float:
    """Seconds from asking for a tab to seeing its content, measured
    through the UI's own ops — which is what a user waits for.

    Deliberately polls to a generous ceiling rather than to the budget:
    an attach that is merely slow should report the number it took, not
    a bare timeout. The budget is the caller's assertion.
    """
    deadline = time.monotonic() + scaled_timeout(30.0)
    started_at = time.monotonic()
    focus(roost, key)
    while True:
        text = try_dump_text(roost, key)
        if text is not None and needle in text:
            return time.monotonic() - started_at
        assert time.monotonic() < deadline, f"{key} never rendered {needle!r}"
        time.sleep(0.005)


def test_a_focused_attach_and_a_refocus_both_land_inside_the_budget(host, roost):
    """Plan 037 §7.10's numbers, asserted rather than measured and
    filed.

    Attach-on-focus does a round trip per tab switch, so the budget is
    what keeps the policy honest: if switching to a host tab ever felt
    slower than switching to a local one, the sanctioned fix is a
    linger-before-detach — and this is the assertion that would ask for
    it.
    """
    budget = scaled_timeout(ATTACH_BUDGET_S)
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        other = quiet_tab(session, project, host.env.launch_cwd)
        key = host_key(roost, tab)
        line = marker("BUDGET")
        session.tab_feed_pty_bytes(tab, f"{line}\r\n".encode())
        wait_dump_contains(roost, key, line)
        other_key = host_key(roost, other)

        # First: a cold-ish attach of a tab this client is not showing.
        focus(roost, other_key)
        first = time_to_frame(roost, key, line)

        # Then the resume path, which is the one a user hits repeatedly.
        focus(roost, other_key)
        again = time_to_frame(roost, key, line)

    assert first < budget, f"attach took {first:.3f}s (budget {budget:.3f}s)"
    assert again < budget, f"refocus took {again:.3f}s (budget {budget:.3f}s)"


# ---------------------------------------------------------------------------
# 10. The attention surfaces, across the wire
# ---------------------------------------------------------------------------


def test_focusing_a_host_tab_clears_its_marker_on_the_session(host, roost):
    """Clearing is event-confirmed, and the event comes from the session.

    The client does not retire the row itself — it asks, and the
    session's `tab.notification { has_pending: false }` commit is what
    takes it down (§3.9's no-optimistic-rows rule). So the assertion
    that matters is on the *session's* view: focus reached it, not just
    the local inbox.
    """
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        parked = quiet_tab(session, project, host.env.launch_cwd)
        parked_key = host_key(roost, parked)
        focus(roost, parked_key)

        session.notify(tab, "attention", "please")
        wait_until(lambda: session.has_notification(tab), 15.0, "the session to mark the tab")

        key = host_key(roost, tab)
        wait_until(
            lambda: not session.has_notification(tab),
            30.0,
            "focus to clear the marker on the SESSION",
        )
        assert f"notif:{key}" not in inbox_ids(roost)


def test_a_reconnect_rebuilds_the_attention_rows_from_the_fresh_list(host, roost):
    """A reconnect purges and re-derives (§3.2), and "re-derive" has to
    include attention.

    Rows keyed by the dead incarnation are dropped, so a client that
    only replayed *events* would come back with an empty inbox and a
    session still flagging a tab. The mirror is rebuilt from a fresh
    `tab.list`, and the rows come back with it — under the new
    incarnation, which is what this asserts on.
    """
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        parked = quiet_tab(session, project, host.env.launch_cwd)
        focus(roost, host_key(roost, parked))

        session.notify(tab, "attention", "please")
        wait_until(
            lambda: any(
                row.startswith("notif:h") for row in inbox_ids(roost)
            ),
            30.0,
            "the host's pending tab to reach the inbox",
        )

        host.disconnect()
        host.wait_not_connected()
        host.connect_and_wait()

        # A fresh incarnation, so the row cannot be a survivor of the old
        # one — it was rebuilt from what the session says now.
        wait_until(
            lambda: any(row.startswith("notif:h") for row in inbox_ids(roost)),
            30.0,
            "the reconnect to restore the attention row",
        )
        assert session.has_notification(tab)


def test_closing_a_host_tab_or_its_project_retires_the_inbox_row(host, roost):
    """A row that outlives its tab is a row that cannot be dismissed.

    The session is the authority on which of its tabs exist, so a close
    it commits has to retire the client's row for it — the same way a
    local tab's close does. Both spellings of "gone" are checked: the tab
    itself, and the project taking it down with it. A cascade is the
    easier one to miss, because the row's own tab never gets a close
    event of its own.
    """
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        parked = quiet_tab(session, project, host.env.launch_cwd)
        focus(roost, host_key(roost, parked))

        def host_rows() -> set[str]:
            return {row for row in inbox_ids(roost) if row.startswith("notif:h")}

        session.notify(tab, "attention", "please")
        row = wait_until(
            lambda: next(iter(host_rows()), None),
            30.0,
            "the host's pending tab to reach the inbox",
        )

        session.close_tab(tab)
        wait_until(
            lambda: row not in inbox_ids(roost),
            30.0,
            "the closed host tab's inbox row to retire",
        )

        # The cascade: a project delete takes rows with it.
        second = session.create_project(name="doomed", cwd=str(host.env.launch_cwd))
        doomed = quiet_tab(session, second, host.env.launch_cwd)
        # Opening a tab makes it the session's active row, and a session
        # suppresses attention for whichever row it considers active.
        # `session.set_focus` (HS-3) makes that row follow the client's
        # selection, but only at the client's own edges — a tab this test
        # opens on the session moves the session's active row underneath
        # it. Move the selection back off the tab this half is about, or
        # the notification below is dropped at the source.
        session.focus(parked)
        session.notify(doomed, "attention", "please")
        wait_until(lambda: host_rows(), 30.0, "the doomed project's row to reach the inbox")

        session.delete_project(second)
        wait_until(
            lambda: not host_rows(),
            30.0,
            "a deleted host project's inbox rows to retire",
        )


def test_removing_a_host_fires_nothing_from_a_batch_still_in_flight(host, roost):
    """Forgetting a host must forget its pending work too.

    `host.remove` disconnects first, and the batches already on the wire
    are keyed by an incarnation that no longer resolves — so the honest
    outcome is that nothing from that host reaches an attention surface
    afterwards, however late it lands. The notify is issued in the same
    breath as the removal precisely so there IS something in flight.
    """
    host.connect_and_wait()
    with host.client() as session:
        project = first_project(session)
        tab = quiet_tab(session, project, host.env.launch_cwd)
        parked = quiet_tab(session, project, host.env.launch_cwd)
        focus(roost, host_key(roost, parked))
        assert not any(row.startswith("notif:h") for row in inbox_ids(roost))

        session.notify(tab, "attention", "please")
        host.remove()

    assert roost.call("host.list", {})["hosts"] == []
    # Give anything still in flight the whole budget to misbehave, then
    # assert it did not: a poll that stopped at the first clean read
    # would pass before the batch could arrive.
    deadline = time.monotonic() + scaled_timeout(3.0)
    while time.monotonic() < deadline:
        rows = {row for row in inbox_ids(roost) if row.startswith("notif:h")}
        assert rows == set(), rows
        time.sleep(0.1)


# ---------------------------------------------------------------------------
# 11. Ops parity, cheaply
# ---------------------------------------------------------------------------


def test_roostctl_host_drives_the_same_registry_the_palette_does(host, roost, target):
    """AC9's parity rule, spot-checked where it is cheapest to break.

    `roostctl host` addresses the **UI** socket (hosts are client-side
    state, D8), so a verb with its own implementation would show up here
    as a list the palette disagrees with. `--socket` rather than
    `--target`: this fixture's environment is a session profile, and the
    auto-detect ladder must not be given the chance to pick a different
    UI than the one the rest of the case is driving.
    """
    listed = host.env.roostctl(
        "--socket", str(ui.socket_path(target)), "host", "list"
    )
    assert listed.returncode == 0, listed.stdout + listed.stderr
    assert f"{host.saved_id}  {host.label}" in listed.stdout, listed.stdout
    assert f"host:connect:{host.saved_id}" in host_row_ids(roost)
