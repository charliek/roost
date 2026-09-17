"""Fast unit coverage for `util.is_live_wayland_desktop` (issue #488).

Eight pixel/render-stat E2E tests self-skip when `WAYLAND_DISPLAY` points
at a live desktop compositor rather than the harness's own
`tools/wayland/weston-run.sh` socket (`wayland-roost-$$`,
`weston-run.sh:43`) — a live desktop's own window decorations and output
scaling make captured pixels unreliable. The predicate that decides this
is the one thing standing between "CI always runs those eight tests" and
"a future socket rename silently skips them" — so it is pinned here,
directly, with no UI and no pytest: `roosttest_unit` runs under CI's bare
`python3 -m unittest discover`, and `util.py` keeps this function
pytest-free for exactly that reason (see `util._pytest`, and commit
627b40d for the precedent of keeping a harness module importable
without pytest).
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOSTTEST_DIR = Path(__file__).resolve().parents[1] / "roosttest"
sys.path.insert(0, str(ROOSTTEST_DIR))

from util import is_live_wayland_desktop  # noqa: E402


class IsLiveWaylandDesktopTests(unittest.TestCase):
    def test_unset_is_not_live(self) -> None:
        self.assertFalse(is_live_wayland_desktop(None))

    def test_empty_is_not_live(self) -> None:
        self.assertFalse(is_live_wayland_desktop(""))

    def test_live_desktop_socket_wayland_0(self) -> None:
        self.assertTrue(is_live_wayland_desktop("wayland-0"))

    def test_live_desktop_socket_wayland_1(self) -> None:
        self.assertTrue(is_live_wayland_desktop("wayland-1"))

    def test_harness_socket_is_not_live(self) -> None:
        self.assertFalse(is_live_wayland_desktop("wayland-roost-1234"))

    def test_value_merely_containing_the_harness_prefix_is_live(self) -> None:
        # Must be a prefix match, not a substring match — a socket name
        # that only happens to contain "wayland-roost-" partway through
        # is not the harness's own socket and must still be treated as a
        # live desktop.
        self.assertTrue(is_live_wayland_desktop("xwayland-roost-1234"))


if __name__ == "__main__":
    unittest.main()
