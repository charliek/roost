"""Discovering a host tab's `h<incarnation>.<id>` wire spelling.

The three host lanes (`test_host_client`, `test_host_ssh`,
`test_host_bootstrap`) all address a host tab by an **incarnation** —
minted afresh on every connect attempt (`keys.rs`) and reported by no
op, so it is discovered rather than assumed. [`host_key`] is that
discovery.

It lives here, and not in `test_host_client.py`, for one reason: that
module imports pytest, so `tools/roosttest_unit`'s bare-`python3` lane
cannot import it — and this search *needs* a unit fence. Whether a probe
finds its target is range arithmetic wearing a race's clothing: the
shape this file replaced passed five consecutive local runs of the
host-client lane and still failed CI. Arithmetic that can only be tested
by launching two processes and hoping is arithmetic that stays broken.
See `tools/roosttest_unit/test_incarnation_probe.py`.
"""

from __future__ import annotations

from collections.abc import Iterator

from client import Roost, RoostError
from session import wait_until

# How many consecutive incarnations [`incarnation_spans`]'s **first** pass
# scans. Small on purpose: `wait_until` calls the probe repeatedly, so a
# fast miss is a genuine retry rather than the whole wait budget being
# burned on one scan. A 4096-wide single pass (the shape before this one)
# did the opposite — a miss cost tens of seconds, leaving `wait_until`
# effectively one shot. [`_incarnation_floor`] is what keeps a narrow
# first pass enough: it tracks forward on every hit, so the common case
# is a target a handful of incarnations above where the last call landed.
HOST_INCARNATION_WINDOW = 64

# The widest a single pass may get. The cap is the pre-window single-pass
# span: no search ever reaches further than the old shape did, and no one
# pass ever costs more than the old one did.
HOST_INCARNATION_SPAN_CAP = 4096


def incarnation_spans(floor: int) -> Iterator[range]:
    """The incarnations each successive pass of a search covers.

    Every pass is anchored at `floor` and doubles the previous span (64,
    128, 256, … up to [`HOST_INCARNATION_SPAN_CAP`]). Anchored *and*
    growing is the whole design, and each half answers one failure:

    * **growing** — a target far above the floor (a big jump: several
      connects since the floor last moved) is inside a later, wider
      pass, so a narrow first pass costs no coverage.
    * **anchored** — a target *within* an earlier span that was not
      minted yet when that pass read it is re-read by every pass after.
      A window that slid forward by its width instead would read each
      incarnation exactly once and march away from one that went live a
      moment later, which is exactly how the reconnect case (fresh
      incarnation, tab not yet in the client's mirror) went from flaky
      to impossible.

    Infinite by design: `wait_until` owns the deadline, so the search
    stops when the budget says so rather than at an arbitrary ceiling.
    """
    span = HOST_INCARNATION_WINDOW
    while True:
        yield range(floor, floor + span)
        span = min(span * 2, HOST_INCARNATION_SPAN_CAP)


# Where the next call's first pass starts. Incarnations only ever go up
# (`keys.rs` mints, never reuses), so the last one that answered is a
# valid floor for the next probe — that is what keeps a run of many
# connects cheap: each call after the first starts right where the last
# one landed instead of re-scanning from 1.
#
# Only ever moved on a HIT, inside `probe()`. A miss never advances it:
# the host-client lane's UI is not necessarily fresh (`make
# e2e-host-client`, unlike the ssh lane, does not force `--roost-fresh` —
# it is fine reusing a developer's already-running instance, hundreds of
# connects in), so at the moment of a miss the incarnation this call is
# looking for may not be minted yet (`host.connect` returns before the
# engine-feed hop that actually creates the `HostConn`). Advancing the
# shared floor past a target that does not exist yet would strand it
# there for every later call in the run, not just this one.
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

    One pass per `wait_until` attempt, widening as described on
    [`incarnation_spans`]. The widening is this call's own, so it never
    touches the shared floor and can never strand it; the floor itself
    only moves on a hit, so the *next* call starts narrow again.
    """
    global _incarnation_floor
    passes = incarnation_spans(_incarnation_floor)

    def probe() -> str | None:
        global _incarnation_floor
        for incarnation in next(passes):
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
