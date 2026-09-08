"""`tab.dump`'s scrollback, on a UI socket, on both targets (plan 053 R4).

`tab.dump` returned the viewport and nothing else, so everything that
scrolled off was unreadable over IPC — `roostctl tab dump | grep` saw
one screen, and a polling client (the phone's peek) showed a frame
where the tool it replaced showed 200 lines of history. R4 adds a
`scrollback` row count to the request and answers with
`scrollback_rows` plus `scrollback_text`.

The contract this file exists to hold is **adjacency**: `scrollback_text`
is the history immediately above the viewport, its last entry the row
right before `rows_text[0]`, so a caller concatenates the two and reads
one continuous screen. Everything else follows from it — an oversized
ask is clamped rather than refused (a client asking for "everything"
should not have to know the cap), and an unasked dump is byte-identical
to what a pre-053 client already got.

The session socket's half of the same contract lives in
`test_session_attach.py::test_tab_dump_serves_history_above_the_viewport`,
and a host tab's mirror in
`test_host_client.py::test_a_build_skew_connects_in_vt_fallback`.

**Not here: the scrolled-up viewport.** `scrollback_rows` is anchored at
the *current* viewport (`scrollbar().offset`), so scrolling up shrinks
it while adjacency still holds. Scrolling a viewport is a UI act with no
IPC seam on either target — `tab.dispatch_mouse_event` drives the mouse
*encoder* (a wheel becomes a mouse report, or nothing at all when no
tracking mode is on) and never the local scroll route, and
`app.keybind_dispatch` serves one action, `paste`. That case is pinned
where a scroll can be performed directly: `roost-vt`'s reader tests and
their Swift twin (`SelectionFormatterScrollbackTests`).

Content is seeded with `tab.feed_pty_bytes`, never a shell, so the
numbering is exact rather than a race with a prompt.
"""

from __future__ import annotations

import os

import pytest

from util import wait_tab_attached, wait_tab_quiet

# The handlers answer `not-enabled` without the gate, which would make
# every assertion here fail for a reason that has nothing to do with
# scrollback. CI sets it on both targets.
TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="seeding viewport content needs tab.feed_pty_bytes (ROOST_TEST_MODE=1)",
)

# `roost_ipc::messages::MAX_DUMP_SCROLLBACK`. Restated rather than
# imported: a change to it should fail a case, not be absorbed.
MAX_DUMP_SCROLLBACK = 10_000

# Comfortably more than either UI's window can show, so there is real
# history above the viewport whatever the runner's geometry is, and
# comfortably under the 2000-row retention both UIs keep.
SEEDED_LINES = 400

# What a bounded ask takes, small enough to be well inside the history
# the seed produces on any window size.
ASKED = 50


def line(index: int) -> str:
    return f"row-{index:04d}"


def numbered(row: str) -> int:
    """The seeded index a dumped row carries."""
    assert row.startswith("row-"), f"not a seeded row: {row!r}"
    return int(row[4:8])


@pytest.fixture
def seeded(roost, project):
    """A tab whose viewport and history are one continuous numbered run.

    The erase-and-home prefix puts the run at a known origin — the
    shell's own startup output is above it, and `\\x1b[2J` in libghostty
    clears the screen without pushing it into history, so every row from
    here down is one this test wrote.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    # Bytes fed while the shell is still painting would be scanned out
    # of terminal order, and the prompt would land on a seeded row.
    wait_tab_quiet(roost, tab)

    body = "\x1b[2J\x1b[H" + "".join(f"{line(i)}\r\n" for i in range(SEEDED_LINES))
    for at in range(0, len(body), 16 * 1024):
        roost.tab_feed_pty_bytes(tab, body[at : at + 16 * 1024].encode())
    roost.wait_text(tab, line(SEEDED_LINES - 1), timeout=30.0)
    return tab


def test_a_dump_that_asks_for_nothing_stays_what_it_always_was(roost, seeded):
    """The old request, answered the old way — plus the count.

    `scrollback_rows` always comes back, so a client learns there *is*
    history without fetching a row of it; `scrollback_text` is absent
    rather than empty, which is what keeps an unasked response the same
    shape a pre-053 client decodes.
    """
    dumped = roost.dump(seeded)

    assert len(dumped["rows_text"]) == dumped["rows"], dumped["rows"]
    assert "scrollback_text" not in dumped, dumped
    # The seed is longer than any window, so rows went above the top.
    assert dumped["scrollback_rows"] >= SEEDED_LINES - dumped["rows"], dumped


def test_the_history_ends_at_the_row_above_the_viewport(roost, seeded):
    """Adjacency, which is the whole contract.

    Both halves are asserted: the asked-for rows are consecutive among
    themselves, and the last of them is the line immediately before
    `rows_text[0]`. A reader off by one row — a fencepost in the
    selection it formats, or an anchor at the history's top instead of
    the viewport's — passes a "returns N rows" check and fails this one.

    The viewport is asserted unchanged as well: the two arrays are
    distinguishable by construction, and `rows_text` must not start
    carrying history.
    """
    plain = roost.dump(seeded)
    dumped = roost.dump(seeded, scrollback=ASKED)

    assert dumped["rows_text"] == plain["rows_text"], "the ask moved the viewport"
    assert len(dumped["scrollback_text"]) == ASKED, len(dumped["scrollback_text"])

    numbers = [numbered(row) for row in dumped["scrollback_text"]]
    assert numbers == list(range(numbers[0], numbers[0] + ASKED)), numbers
    assert numbered(dumped["rows_text"][0]) == numbers[-1] + 1, (
        dumped["scrollback_text"][-1],
        dumped["rows_text"][0],
    )


def test_asking_for_more_history_than_exists_returns_what_exists(roost, seeded):
    """Not an error, and not padding: a short answer.

    The count is the client's cue — `scrollback_rows` says how much
    there was, `scrollback_text` carries exactly that many, and the tail
    of the long answer is the whole of the short one.
    """
    bounded = roost.dump(seeded, scrollback=ASKED)
    over = roost.dump(seeded, scrollback=bounded["scrollback_rows"] + 500)

    assert over["scrollback_rows"] == bounded["scrollback_rows"]
    assert len(over["scrollback_text"]) == over["scrollback_rows"]
    assert over["scrollback_text"][-ASKED:] == bounded["scrollback_text"]
    assert numbered(over["rows_text"][0]) == numbered(over["scrollback_text"][-1]) + 1


def test_an_ask_above_the_maximum_is_clamped_never_refused(roost, seeded):
    """A client that wants everything says so and is served.

    Refusing would make the cap something every caller has to know; the
    clamp makes "give me all of it" a legal request whose answer is
    bounded by what the tab actually retains (2000 rows on both UIs).
    """
    everything = roost.dump(seeded, scrollback=1_000_000)

    assert len(everything["scrollback_text"]) == everything["scrollback_rows"]
    assert everything["scrollback_rows"] <= 2000, everything["scrollback_rows"]
    assert everything["scrollback_text"] == roost.dump(
        seeded, scrollback=MAX_DUMP_SCROLLBACK
    )["scrollback_text"]
