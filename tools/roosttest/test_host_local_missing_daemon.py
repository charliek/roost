"""Plan 042 W2: `roost-session` unreachable on `localhost` settles once
and says why — never a retry ladder, never silence.

# What this proves

`test_host_ssh.py` proves the *ssh* transport's launch-failure and
retry-ladder behavior. This module is the twin for the *local* daemon
spawn: `HostStateMachine::settled` (`host_conn/state.rs`) publishes
`Disconnected { reason, detail: Some(detail), retry_in: None }` after
exactly one failed spawn attempt, whatever the transport — the
localhost bool never consults it, so nothing here ever arms a retry.
The only thing a lane can assert about "exactly one attempt" is that
`generation` never moves past 1 and `retry` never appears again; the
invariant that the task itself never re-arms is unit-tested in Rust
(`state.rs`'s `a_settled_localhost_never_schedules_a_retry`).

# The override rung, not the three-rung search

With `$ROOST_SESSION_BIN` set to a path that does not exist,
`locate_session_binary` (`session_launch.rs:365-424`) short-circuits at
the very first rung — an explicit override that fails is a hard error,
never a fallthrough to the sibling/PATH guesses — and returns
`"{BIN_ENV}={path} is not a file this user can execute"`. That string,
not the three-rung "cannot find the roost-session binary. Tried: ..."
listing, is what this lane's `detail` assertion is against. The harness
always builds a sibling `roost-session` next to `roost-iced` anyway
(`make e2e-host-missing-daemon`'s prerequisite), so without the
override the daemon would simply start.

# Why the env is set at import, not in a fixture

`$ROOST_SESSION_BIN` is read once by the UI process, out of its own
launch environment, the moment a connect attempt tries to locate the
binary. `conftest.py`'s session-scoped `_ui_session` fixture launches
that UI before any test-scoped fixture runs, so the override has to be
in `os.environ` at *import* time — the same reason `test_host_ssh.py`
sets `$ROOST_SSH_BIN` at import rather than in a fixture (see that
module's header). That is also why this module needs its own pytest
invocation, separate from the shared-UI directory run: importing it
would point every other module's UI at a dead `ROOST_SESSION_BIN` too.

`pytestmark = pytest.mark.host_client` (as `test_host_ssh.py` does) so
a whole-directory run — `make e2e-mac` in particular, where the Swift
app answers `unknown-op` to every `host.*` op — deselects it rather
than failing it.

Condition waits only.
"""

from __future__ import annotations

import contextlib
import os
import time
import uuid

# Before anything else in this module runs, and before the session-scoped
# UI fixture in conftest.py launches: the override has to be in the
# environment the UI process itself is spawned with.
BIN_ENV = "ROOST_SESSION_BIN"
os.environ[BIN_ENV] = "/nonexistent/roost-session-does-not-exist-42"
MISSING_BIN = os.environ[BIN_ENV]

import pytest  # noqa: E402

from client import Roost, scaled_timeout  # noqa: E402
from test_host_client import host_row_ids, wait_until  # noqa: E402

pytestmark = pytest.mark.host_client

# `spawn_failure`'s `Locate` row (`task.rs`), restated rather than
# imported for the same reason `test_host_ssh.py` restates its copy
# constants: this is user-facing wording, and a change to it should have
# to be made twice, on purpose.
REASON = "cannot find roost-session"
ROLLUP = f"disconnected — {REASON}"
# The override rung's own text (`session_launch.rs:365-424`) — a stable
# substring, not the full sentence, so this lane does not pin
# punctuation the Rust side is free to reword.
DETAIL_NEEDLE = "is not a file this user can execute"


def status(roost: Roost, host_id: str) -> dict:
    """This host's one `host.status` row."""
    return roost.host_status(host_id)["hosts"][0]


def wait_settled(roost: Roost, host_id: str, timeout: float = 15.0) -> dict:
    """Wait for the one spawn attempt this lane triggers to settle.

    `generation == 1` is the strongest of the three conditions: a
    settled state can only be reached by an attempt that started, so
    seeing it is also the "exactly one attempt so far" read this test
    goes on to hold. `state == "disconnected"` and `retry is None` rule
    out reading a still-in-flight `connecting` row.
    """

    def settled() -> dict | None:
        row = status(roost, host_id)
        if row["generation"] != 1:
            return None
        if row["state"] != "disconnected":
            return None
        if "retry" in row:
            return None
        return row

    return wait_until(settled, timeout, "the localhost spawn attempt to settle")


def assert_settled_on_missing_daemon(row: dict) -> None:
    """Every field plan 042 AC4 names, read off one `host.status` row."""
    assert row["state"] == "disconnected", row
    assert row["rollup"] == ROLLUP, row
    # Absent, not `null`: the wire omits the key when nothing is armed.
    assert "retry" not in row, row
    assert row["generation"] == 1, row
    assert row["reason"] == REASON, row
    detail = row.get("detail") or ""
    assert f"{BIN_ENV}=" in detail, row
    assert MISSING_BIN in detail, row
    assert DETAIL_NEEDLE in detail, row


def hold(check, seconds: float = 3.0) -> None:
    """Hold an assertion for a window rather than reading it once.

    "Nothing further happens" has no edge to wait for, so a single read
    the instant a settle lands proves only "nothing further *yet*" — a
    regression that armed a retry one poll later would fire after it and
    still pass. Same shape as `test_host_ssh.py`'s `hold`. At the default
    100ms interval, 3s is at least the old 250ms ladder's first eight
    rungs' worth of window.
    """
    deadline = time.monotonic() + scaled_timeout(seconds)
    while True:
        check()
        if time.monotonic() >= deadline:
            return
        time.sleep(0.1)


@pytest.fixture
def missing_daemon_host(roost: Roost):
    """A `localhost` host, saved but not yet connected."""
    label = f"missing-daemon-{uuid.uuid4().hex[:8]}"
    added = roost.call("host.add", {"label": label, "target": "localhost"})["host"]
    host_id = added["id"]
    try:
        yield host_id
    finally:
        with contextlib.suppress(Exception):
            roost.palette_dismiss()
        with contextlib.suppress(Exception):
            roost.call("host.remove", {"id": host_id})


def test_a_missing_local_daemon_settles_once_and_stays_settled(roost, missing_daemon_host):
    """AC4 + AC6: an explicit Connect against an unreachable
    `roost-session` settles disconnected, names `ROOST_SESSION_BIN` in
    `detail`, and never arms a retry — held over a window, not just read
    once at the moment it first settles."""
    fresh = status(roost, missing_daemon_host)
    assert fresh["generation"] == 0, fresh
    assert "retry" not in fresh, fresh

    result = roost.call("host.connect", {"id": missing_daemon_host})
    # The op reports what was asked for, not the far end's verdict — the
    # attempt may already be in flight, or may have failed synchronously
    # fast enough to already read as settled.
    assert result["state"] in ("disconnected", "connecting", "connected"), result

    row = wait_settled(roost, missing_daemon_host)
    assert_settled_on_missing_daemon(row)

    # Still offering Connect (the ↻ Reconnect / Connect verb), never
    # Disconnect — a settled localhost host is exactly as inert as a
    # settled ssh one.
    rows = host_row_ids(roost)
    assert f"host:connect:{missing_daemon_host}" in rows, rows
    assert f"host:disconnect:{missing_daemon_host}" not in rows, rows

    def unchanged() -> None:
        current = status(roost, missing_daemon_host)
        assert_settled_on_missing_daemon(current)

    hold(unchanged)

    # The palette verb survives the hold too.
    rows = host_row_ids(roost)
    assert f"host:connect:{missing_daemon_host}" in rows, rows
    assert f"host:disconnect:{missing_daemon_host}" not in rows, rows
