"""Kitty images on screen — plan 088 D3.8.

`tab.dump_resolved.images` (`test_kitty_images.py`) says what a terminal
would draw; these read the pixels the iced widget drew: where a placement
lands, which of the text and the cell backgrounds it sits above, that a
crop shows only its source rect, that nothing paints the padding past the
grid, and that a freed image leaves its cells.

Each image is one solid colour (two for the crop), sized in the terminal's
own cell pixels so it draws unscaled, which keeps every renderer's pixels
exact (one case stretches a single pixel over a cell on purpose, which
tiny-skia can't place exactly). Cells
are located with the cursor-pixel tests' marker: an explicit-background
cell at (0, 0), fed last, pins the grid origin and measures a cell. Every
capture marks in a colour no earlier capture used, so only a frame drawn
after this scene was fed can show it: a previous test's identical scene
never passes for this one's. A cell's pixels are read one pixel in from
its edges.
"""

from __future__ import annotations

import base64
import itertools
import os

import pytest

from client import Roost
from conftest import skip_on_live_wayland_desktop
from test_cursor_pixels import INK_TOL, _artifact, _color_run, _dist, _pixel
from test_cursor_pixels import _Grid as _CellGrid
from test_kitty_images import cell_px, free, placeholders, quiet_tab, virtual
from test_sidebar_pixels import _capture
from util import show_sidebar

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="scene injection requires ROOST_TEST_MODE=1 in the UI's launch env",
)

_skip_wayland = skip_on_live_wayland_desktop()

#: Each capture's marker green channel, never repeated in a run. The red
#: and blue stay far from every image colour and the theme background.
_MARKER_GREENS = itertools.count(100)

RED = (230, 40, 40)
GREEN = (40, 220, 60)
BLUE = (30, 60, 230)
YELLOW = (240, 220, 20)
ORANGE = (250, 140, 0)
CYAN = (0, 200, 210)
PURPLE = (150, 40, 200)
CELL_BG = (200, 0, 200)
INK = (255, 255, 255)

#: Libghostty's below-background threshold is `z < i32::MIN / 2`.
BELOW_BACKGROUND_Z = -1073741825

#: Kitty's chunk size for a direct transmission's base64.
CHUNK = 4096


def solid(color, width: int, height: int) -> bytes:
    return bytes((*color, 255)) * (width * height)


def halves(left, right, width: int, height: int) -> bytes:
    """`width` × `height` RGBA, the left half `left` and the right `right`."""
    row = bytes((*left, 255)) * (width // 2) + bytes((*right, 255)) * (width - width // 2)
    return row * height


def kitty(keys: str, rgba: bytes) -> bytes:
    """Transmit and place `rgba` with the control `keys`, chunked as kitty
    requires, without moving the cursor or answering."""
    data = base64.b64encode(rgba)
    chunks = [data[i : i + CHUNK] for i in range(0, len(data), CHUNK)]
    out = b""
    for index, chunk in enumerate(chunks):
        more = int(index < len(chunks) - 1)
        head = f"a=T,t=d,f=32,C=1,q=2,{keys},m={more}" if index == 0 else f"m={more}"
        out += f"\x1b_G{head};".encode() + chunk + b"\x1b\\"
    return out


def at(row: int, col: int) -> bytes:
    return f"\x1b[{row + 1};{col + 1}H".encode()


def place(image_id: int, row: int, col: int, rgba: bytes, width: int, height: int, extra="") -> bytes:
    keys = f"i={image_id},s={width},v={height}" + (f",{extra}" if extra else "")
    return at(row, col) + kitty(keys, rgba)


def marker_bytes(color) -> bytes:
    r, g, b = color
    return f"\x1b7\x1b[H\x1b[48;2;{r};{g};{b}m \x1b[0m\x1b8".encode()


class _Grid(_CellGrid):
    def cells(self, col: int, row: int, cols: int = 1) -> list[tuple[int, int, int]]:
        """The pixels of `cols` cells from (`col`, `row`), one in from
        their outer edges."""
        left = self.x0 + col * self.cell_w + 1
        right = self.x0 + (col + cols) * self.cell_w - 1
        top = self.y0 + row * self.cell_h + 1
        bottom = self.y0 + (row + 1) * self.cell_h - 1
        return [_pixel(self.shot, x, y) for y in range(top, bottom) for x in range(left, right)]


def _capture_grid(roost, tab, path) -> _Grid:
    """Mark the origin in a fresh colour, then capture until a frame shows
    the mark."""
    marker = (17, next(_MARKER_GREENS), 93)
    roost.tab_feed_pty_bytes(tab, marker_bytes(marker))
    metrics = roost.window_metrics()
    term_x, term_y = int(metrics["terminal_left"]), round(roost.terminal_top(metrics))
    latest: dict = {}

    def painted() -> bool:
        shot = _capture(roost, path)
        if shot is None:
            return False
        scale = max(1, round(shot[0] / metrics["window_width"]))
        latest["shot"], latest["scale"] = shot, scale
        return _pixel(shot, term_x * scale + 1, term_y * scale + 1) == marker

    Roost._wait(painted, 10.0, "a frame showing the origin marker")
    shot, scale = latest["shot"], latest["scale"]
    x0, y0 = term_x * scale, term_y * scale
    cell_w = _color_run(shot, x0 + 1, y0 + 1, 1, 0, marker) + 1
    cell_h = _color_run(shot, x0 + 1, y0 + 1, 0, 1, marker) + 1
    assert cell_w >= 4 * scale and cell_h >= 8 * scale, (cell_w, cell_h)
    return _Grid(shot, x0, y0, cell_w, cell_h)


def _scene(roost, project) -> tuple[int, tuple[int, int]]:
    """A quiet tab with its cursor hidden, and its terminal's cell px."""
    show_sidebar(roost)
    tab = quiet_tab(roost, project)
    roost.tab_feed_pty_bytes(tab, b"\x1b[?25l\x1b[2J")
    return tab, cell_px(roost.window_metrics())


def _listed(roost, tab, count: int) -> list[dict]:
    """Wait until the tab's terminal lists `count` placements."""
    images: list = []

    def listed() -> bool:
        images[:] = roost.tab_dump_resolved(tab).get("images", [])
        return len(images) == count

    Roost._wait(listed, 10.0, f"{count} image placement(s) listed")
    return images


def _only(pixels, color) -> list:
    """The pixels that aren't `color`."""
    return [px for px in pixels if px != color]


def test_an_image_fills_its_cells_and_no_others(roost, project, tmp_path):
    tab, (cw, ch) = _scene(roost, project)
    roost.tab_feed_pty_bytes(tab, place(51, 3, 5, solid(RED, 2 * cw, ch), 2 * cw, ch))
    _listed(roost, tab, 1)
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-position"))

    background = grid.corner(1, 0)
    assert _dist(background, RED) > 64, background
    assert not _only(grid.cells(5, 3, cols=2), RED), "the image fills its two cells"
    for col, row in [(4, 3), (7, 3), (5, 2), (6, 2), (5, 4), (6, 4)]:
        assert grid.corner(col, row) == background, (col, row)
        assert RED not in grid.cells(col, row), f"the image spilled into {(col, row)}"


@pytest.mark.skipif(
    os.environ.get("ICED_BACKEND") == "tiny-skia",
    reason="tiny-skia's raster path truncates an upscaled image's origin to whole "
    "source pixels (iced_tiny_skia raster.rs:67); plan 088 R6",
)
def test_a_stretched_pixel_fills_exactly_its_cell(roost, project, tmp_path):
    """A 1×1 image placed one cell large is scaled by the cell size, which
    the image's origin is generally not a multiple of; it still lands on
    the cell."""
    tab, _cell = _scene(roost, project)
    roost.tab_feed_pty_bytes(tab, place(58, 3, 5, solid(RED, 1, 1), 1, 1, "c=1,r=1"))
    (image,) = _listed(roost, tab, 1)
    assert (image["grid_cols"], image["grid_rows"]) == (1, 1), image
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-stretched"))

    background = grid.corner(1, 0)
    assert not _only(grid.cells(5, 3), RED), "the pixel fills its cell"
    for col, row in [(4, 3), (6, 3), (5, 2), (5, 4), (4, 2), (6, 4)]:
        assert grid.corner(col, row) == background, (col, row)
        assert RED not in grid.cells(col, row), f"the pixel spilled into {(col, row)}"


def test_an_image_above_the_text_hides_it_and_one_below_shows_it(roost, project, tmp_path):
    tab, (cw, ch) = _scene(roost, project)
    text = b"\x1b[38;2;255;255;255mXXXX\x1b[0m"
    roost.tab_feed_pty_bytes(
        tab,
        at(5, 0)
        + text
        + at(7, 0)
        + text
        + place(52, 5, 0, solid(GREEN, 4 * cw, ch), 4 * cw, ch)
        + place(53, 7, 0, solid(BLUE, 4 * cw, ch), 4 * cw, ch, "z=-1"),
    )
    layers = {image["image_id"]: image["layer"] for image in _listed(roost, tab, 2)}
    assert layers == {52: "above-text", 53: "below-text"}, layers
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-text"))

    covered = _only(grid.cells(0, 5, cols=4), GREEN)
    assert not covered, f"glyph pixels show through an above-text image: {covered[:4]}"
    under = grid.cells(0, 7, cols=4)
    assert grid.corner(0, 7) == BLUE and grid.corner(3, 7) == BLUE
    ink = max(under, key=lambda px: _dist(px, BLUE))
    assert _dist(ink, INK) <= INK_TOL, f"no glyph over the below-text image: {ink}"


def test_a_below_background_image_sits_under_explicit_cell_backgrounds(roost, project, tmp_path):
    tab, (cw, ch) = _scene(roost, project)
    r, g, b = CELL_BG
    roost.tab_feed_pty_bytes(
        tab,
        at(9, 0)
        + f"\x1b[48;2;{r};{g};{b}m  \x1b[0m".encode()
        + place(54, 9, 0, solid(YELLOW, 4 * cw, ch), 4 * cw, ch, f"z={BELOW_BACKGROUND_Z}"),
    )
    assert _listed(roost, tab, 1)[0]["layer"] == "below-background"
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-below-bg"))

    assert not _only(grid.cells(0, 9, cols=2), CELL_BG), "explicit backgrounds cover the image"
    assert not _only(grid.cells(2, 9, cols=2), YELLOW), "default-background cells show it"


def test_a_crop_shows_only_its_source_rect(roost, project, tmp_path):
    tab, (cw, ch) = _scene(roost, project)
    width = 4 * cw
    crop = f"x={2 * cw},y=0,w={2 * cw},h={ch}"
    roost.tab_feed_pty_bytes(tab, place(55, 11, 4, halves(ORANGE, CYAN, width, ch), width, ch, crop))
    (image,) = _listed(roost, tab, 1)
    assert (image["pixel_width"], image["grid_cols"]) == (2 * cw, 2), image
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-crop"))

    assert not _only(grid.cells(4, 11, cols=2), CYAN), "the source rect fills the placement"
    row = grid.cells(0, 11, cols=8)
    assert ORANGE not in row, "the cropped-away half drew"
    background = grid.corner(1, 0)
    assert grid.corner(2, 11) == background and grid.corner(3, 11) == background


def test_an_image_past_the_last_column_leaves_the_padding(roost, project, tmp_path):
    """The terminal is wider than its grid by less than a cell; a placement
    running off the grid's right edge stops there. The sidebar is widened
    by half a cell to make that padding, since a window resize isn't
    honoured by every headless compositor."""
    show_sidebar(roost)
    before = roost.window_metrics()
    cell_w = before["cell_width"]

    def padding(metrics) -> float:
        width = metrics["window_width"] - metrics["terminal_left"]
        return width - (width // cell_w) * cell_w

    try:
        if padding(before) < 3:
            roost.sidebar_set_width(before["sidebar_width"] + cell_w / 2)
            Roost._wait(
                lambda: padding(roost.window_metrics()) >= 3,
                10.0,
                "a terminal leaving padding right of the grid",
            )
        tab, (cw, ch) = _scene(roost, project)
        metrics = roost.window_metrics()
        cols = int((metrics["window_width"] - metrics["terminal_left"]) // cell_w)
        Roost._wait(lambda: roost.dump(tab)["cols"] == cols, 10.0, f"the tab at {cols} columns")
        roost.tab_feed_pty_bytes(tab, place(56, 2, cols - 1, solid(PURPLE, 3 * cw, ch), 3 * cw, ch))
        (image,) = _listed(roost, tab, 1)
        assert image["pixel_width"] == 3 * cw, image
        grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-grid-clip"))

        assert not _only(grid.cells(cols - 1, 2), PURPLE), "the image's first cell shows"
        width = grid.shot[0]
        grid_right = grid.x0 + cols * grid.cell_w
        assert grid_right < width, (grid_right, width)
        background = grid.corner(1, 0)
        top = grid.y0 + 2 * grid.cell_h
        pad = [
            _pixel(grid.shot, x, y)
            for y in range(top + 1, top + grid.cell_h - 1)
            for x in range(grid_right, width)
        ]
        assert PURPLE not in pad, "the image painted the padding past the grid"
        assert not _only(pad, background), set(pad)
    finally:
        roost.sidebar_set_width(before["sidebar_width"])


def test_a_freed_image_leaves_its_cells(roost, project, tmp_path):
    tab, (cw, ch) = _scene(roost, project)
    roost.tab_feed_pty_bytes(tab, place(57, 3, 5, solid(RED, 2 * cw, ch), 2 * cw, ch))
    _listed(roost, tab, 1)
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-before-free"))
    assert not _only(grid.cells(5, 3, cols=2), RED)

    roost.tab_feed_pty_bytes(tab, free(57))
    _listed(roost, tab, 0)
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-freed"))
    background = grid.corner(1, 0)
    assert not _only(grid.cells(5, 3, cols=2), background), "the freed image still shows"


def test_unicode_placeholder_cells_show_the_image_and_no_glyph(roost, project, tmp_path):
    """A `U=1` placement draws in its placeholder cells, each row its own
    slice of the image (green over purple) with no seam between them, and
    the U+10EEEE text those cells hold is never drawn: a font with a
    notdef glyph would box every cell (yazi on macOS). The cells'
    foreground is palette 196, a red no image pixel has, so any ink
    shows."""
    tab, (cw, ch) = _scene(roost, project)
    width, height = 4 * cw, 2 * ch
    roost.tab_feed_pty_bytes(
        tab,
        virtual(196, solid(GREEN, width, ch) + solid(PURPLE, width, ch), width, height, 4, 2)
        + placeholders(196, row=3, col=5, cols=4, rows=2),
    )
    _listed(roost, tab, 2)
    grid = _capture_grid(roost, tab, _artifact(tmp_path, "kitty-placeholders"))

    for row, color in ((3, GREEN), (4, PURPLE)):
        stray = _only(grid.cells(5, row, cols=4), color)
        assert not stray, f"row {row} isn't only its slice: {sorted(set(stray))[:4]}"
    seam = grid.y0 + 4 * grid.cell_h
    left, right = grid.x0 + 5 * grid.cell_w + 1, grid.x0 + 9 * grid.cell_w - 1
    for y, color in ((seam - 1, GREEN), (seam, PURPLE)):
        edge = [_pixel(grid.shot, x, y) for x in range(left, right)]
        assert not _only(edge, color), f"a seam between the rows' slices at y={y}"
    background = grid.corner(1, 0)
    for col, row in [(4, 3), (9, 3), (5, 2), (8, 5)]:
        assert grid.corner(col, row) == background, (col, row)
        spilled = {GREEN, PURPLE} & set(grid.cells(col, row))
        assert not spilled, f"the image spilled into {(col, row)}"
