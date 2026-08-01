"""Required functional gate for the first launchable Iced vertical slice.

Keep this file small and target-neutral in behavior: it proves the native
window is backed by the shared engine, a real PTY, libghostty-vt, and the
common IPC seam. The broader suite replaces this slice milestone by milestone.
"""

from __future__ import annotations

import os
import uuid

import pytest

from client import Roost
from util import drain_until_match, wait_tab_attached


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("Iced walking-skeleton milestone is specific to the iced adapter")


def test_iced_identity_and_real_pty(roost, project, target):
    identity = roost.identify()
    assert identity["app_id"] == "ai.stridelabs.Roost.iced"

    tab = roost.open_tab(project, cwd="/tmp", title="Iced PTY")
    marker = f"ICED_WALK_{uuid.uuid4().hex[:8]}"
    roost.run(tab, f"printf '{marker}\\n'")
    roost.wait_text(tab, marker, timeout=8)
    dump = roost.dump(tab)
    assert marker in "\n".join(dump["rows_text"])
    assert dump["cols"] >= 2 and dump["rows"] >= 2


@pytest.mark.skipif(
    os.environ.get("ROOST_TEST_MODE") != "1",
    reason="window resize + byte injection require ROOST_TEST_MODE=1",
)
def test_iced_resize_and_device_reply(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    before = roost.dump(tab)
    roost.window_resize(820, 520)
    Roost._wait(
        lambda: (roost.dump(tab)["cols"], roost.dump(tab)["rows"])
        != (before["cols"], before["rows"]),
        timeout=5,
        what="Iced window resize re-quantizes the libghostty grid",
    )

    # DA1 proves the libghostty write_pty callback is installed and its
    # generated reply is routed through the shared serial PTY session.
    roost.tab_capture_pty_input(tab, drain=True)
    roost.tab_feed_pty_bytes(tab, b"\x1b[c")
    captured = drain_until_match(roost, tab, rb"\x1b\[\?62;22c")
    assert b"\x1b[?62;22c" in captured
