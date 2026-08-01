"""Required functional gate for the first launchable Iced vertical slice.

Keep this file small and target-neutral in behavior: it proves the native
window is backed by the shared engine, a real PTY, libghostty-vt, and the
common IPC seam. The broader suite replaces this slice milestone by milestone.
"""

from __future__ import annotations

import concurrent.futures
import os
import sys
import threading
import uuid
from pathlib import Path

import pytest

from client import Roost, scaled_timeout
import ui
from test_sidebar_collapse_persistence import _toggle_to_collapsed, _toggle_to_visible
from util import drain_until_match, wait_tab_attached

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "screenshot"))
import pngtool  # noqa: E402 — pure stdlib PNG decoder, imported not shelled out


TAB_BAR_HEIGHT = 44
TERMINAL_PADDING = 12
ORIGIN_MARKER = (17, 201, 93)


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


@pytest.mark.skipif(
    os.environ.get("ROOST_TEST_MODE") != "1",
    reason="renderer geometry fixture requires test-only PTY byte injection",
)
def test_iced_terminal_widget_uses_its_layout_origin_and_full_extent(
    roost, project, tmp_path
):
    """The terminal is a normal renderer-neutral widget at a non-zero origin.

    Iced 0.14.0's tiny-skia Canvas path applied the sidebar/tab translation
    twice. This focused product-capture guard seeds one distinctive cell and
    samples logical geometry; it deliberately does not compare whole-window
    pixels or encode GTK/Iced visual parity.
    """
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    _toggle_to_visible(roost)
    # Cell (0,0): distinctive explicit background. Row 2: high-contrast ASCII,
    # wide, combining, and plain clusters for a shape-independent glyph
    # fallback/shaping smoke.
    roost.tab_feed_pty_bytes(
        tab,
        b"\x1b[2J\x1b[H\x1b[48;2;17;201;93m \x1b[0m"
        + "\x1b[2;1H\x1b[38;2;233;67;99mA界e\u0301e\x1b[0m".encode(),
    )

    resolved: dict = {}

    def fixture_resolved() -> bool:
        resolved["dump"] = roost.tab_dump_resolved(tab)
        cells = resolved["dump"]["cells"]
        return any(
            c["row"] == 0
            and c["col"] == 0
            and c["bg"].lower() == "#11c95d"
            and c["has_explicit_bg"]
            for c in cells
        ) and any(c["text"] == "A" for c in cells)

    Roost._wait(fixture_resolved, 5.0, "terminal renderer fixture to resolve")
    default_bg = next(c["bg"] for c in resolved["dump"]["cells"] if c["text"] == "A")
    default_rgb = tuple(bytes.fromhex(default_bg.removeprefix("#")))

    artifact_dir = Path(os.environ.get("ROOST_E2E_ARTIFACT_DIR", tmp_path))
    artifact_dir.mkdir(parents=True, exist_ok=True)
    renderer = os.environ.get("ICED_BACKEND", "best").replace("/", "-")

    def pixel(shot, x: int, y: int) -> tuple[int, int, int]:
        width, _height, bpp, pixels = shot
        offset = (y * width + x) * bpp
        return tuple(pixels[offset : offset + 3])

    def foreground_bounds(shot, x0: int, x1: int, y0: int, y1: int):
        width, height, bpp, pixels = shot
        points = []
        for y in range(max(0, y0), min(height, y1)):
            for x in range(max(0, x0), min(width, x1)):
                offset = (y * width + x) * bpp
                red, green, blue = pixels[offset : offset + 3]
                if red >= 140 and red > green * 3 // 2 and red > blue:
                    points.append((x, y))
        if not points:
            return None
        return (
            min(x for x, _y in points),
            min(y for _x, y in points),
            max(x for x, _y in points),
            max(y for _x, y in points),
        )

    def assert_geometry(collapsed: bool) -> None:
        expected_sidebar = 0 if collapsed else int(roost.window_metrics()["sidebar_width"])
        shot_path = artifact_dir / f"terminal-widget-{renderer}-{'collapsed' if collapsed else 'expanded'}.png"
        latest: dict = {}

        def painted() -> bool:
            # A sidebar transition resizes/reflows the live PTY. The shell can
            # repaint its prompt after the transition and legitimately replace
            # row 0, so reseed the test-only marker immediately before each
            # capture attempt instead of treating VT contents as resize-stable.
            roost.tab_feed_pty_bytes(
                tab, b"\x1b[H\x1b[48;2;17;201;93m \x1b[0m"
            )
            png, _width, _height = roost.screenshot(scale=1)
            shot_path.write_bytes(png)
            latest["shot"] = pngtool.load(str(shot_path))
            shot = latest["shot"]
            marker_x = expected_sidebar + TERMINAL_PADDING + 1
            marker_y = TAB_BAR_HEIGHT + TERMINAL_PADDING + 1
            return pixel(shot, marker_x, marker_y) == ORIGIN_MARKER

        Roost._wait(
            painted,
            10.0,
            f"terminal marker at the {'collapsed' if collapsed else 'expanded'} layout origin",
        )
        shot = latest["shot"]
        width, height, _bpp, _pixels = shot
        assert pixel(shot, expected_sidebar + 1, TAB_BAR_HEIGHT + 1) == default_rgb
        assert pixel(shot, width - 2, TAB_BAR_HEIGHT + 1) == default_rgb
        assert pixel(shot, width - 2, height - 2) == default_rgb

        if not collapsed:
            glyph_y0 = TAB_BAR_HEIGHT + TERMINAL_PADDING + 18
            glyph_x0 = expected_sidebar + TERMINAL_PADDING
            # Keep this semantic rather than a glyph-shape golden: ASCII must
            # draw; the CJK glyph must reach its second logical cell (a
            # one-cell tofu box cannot pass); and e+combining-acute must rise
            # above the adjacent plain e. Exact outlines and antialiasing stay
            # renderer/platform-owned. The sidebar collapse resizes/reflows
            # the PTY, so glyphs are checked before that transition.
            assert foreground_bounds(
                shot, glyph_x0, glyph_x0 + 8, glyph_y0, glyph_y0 + 18
            )
            assert foreground_bounds(
                shot, glyph_x0 + 17, glyph_x0 + 25, glyph_y0, glyph_y0 + 18
            )
            combined = foreground_bounds(
                shot, glyph_x0 + 25, glyph_x0 + 34, glyph_y0, glyph_y0 + 18
            )
            plain = foreground_bounds(
                shot, glyph_x0 + 34, glyph_x0 + 42, glyph_y0, glyph_y0 + 18
            )
            assert combined and plain
            assert combined[1] < plain[1]

    try:
        assert_geometry(collapsed=False)
        _toggle_to_collapsed(roost)
        assert_geometry(collapsed=True)
    finally:
        _toggle_to_visible(roost)


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
