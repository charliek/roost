"""Iced cell-paint pixel guards — plan 073 D6 (#579).

What the draw pass decides and `tab.dump_resolved` never sees, since
neither is baked into a row (the `RenderedRow` overlay invariant):

* **A focused block cursor inverts its glyph.** The cell is opaque in the
  cursor color and the glyph is redrawn in the terminal background, as
  Swift (`drawCursorBlock`) and Ghostty do. It used to be an α0.55 quad
  under an untouched glyph.
* **A selection paints Ghostty's colors.** An opaque
  `selection-background` with the glyph in the theme's
  `selection-foreground`. It used to be an α0.35 tint under an untouched
  glyph.

Cells are located with the walking-skeleton marker: an explicit-background
cell at (0, 0) pins the grid origin and measures one cell. A corner pixel
must be exact, since quads paint opaque colors; the ink checks leave room
for antialiased glyph edges, never for the ink itself.

Iced-only: the pixel geometry is the iced adapter's (walking-skeleton
precedent), so the module skips under `--roost-target mac`.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

import ui
from client import Roost
from conftest import skip_on_live_wayland_desktop
from test_sidebar_collapse_persistence import _toggle_to_visible
from test_sidebar_pixels import _capture
from util import BARE_SHELL_ARGV, wait_for_config_line, wait_tab_quiet

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# crates/roost-iced/src/terminal_widget.rs — edge-pinned grid, no gutter.
TERMINAL_PADDING = 0
# Fed LAST, after every state change a test makes, and the cursor put back
# where it was. An `app.screenshot` can capture a frame drawn before the
# latest change, so only a frame showing this marker shows the scene.
MARKER = (17, 201, 93)
MARKER_BYTES = b"\x1b7\x1b[H\x1b[48;2;17;201;93m \x1b[0m\x1b8"
# "XYZ" sits on this 0-based row, at columns 0-2.
SCENE_ROW = 2
SCENE_TEXT = "XYZ"
SCENE_BYTES = f"\x1b[2J\x1b[{SCENE_ROW + 1};1H{SCENE_TEXT}".encode()

# An OSC 12 cursor override, so the expected cursor color doesn't depend on
# the harness theme. Far from roost-dark's foreground and background.
CURSOR_HEX = "#ff8800"
CURSOR = (0xFF, 0x88, 0x00)

# Synthwave, because its foreground, selection background and selection
# foreground are all far apart (the bundled theme file's values). GitHub
# Dark Default's selection background equals its foreground, which would
# make the "no foreground ink" check impossible to pass.
SELECTION_THEME = "Synthwave"
SELECTION_BACKGROUND = (0x19, 0xCD, 0xE6)
SELECTION_FOREGROUND = (0x00, 0x00, 0x00)

# Ghostty-style ink: the glyph must carry no pixel this close to the cell's
# normal foreground.
FOREGROUND_TOL = 8
# How close the strongest selected ink must come to `selection-foreground`.
# Thin strokes don't reach full coverage at every pixel; the strongest one
# does nearly.
INK_TOL = 48


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("cell-paint pixel geometry is the iced adapter's (plan 073 D6)")


_skip_wayland = skip_on_live_wayland_desktop()


def _pixel(shot, x: int, y: int) -> tuple[int, int, int]:
    width, _height, bpp, pixels = shot
    offset = (y * width + x) * bpp
    return tuple(pixels[offset : offset + 3])


def _dist(a, b) -> int:
    return max(abs(x - y) for x, y in zip(a, b, strict=True))


def _hex_rgb(value: str) -> tuple[int, int, int]:
    return tuple(bytes.fromhex(value.removeprefix("#")))


def _color_run(shot, x: int, y: int, dx: int, dy: int, color) -> int:
    width, height, _bpp, _pixels = shot
    length = 0
    while 0 <= x < width and 0 <= y < height and _pixel(shot, x, y) == color:
        length += 1
        x += dx
        y += dy
    return length


def _scene_tab(roost, project) -> int:
    _toggle_to_visible(roost)
    tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_quiet(roost, tab)
    return tab


def _seed(roost, tab, scene: bytes) -> None:
    """Feed `scene` until the viewport holds it — the bare shell can repaint
    its prompt after `wait_tab_quiet` (walking-skeleton precedent)."""

    def settled() -> bool:
        roost.tab_feed_pty_bytes(tab, scene)
        rows = roost.dump(tab)["rows_text"]
        return len(rows) > SCENE_ROW and rows[SCENE_ROW] == SCENE_TEXT

    Roost._wait(settled, 10.0, f"{SCENE_TEXT!r} seeded on row {SCENE_ROW}")


def _resolved_text_cell(roost, tab) -> dict:
    cells = roost.tab_dump_resolved(tab)["cells"]
    return next(c for c in cells if c["row"] == SCENE_ROW and c["col"] == 0)


class _Grid:
    """One screenshot, with the grid origin and cell size measured off the
    marker cell."""

    def __init__(self, shot, x0: int, y0: int, cell_w: int, cell_h: int):
        self.shot, self.x0, self.y0, self.cell_w, self.cell_h = shot, x0, y0, cell_w, cell_h

    def corner(self, col: int, row: int) -> tuple[int, int, int]:
        """The top-left interior pixel: inside the cell's quad, clear of its
        edges and of the glyph."""
        return _pixel(self.shot, self.x0 + col * self.cell_w + 1, self.y0 + row * self.cell_h + 1)

    def pixels(self, col: int, row: int) -> list[tuple[int, int, int]]:
        """The cell's pixels, inset one column each side so a neighbour's
        antialiased glyph edge can't count as this cell's ink."""
        left = self.x0 + col * self.cell_w + 1
        top = self.y0 + row * self.cell_h
        return [
            _pixel(self.shot, x, y)
            for y in range(top, top + self.cell_h)
            for x in range(left, left + self.cell_w - 2)
        ]


def _capture_grid(roost, tab, path: Path) -> _Grid:
    """Mark the origin, then capture until a frame shows the mark."""
    roost.tab_feed_pty_bytes(tab, MARKER_BYTES)
    metrics = roost.window_metrics()
    term_x = int(metrics["sidebar_width"]) + TERMINAL_PADDING
    term_y = round(roost.terminal_top(metrics)) + TERMINAL_PADDING
    latest: dict = {}

    def painted() -> bool:
        shot = _capture(roost, path)
        if shot is None:
            return False
        scale = max(1, round(shot[0] / metrics["window_width"]))
        latest["shot"], latest["scale"] = shot, scale
        return _pixel(shot, term_x * scale + 1, term_y * scale + 1) == MARKER

    Roost._wait(painted, 10.0, "a frame showing the origin marker")
    shot, scale = latest["shot"], latest["scale"]
    x0, y0 = term_x * scale, term_y * scale
    cell_w = (
        _color_run(shot, x0 + 1, y0 + 1, 1, 0, MARKER)
        + _color_run(shot, x0 + 1, y0 + 1, -1, 0, MARKER)
        - 1
    )
    cell_h = (
        _color_run(shot, x0 + 1, y0 + 1, 0, 1, MARKER)
        + _color_run(shot, x0 + 1, y0 + 1, 0, -1, MARKER)
        - 1
    )
    assert cell_w >= 4 * scale and cell_h >= 8 * scale, (cell_w, cell_h)
    return _Grid(shot, x0, y0, cell_w, cell_h)


def _artifact(tmp_path: Path, name: str) -> Path:
    artifact_dir = Path(os.environ.get("ROOST_E2E_ARTIFACT_DIR", tmp_path))
    artifact_dir.mkdir(parents=True, exist_ok=True)
    renderer = os.environ.get("ICED_BACKEND", "best").replace("/", "-")
    return artifact_dir / f"{name}-{renderer}.png"


@pytest.mark.skipif(
    not TEST_MODE,
    reason="scene injection and window focus require ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestCellPaint:
    def test_a_focused_block_cursor_inverts_its_glyph(self, roost, project, tmp_path):
        tab = _scene_tab(roost, project)
        # Visible, steady block, a known color; then the cursor steps back
        # onto the Z.
        scene = (
            b"\x1b[?25h\x1b[2 q"
            + f"\x1b]12;{CURSOR_HEX}\x07".encode()
            + SCENE_BYTES
            + b"\x1b[D"
        )
        _seed(roost, tab, scene)
        cursor = roost.dump(tab)["cursor"]
        assert (cursor["row"], cursor["col"]) == (SCENE_ROW, 2), cursor
        assert roost.app_active_terminal_focused()

        text = _resolved_text_cell(roost, tab)
        foreground, background = _hex_rgb(text["fg"]), _hex_rgb(text["bg"])
        assert _dist(CURSOR, foreground) > 64 and _dist(CURSOR, background) > 64

        # An unfocused cursor draws hollow, and a headless display may never
        # focus the window. Set just before the capture: on the X11 lane, a
        # focus set before the tab opened was gone by the capture.
        roost.app_set_window_focus(focus=True)
        grid = _capture_grid(roost, tab, _artifact(tmp_path, "cursor-block"))
        cell = grid.pixels(2, SCENE_ROW)
        foreground_ink = [px for px in cell if _dist(px, foreground) <= FOREGROUND_TOL]
        assert not foreground_ink, (
            f"the glyph under the block cursor kept its foreground {foreground}: "
            f"{foreground_ink[:4]}"
        )
        assert grid.corner(2, SCENE_ROW) == CURSOR
        ink = max(cell, key=lambda px: _dist(px, CURSOR))
        assert _dist(ink, background) < _dist(ink, CURSOR), (
            f"the strongest ink {ink} is not the inverted background {background}"
        )

    def test_a_selection_paints_the_theme_selection_colors(
        self, roost, project, palette, tmp_path
    ):
        config_path = ui.owned_session_config_path()
        if config_path is None:
            pytest.skip("switching the theme requires a harness-owned config copy")
        tab = _scene_tab(roost, project)
        palette.palette_open()
        themes = palette.palette_activate("select_theme")
        original = themes["items"][themes["selection"]]
        assert original["id"] != SELECTION_THEME, original
        palette.palette_dismiss()
        try:
            palette.palette_open()
            palette.palette_activate("select_theme")
            assert palette.palette_activate(SELECTION_THEME)["open"] is False
            Roost._wait(
                lambda: _resolved_text_cell(roost, tab)["bg"].lower() == "#000000",
                5.0,
                f"{SELECTION_THEME} reaches the tab",
            )
            _seed(roost, tab, b"\x1b[?25l" + SCENE_BYTES)
            foreground = _hex_rgb(_resolved_text_cell(roost, tab)["fg"])
            assert _dist(foreground, SELECTION_BACKGROUND) > 64
            assert _dist(foreground, SELECTION_FOREGROUND) > 64

            roost.selection_set(tab, (0, SCENE_ROW), (len(SCENE_TEXT) - 1, SCENE_ROW))
            assert roost.selection_dump(tab)["text"] == SCENE_TEXT

            grid = _capture_grid(roost, tab, _artifact(tmp_path, "selection"))
            for col, glyph in enumerate(SCENE_TEXT):
                cell = grid.pixels(col, SCENE_ROW)
                ink = max(cell, key=lambda px: _dist(px, SELECTION_BACKGROUND))
                assert _dist(ink, SELECTION_FOREGROUND) <= INK_TOL, (
                    f"{glyph}: the strongest ink {ink} is not the theme's "
                    f"selection-foreground {SELECTION_FOREGROUND}"
                )
                foreground_ink = [px for px in cell if _dist(px, foreground) <= FOREGROUND_TOL]
                assert not foreground_ink, (
                    f"{glyph}: selected ink kept the foreground {foreground}: "
                    f"{foreground_ink[:4]}"
                )
                assert grid.corner(col, SCENE_ROW) == SELECTION_BACKGROUND, glyph
        finally:
            palette.palette_dismiss()
            palette.palette_open()
            palette.palette_activate("select_theme")
            assert palette.palette_activate(original["id"])["open"] is False
            wait_for_config_line(config_path, "theme", f"theme = {original['id']}")
