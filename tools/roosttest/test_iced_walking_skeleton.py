"""Required functional gate for the first launchable Iced vertical slice.

Keep this file small and target-neutral in behavior: it proves the native
window is backed by the shared engine, a real PTY, libghostty-vt, and the
common IPC seam. The broader suite replaces this slice milestone by milestone.
"""

from __future__ import annotations

import concurrent.futures
import os
import threading
import uuid

import pytest

from client import Roost, scaled_timeout
import ui
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


def test_iced_screenshot_scale_and_queued_clients(roost, target):
    """Native captures have stable logical scaling and every queued caller
    receives exactly one reply. The latter guards the Iced adapter's single
    in-flight UI-task bridge rather than merely checking PNG encoding."""

    # A preceding test-mode case may have requested a resize whose compositor
    # framebuffer is still in flight. Wait for two identical renderer extents
    # without comparing them to `window_metrics`: under Wayland the renderer
    # surface and decorated window deliberately have different dimensions.
    # This also avoids the test-mode-only `window.resize` operation, so the
    # contract runs from the normal `make e2e-iced` target.
    settled: dict[str, tuple[bytes, int, int]] = {}
    last_size: list[tuple[int, int] | None] = [None]

    def capture_surface_settled() -> bool:
        settled["one"] = roost.screenshot(scale=1)
        size = settled["one"][1:]
        if last_size[0] != size:
            last_size[0] = size
            return False
        return True

    Roost._wait(
        capture_surface_settled,
        timeout=5,
        what="Iced renderer presents the resized screenshot surface",
    )
    one, width, height = settled["one"]
    two, width2, height2 = roost.screenshot(scale=2)
    assert one.startswith(b"\x89PNG\r\n\x1a\n")
    assert two.startswith(b"\x89PNG\r\n\x1a\n")
    assert width > 0 and height > 0
    assert (width2, height2) == (width * 2, height * 2)

    callers_ready = threading.Barrier(3)

    def capture(scale: int) -> tuple[bytes, int, int]:
        client = Roost(ui.socket_path(target), timeout=scaled_timeout(10))
        try:
            callers_ready.wait(timeout=scaled_timeout(5))
            return client.screenshot(scale=scale)
        finally:
            client.close()

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(capture, scale) for scale in (1, 2)]
        callers_ready.wait(timeout=scaled_timeout(5))
        captures = [future.result(timeout=scaled_timeout(10)) for future in futures]

    for scale, (png, captured_width, captured_height) in zip((1, 2), captures):
        assert png.startswith(b"\x89PNG\r\n\x1a\n")
        assert (captured_width, captured_height) == (width * scale, height * scale)
