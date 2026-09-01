"""Fast unit coverage for the host lanes' incarnation search.

`host_probe.host_key` discovers the `h<incarnation>.<id>` spelling of a
host tab by focusing upward until one answers. Whether it finds its
target is range arithmetic, not luck — but it looks like luck, because
the thing it races is a tab arriving in the client's mirror. A shape
that read each incarnation exactly once and slid its window forward on
every miss passed five consecutive local runs of `make e2e-host-client`
and still failed CI: on a reconnect the fresh incarnation sits one above
the floor, the first pass reads it before the mirror has the tab, and
the window then marches away and never looks back.

So the arithmetic is fenced here instead of there: no pytest, no UI, no
two processes, and a wrong answer is deterministic.
"""

from __future__ import annotations

import itertools
import sys
import unittest
from pathlib import Path

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

import host_probe  # noqa: E402
from client import RoostError  # noqa: E402
from host_probe import (  # noqa: E402
    HOST_INCARNATION_SPAN_CAP,
    HOST_INCARNATION_WINDOW,
    incarnation_spans,
)


class FakeFocus:
    """A stand-in UI socket that answers `tab.focus` for exactly one key,
    and only once `live_after` probes have gone by.

    The delay is the whole point: `host.connect` returns before the
    incarnation it minted has the tab in the client's mirror, so the key
    the search is hunting really does answer `not-found` for a moment and
    only then becomes live.
    """

    def __init__(self, key: str, live_after: int = 0) -> None:
        self.key = key
        self.live_after = live_after
        self.probes: list[str] = []

    def call(self, op: str, params: dict) -> dict:
        assert op == "tab.focus", op
        asked = params["tab_id"]
        self.probes.append(asked)
        if len(self.probes) > self.live_after and asked == self.key:
            return {}
        raise RoostError("not-found", asked)


class IncarnationSpanTests(unittest.TestCase):
    """The three properties the pass sequence has to hold at once."""

    def spans(self, floor: int, count: int) -> list[range]:
        return list(itertools.islice(incarnation_spans(floor), count))

    def test_the_first_pass_is_narrow_enough_to_be_worth_retrying(self) -> None:
        # `wait_until` re-calls the probe; a first pass that scanned
        # thousands of incarnations would burn the whole budget on one
        # attempt, which is the flake this window replaced.
        first = self.spans(7, 1)[0]
        self.assertEqual(first, range(7, 7 + HOST_INCARNATION_WINDOW))

    def test_every_pass_is_anchored_at_the_floor(self) -> None:
        # The regression, stated as arithmetic: a sliding window would
        # start pass 2 at floor + 64 and never read floor + 1 again.
        for span in self.spans(7, 12):
            self.assertEqual(span.start, 7)

    def test_the_spans_grow_and_then_stop_growing(self) -> None:
        widths = [len(span) for span in self.spans(1, 12)]
        self.assertEqual(widths[:4], [64, 128, 256, 512])
        self.assertEqual(max(widths), HOST_INCARNATION_SPAN_CAP)
        self.assertEqual(widths[-1], HOST_INCARNATION_SPAN_CAP)
        self.assertEqual(sorted(widths), widths)

    def test_a_target_once_covered_stays_covered(self) -> None:
        # What a late arrival needs: being inside pass *k* is a promise
        # about every pass after it, not just that one.
        floor = 5
        spans = self.spans(floor, 10)
        for target in (floor, floor + 1, floor + 63, floor + 64, floor + 4095):
            covering = [i for i, span in enumerate(spans) if target in span]
            self.assertTrue(covering, f"{target} is never covered")
            self.assertEqual(covering, list(range(covering[0], len(spans))))


class HostKeyTests(unittest.TestCase):
    """The search itself, against a socket that is not there."""

    def setUp(self) -> None:
        self.saved_floor = host_probe._incarnation_floor
        host_probe._incarnation_floor = 1

    def tearDown(self) -> None:
        host_probe._incarnation_floor = self.saved_floor

    def test_it_finds_a_target_whenever_it_appears(self) -> None:
        cases = {
            # The CI regression: a reconnect's fresh incarnation sits
            # just above the floor, but its tab reaches the client's
            # mirror only after the first pass has already read it.
            "just above the floor, and late": (3, 200),
            # What the growth is for: many connects since the floor moved.
            "far above the floor": (900, 0),
            "far above the floor, and late": (300, 400),
        }
        for name, (incarnation, live_after) in cases.items():
            with self.subTest(name):
                host_probe._incarnation_floor = 1
                fake = FakeFocus(f"h{incarnation}.7", live_after=live_after)
                self.assertEqual(host_probe.host_key(fake, 7, timeout=5.0), fake.key)
                # A hit is also what advances the shared floor, so the
                # next call in a run starts narrow again.
                self.assertEqual(host_probe._incarnation_floor, incarnation)

    def test_a_miss_leaves_the_shared_floor_where_it_was(self) -> None:
        # A miss does not prove the target does not exist yet, so moving
        # the floor past it would strand every later call in the run.
        host_probe._incarnation_floor = 12
        never = FakeFocus("h1.7", live_after=10**9)
        with self.assertRaises(TimeoutError):
            host_probe.host_key(never, 7, timeout=0.2)
        self.assertEqual(host_probe._incarnation_floor, 12)
        self.assertTrue(never.probes)
        self.assertEqual(never.probes[0], "h12.7")


if __name__ == "__main__":
    unittest.main()
