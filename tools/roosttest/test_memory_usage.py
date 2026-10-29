"""`tab.memory_usage` on both terminals that can be a tab's authoritative
one (plan 082 B-7).

The op reads libghostty's own measure of a terminal — pages and bytes per
screen, plus two totals — and echoes the caller's tab ref. It runs here
against each lane of `test_program_status.py`'s `lane` fixture:

* `session` — a session tab's server terminal, on the session socket
  (marked `session_daemon`, so it runs in `make e2e-session`);
* `iced` — an in-process tab's own terminal, on the UI socket (unmarked,
  so it runs in the iced lanes).

A host tab on the UI socket — the session's figure, the window's copy as
`mirror`, and the echoed `h<n>.<id>` — is the host-client lane's
(`test_host_client.py`).

The Mac app does not serve the op (`unknown-op`, pinned by a Swift unit
test), so every case is iced-only.

Condition waits only.
"""

from __future__ import annotations

import json
import os
import re
import subprocess

import pytest
from client import RoostError
from session import wait_until
from test_program_status import lane  # noqa: F401  (the fixture, by name)
from util import roostctl_path

# A tab id no test run allocates: projects and tabs share one counter.
UNLISTED_TAB = 9_000_000_000

# Enough output that the primary screen has to take more pages than a
# fresh terminal holds.
HISTORY = "".join(f"history line {n}\r\n" for n in range(3000)).encode()

MEMORY_LINE = re.compile(
    r"(terminal|mirror): (\d+) bytes resident, (\d+) bytes virtual, "
    r"(\d+) pages \(primary (\d+), alternate (\d+)\)"
)


@pytest.fixture(autouse=True)
def iced_only(target):
    if target != "iced":
        pytest.skip("the Mac app does not serve tab.memory_usage (plan 082 D6.2)")


def assert_consistent(usage: dict) -> None:
    """The totals are the two screens summed, and a live terminal holds
    at least one resident page."""
    primary, alternate = usage["primary"], usage["alternate"]
    assert usage["resident_bytes"] == primary["resident_bytes"] + alternate["resident_bytes"], usage
    assert usage["virtual_bytes"] == primary["virtual_bytes"] + alternate["virtual_bytes"], usage
    assert primary["pages"] >= 1, usage
    assert primary["resident_bytes"] > 0, usage


def refusal(call, *args) -> RoostError:
    with pytest.raises(RoostError) as caught:
        call(*args)
    return caught.value


def roostctl(socket: str, *args: str) -> subprocess.CompletedProcess:
    env = {
        key: value
        for key, value in os.environ.items()
        if key not in ("ROOST_SOCKET", "ROOST_TAB_ID", "ROOST_AGENT_HOOK")
    }
    return subprocess.run(
        [roostctl_path(), "--socket", socket, *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=60,
    )


def test_a_tab_reports_its_own_terminal_and_grows_with_history(lane):
    tab = lane.quiet_tab()
    before = lane.client.tab_memory_usage(tab)
    assert before["tab_id"] == str(tab), before
    assert "mirror" not in before, "an authoritative terminal has no mirror"
    assert_consistent(before["terminal"])

    lane.fed(tab, HISTORY)
    after = lane.client.tab_memory_usage(tab)["terminal"]
    assert_consistent(after)
    assert after["primary"]["pages"] > before["terminal"]["primary"]["pages"], (before, after)
    assert after["resident_bytes"] > before["terminal"]["resident_bytes"], (before, after)


def test_an_unknown_or_closed_tab_is_not_found(lane):
    unknown = refusal(lane.client.tab_memory_usage, UNLISTED_TAB)
    assert unknown.code == "not-found", unknown

    tab = lane.quiet_tab()
    lane.client.tab_memory_usage(tab)
    lane.client.close_tab(tab)

    def gone() -> RoostError | None:
        try:
            lane.client.tab_memory_usage(tab)
        except RoostError as error:
            return error
        return None

    closed = wait_until(gone, 30.0, f"tab {tab}'s terminal to go with it")
    assert closed.code == "not-found", closed


def test_roostctl_tab_memory_prints_the_ops_result(lane):
    tab = lane.quiet_tab()

    ran = roostctl(lane.events_socket, "--json", "tab", "memory", "--tab", str(tab))
    assert ran.returncode == 0, ran
    printed = json.loads(ran.stdout)
    assert printed["tab_id"] == str(tab), printed
    assert "mirror" not in printed, printed
    assert_consistent(printed["terminal"])

    ran = roostctl(lane.events_socket, "tab", "memory", "--tab", str(tab))
    assert ran.returncode == 0, ran
    lines = ran.stdout.splitlines()
    assert len(lines) == 1, ran.stdout
    matched = MEMORY_LINE.fullmatch(lines[0])
    assert matched is not None, lines[0]
    label, resident, _virtual, pages, primary, alternate = matched.groups()
    assert label == "terminal", lines[0]
    assert int(resident) > 0, lines[0]
    assert int(pages) == int(primary) + int(alternate) >= 1, lines[0]

    ran = roostctl(lane.events_socket, "tab", "memory", "--tab", str(UNLISTED_TAB))
    assert ran.returncode != 0, ran
    assert "not-found" in ran.stderr, ran.stderr
