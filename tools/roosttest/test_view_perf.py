"""`App::view()` never re-elides a tab pill mid-typing — iced only.

The plan 029 F1 regression: `chrome::elide_to_width` binary-searches over
full `Paragraph` shaping, and the tab-pill loop ran it for every pill on
every `view()` rebuild — 79% of per-keystroke main-thread work. The fix
memoizes the elided label per tab, keyed on every elision input, and
refreshes it from `reconcile()`. This module is the durable guard on that:
it asserts the COUNT `app.render_stats.elide_calls` reports, not a
wall-clock budget — an honest time ceiling on a loaded CI runner would
have to sit so high it could not catch the next 3x regression, while the
count catches the actual failure mode (elision re-entering the
per-keystroke path) exactly.

Shape, in order:

1. **Settle** — a freshly opened tab legitimately elides (new pill, new
   key). Poll until two consecutive reads show a zero `elide_calls` delta,
   so the burst below starts from quiescence rather than racing the
   opening reconcile.
2. **Burst 1** — ~100 single-character echoes at ~30/s. `elide_calls`
   must be 0 and `view_calls` must be > 0 (a zero-view burst would make
   the guard vacuous).
3. **Lifecycle pulse** — `tab.set_title` changes a key, so the memo MUST
   recompute: `elide_calls` delta >= 1. Without this the burst assertions
   could be green because the cache was never consulted at all.
4. **Burst 2** — ~30 more echoes after the recompute: back to zero. That
   is the skip-on-match half of the memo.

The per-view microseconds are PRINTED, never asserted — they are useful
to a human reading the CI log and meaningless as a gate.

Caveat on that number: IPC `tab.write` is a `UiRequest`, which marks the
engine batch dirty, so this path reconciles once per keystroke. Real
winit keystrokes do not (pure PTY byte batches never set `dirty` —
`engine_feed.rs`), so the printed timing is the pessimistic IPC path, not
a pure typing-latency measurement. That is deliberate: it is the stricter
of the two, and it is the path the `tools/perf/echo-latency.py` probe
rides too.

Iced-only: `view_calls`/`elide_calls` are iced instrumentation (plan 029
C1). GTK reports structurally-uniform zeros, which would green this
module vacuously, and the Mac handler does not send the fields at all —
so the whole module skips off `--roost-target`, the way
`test_tab_strip_pixels.py` guards its iced-only chrome assertions.
"""

from __future__ import annotations

import time

import pytest

from client import Timeout, scaled_timeout

TAB_TITLE = "view-perf-guard"
RETITLED = "view-perf-guard-renamed"

# ~30 keystrokes/second, the pace a fast human types at and the one
# `tools/perf/echo-latency.py` uses, so the two read the same counters
# under the same load.
KEY_INTERVAL = 1.0 / 30.0
BURST_KEYSTROKES = 100
SECOND_BURST_KEYSTROKES = 30

# The quiescence probe: two consecutive zero-delta reads this far apart.
SETTLE_INTERVAL = 0.25
SETTLE_TIMEOUT = 20.0


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip(
            "view/elide render-stats counters are iced instrumentation "
            "(plan 029 C1); gtk reports zeros and mac omits the fields"
        )


def _stats(roost, reset: bool = False) -> dict[str, int]:
    """`app.render_stats`, ints. Every counter rides the wire as a string
    (`string_int64`), so a bare `>`/`==` against a raw value would compare
    text."""
    return {
        key: int(value)
        for key, value in roost.call("app.render_stats", {"reset": reset}).items()
    }


def _echo_tab(roost, project) -> int:
    """A `/bin/cat` tab: cat can never emit OSC-0/2, so nothing can retitle
    the tab mid-run (an explicit `tab.open` title does NOT set `user_titled`
    — only `tab.set_title` locks; the argv IS the safeguard here, and a
    title change is exactly what this module's pulse asserts on). Focused so
    its project owns the tab strip and its pill is actually built by
    `view()`."""
    tab = roost.open_tab(
        project, cwd="/tmp", title=TAB_TITLE, cols=100, rows=30, argv=["/bin/cat"]
    )
    roost.focus(tab)
    # Readiness by condition, not by sleep: the tty echoes a typed
    # character back the moment the PTY is live and attached.
    roost.send(tab, "R")
    roost.wait_text(tab, "R")
    return tab


def _settle(roost) -> None:
    """Wait until the UI stops eliding on its own.

    Opening + focusing the tab legitimately recomputes pills (new tab, new
    active tab). Reset-and-read in a loop until two consecutive windows
    are elide-free, so the burst measures typing, not the tail of setup.
    """
    interval = scaled_timeout(SETTLE_INTERVAL)
    deadline = time.monotonic() + scaled_timeout(SETTLE_TIMEOUT)
    _stats(roost, reset=True)
    quiet = 0
    while True:
        time.sleep(interval)
        elided = _stats(roost, reset=True)["elide_calls"]
        quiet = quiet + 1 if elided == 0 else 0
        if quiet >= 2:
            return
        if time.monotonic() >= deadline:
            raise Timeout(
                f"the UI never stopped eliding: last window recorded {elided} "
                f"elide calls after {SETTLE_TIMEOUT}s (scaled)"
            )


def _burst(roost, tab: int, char: str, count: int) -> dict[str, int]:
    """Drive `count` single-character echoes and return the counter deltas
    for that window alone."""
    _stats(roost, reset=True)
    for _ in range(count):
        roost.send(tab, char)
        time.sleep(KEY_INTERVAL)
    # The echo has to have LANDED before the counters are read, or a burst
    # whose last writes are still in flight would under-count both sides.
    roost._wait(
        lambda: roost.dump_text(tab).count(char) >= count,
        10.0,
        f"tab {tab} echoed {count}x{char!r}",
    )
    return _stats(roost)


def test_typing_never_re_elides_a_tab_pill(roost, project):
    tab = _echo_tab(roost, project)
    _settle(roost)

    first = _burst(roost, tab, "x", BURST_KEYSTROKES)
    assert first["view_calls"] > 0, (
        "no view rebuild happened during the burst, so a zero elide count "
        "proves nothing — the guard would be vacuous"
    )
    assert first["elide_calls"] == 0, (
        f"{first['elide_calls']} tab-pill elisions during {BURST_KEYSTROKES} "
        "keystrokes: the pill memo is missing on the typing path (plan 029 F1)"
    )
    print(
        f"\nview: {first['view_calls']} calls, "
        f"{first['view_nanos'] / first['view_calls'] / 1000.0:.1f}us avg "
        f"(IPC tab.write path; informational, not asserted)"
    )

    # The pulse: a key change MUST recompute, or the zeros above would be
    # the signature of a cache nothing ever consults.
    _stats(roost, reset=True)
    roost.set_title(tab, RETITLED)
    roost._wait(
        lambda: _stats(roost)["elide_calls"] >= 1,
        10.0,
        "a renamed tab re-elides its pill label",
    )

    second = _burst(roost, tab, "y", SECOND_BURST_KEYSTROKES)
    assert second["view_calls"] > 0
    assert second["elide_calls"] == 0, (
        f"{second['elide_calls']} elisions after the rename settled: the memo "
        "recomputes but never skips on a matching key"
    )
