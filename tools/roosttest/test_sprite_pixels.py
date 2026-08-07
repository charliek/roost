"""Iced sprite pixel guards — plan 020 C4 (roadmap slice E5).

The iced adapter draws U+2500–U+259F geometrically (shared
`roost_ui_model::sprite` geometry, quads with integer-snapped edges in
`crates/roost-iced/src/terminal_widget.rs::draw_sprite`) because font
glyphs neither tile seamlessly across adjacent cells nor keep hard
edges inside a cell. Two things pin that path end-to-end, and both are
invisible to `tab.dump`:

* **Counter semantics (D4):** a sprite draw *replaces* a glyph draw,
  so `app.render_stats` must report `fill_text_calls > 0` for a
  redraw of a sprite-only scene. Asserted BEFORE any screenshot —
  `app.screenshot` re-renders the widget and would inflate the
  counters it is meant to isolate.
* **Seams and internal edges (D6):** full-block runs must show no
  background seam column at cell boundaries; partial blocks (▀ ▄ ▌)
  must step fg→bg in a hard edge at the half-cell boundary with NO
  intermediate-color band (the AA artifact integer snapping exists to
  kill); a █ row must tile seamlessly into a ▀ row below it; and box
  lines (─ ═) must run unbroken across 40 cells.

Colors are read back from `tab.dump_resolved` (the production
resolver), so the pixel classification tracks the live theme instead
of hardcoding it. wgpu writes exact fg for opaque quads (C3 evidence);
`COLOR_TOL` leaves room for compositing rounding only — a real AA band
sits far outside it and fails the no-intermediate assertion.

Iced-target-only (walking-skeleton precedent): the GTK sprite path has
its own byte-exact Cairo suite + golden-hash fixture, and the Mac path
is covered by `Sprite.swift`'s reference tests.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from client import Roost
from test_sidebar_collapse_persistence import _toggle_to_visible
from test_sidebar_pixels import _capture
from util import BARE_SHELL_ARGV, wait_tab_quiet

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# crates/roost-iced/src/terminal_widget.rs — edge-pinned grid, no gutter.
TERMINAL_PADDING = 0
# Same distinctive explicit-bg marker the walking skeleton seeds at cell
# (0,0): it locates the grid origin and measures one cell in the shot.
MARKER = (17, 201, 93)
RUN_CELLS = 40
# fg/bg classification tolerance. Quads render the exact resolved color;
# 2 absorbs compositing rounding without reaching an AA mid-tone.
COLOR_TOL = 2

# Grid rows (0-based) the scene occupies; row 0 is the marker, row 1 is
# deliberately blank so the marker's vertical color run measures cell_h.
BLOCK_ROW = 2  # █ ×40
UPPER_ROW = 3  # ▀ ×40 — directly below █ so the boundary must tile
LOWER_ROW = 4  # ▄ ×40
LEFT_ROW = 5  # ▌ ×40
LIGHT_ROW = 6  # ─ ×40
DOUBLE_ROW = 7  # ═ ×40

SCENE_ROWS: dict[int, str] = {
    BLOCK_ROW: "█" * RUN_CELLS,
    UPPER_ROW: "▀" * RUN_CELLS,
    LOWER_ROW: "▄" * RUN_CELLS,
    LEFT_ROW: "▌" * RUN_CELLS,
    LIGHT_ROW: "─" * RUN_CELLS,
    DOUBLE_ROW: "═" * RUN_CELLS,
}


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("sprite pixel e2e pins the iced sprite adapter (plan 020 C4)")


def _scene_bytes() -> bytes:
    """Hide the cursor (its overlay quad would corrupt sampled cells),
    clear, then place the marker + six 40-cell sprite rows at fixed
    grid rows via absolute cursor addressing."""
    parts = ["\x1b[?25l\x1b[2J\x1b[H\x1b[48;2;17;201;93m \x1b[0m"]
    for row, text in sorted(SCENE_ROWS.items()):
        parts.append(f"\x1b[{row + 1};1H{text}")
    return "".join(parts).encode()


def _scene_settled(roost, tab) -> None:
    """Re-feed until the viewport holds the scene and nothing else.

    The bare shell can repaint its prompt after `wait_tab_quiet`
    (walking-skeleton precedent), so each poll reseeds the whole scene
    before checking. The no-alphanumerics guard is what makes the
    counter assertion meaningful: it proves the visible content is
    sprite-only, so any `fill_text_calls` must come from sprite cells.
    """

    def settled() -> bool:
        roost.tab_feed_pty_bytes(tab, _scene_bytes())
        rows_text = roost.dump(tab)["rows_text"]
        return all(
            row < len(rows_text) and rows_text[row].startswith(text)
            for row, text in SCENE_ROWS.items()
        ) and not any(ch.isalnum() for line in rows_text for ch in line)

    Roost._wait(settled, 10.0, "sprite scene settles as the only tab content")


def _scene_tab(roost, project) -> int:
    tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_quiet(roost, tab)
    if roost.dump(tab)["cols"] <= RUN_CELLS:
        roost.window_resize(1100.0, 700.0)
        Roost._wait(
            lambda: roost.dump(tab)["cols"] > RUN_CELLS,
            timeout=5.0,
            what=f"terminal re-quantizes to more than {RUN_CELLS} columns",
        )
        wait_tab_quiet(roost, tab)
    _scene_settled(roost, tab)
    return tab


def _pixel(shot, x: int, y: int) -> tuple[int, int, int]:
    width, _height, bpp, pixels = shot
    offset = (y * width + x) * bpp
    return tuple(pixels[offset : offset + 3])


def _near(px, color) -> bool:
    return all(abs(a - b) <= COLOR_TOL for a, b in zip(px, color, strict=True))


def _classify(px, fg, bg) -> str | None:
    if _near(px, fg):
        return "fg"
    if _near(px, bg):
        return "bg"
    return None


def _color_run(shot, x: int, y: int, dx: int, dy: int, color) -> int:
    width, height, _bpp, _pixels = shot
    length = 0
    while 0 <= x < width and 0 <= y < height and _pixel(shot, x, y) == color:
        length += 1
        x += dx
        y += dy
    return length


def _hex_rgb(value: str) -> tuple[int, int, int]:
    return tuple(bytes.fromhex(value.removeprefix("#")))


def _hard_split(classes: list[str | None], what: str) -> int:
    """Assert `classes` is fg…fg bg…bg (or bg…bg fg…fg) with exactly one
    transition and no unclassifiable (intermediate-color) entries.
    Returns the index of the transition."""
    assert None not in classes, f"{what}: intermediate colors at {classes}"
    first = classes[0]
    assert first in ("fg", "bg"), what
    edge = len(classes)
    for i, c in enumerate(classes):
        if c != first:
            edge = i
            break
    tail = classes[edge:]
    assert all(c == tail[0] for c in tail) and 0 < edge < len(classes), (
        f"{what}: expected one hard transition, got {classes}"
    )
    return edge


@pytest.mark.skipif(
    not TEST_MODE,
    reason="sprite scene injection requires ROOST_TEST_MODE=1 in the UI's launch env",
)
class TestSpritePixels:
    def test_sprite_cells_count_as_glyph_draws(self, roost, project):
        """D4: with a sprite-only scene on screen, a redraw registers
        `fill_text_calls > 0` — sprite cells count as glyph draws.
        Runs before any screenshot in this module (module order):
        `app.screenshot` re-renders and would inflate the counters."""
        tab = _scene_tab(roost, project)

        roost.call("app.render_stats", {"reset": True})
        # Re-home the cursor: a no-op for the grid content, but it rides
        # the refresh + paint path and forces a fresh widget draw.
        roost.tab_feed_pty_bytes(tab, b"\x1b[H")

        stats: dict = {}

        def drew() -> bool:
            stats["now"] = roost.call("app.render_stats", {})
            return int(stats["now"]["draw_calls"]) > 0

        Roost._wait(drew, 10.0, "a redraw of the sprite scene lands in render stats")
        assert int(stats["now"]["fill_text_calls"]) > 0, (
            f"sprite-only scene drew no glyphs: {stats['now']}"
        )

    def test_sprite_seams_and_internal_edges(self, roost, project, tmp_path):
        _toggle_to_visible(roost)
        tab = _scene_tab(roost, project)

        metrics = roost.window_metrics()
        term_x = int(metrics["sidebar_width"]) + TERMINAL_PADDING
        term_y = round(roost.terminal_top(metrics)) + TERMINAL_PADDING

        resolved = roost.tab_dump_resolved(tab)
        block = next(c for c in resolved["cells"] if c["text"] == "█")
        assert not block["has_explicit_bg"]
        fg = _hex_rgb(block["fg"])
        bg = _hex_rgb(block["bg"])

        artifact_dir = Path(os.environ.get("ROOST_E2E_ARTIFACT_DIR", tmp_path))
        artifact_dir.mkdir(parents=True, exist_ok=True)
        shot_path = artifact_dir / "sprite-scene.png"
        latest: dict = {}

        def painted() -> bool:
            # Reseed per attempt: a late prompt repaint between the
            # settle wait and the capture would otherwise poison the
            # shot (walking-skeleton precedent). `_capture` polls
            # through the transient empty-snapshot error.
            roost.tab_feed_pty_bytes(tab, _scene_bytes())
            shot = _capture(roost, shot_path)
            if shot is None:
                return False
            # scale=1 keeps logical == physical on iced; detect anyway
            # so a retina-doubled capture maps geometry instead of
            # misreading pixels (walking-skeleton scale contract).
            scale = max(1, round(shot[0] / metrics["window_width"]))
            latest["shot"], latest["scale"] = shot, scale
            return _pixel(shot, term_x * scale + 1, term_y * scale + 1) == MARKER

        Roost._wait(painted, 10.0, "sprite scene painted with the origin marker")
        shot, scale = latest["shot"], latest["scale"]
        x0, y0 = term_x * scale, term_y * scale

        # Measure from the interior pixel `painted` already validated
        # ((x0+1, y0+1)) and sum both directions (minus the shared
        # start pixel): if edge snapping ever shifts the marker's
        # leading edge off (x0, y0) by one, a run started there would
        # read 0 and hard-fail instead of retrying.
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
        run_w = RUN_CELLS * cell_w

        def row_top(row: int) -> int:
            return y0 + row * cell_h

        def scanline(y: int) -> list[str | None]:
            return [_classify(_pixel(shot, x, y), fg, bg) for x in range(x0, x0 + run_w)]

        def assert_scanline(y: int, expect: str, what: str) -> None:
            classes = scanline(y)
            assert all(c == expect for c in classes), (
                f"{what}: y={y} expected all {expect}, got "
                f"{[(x0 + i, c) for i, c in enumerate(classes) if c != expect][:8]}"
            )

        # (b) Full-block seam scan: the █ row's vertical middle is one
        # contiguous fg run across all 40 cells — no bg seam column.
        assert_scanline(row_top(BLOCK_ROW) + cell_h // 2, "fg", "█ mid-row seam scan")

        # (d) Block-to-halfblock vertical tiling: █ bottom edge and the
        # ▀ top half directly below it abut with no bg line between.
        assert_scanline(row_top(UPPER_ROW) - 1, "fg", "█ bottom edge")
        assert_scanline(row_top(UPPER_ROW), "fg", "▀ top edge")

        def assert_half_row(row: int, order: tuple[str, str], what: str) -> None:
            """(c) horizontal partial blocks: every scanline in the row
            band is uniformly fg or bg (so no intermediate-color band
            anywhere in the band, including half_cell±1), and the
            per-scanline classes step once, hard, at ~cell_h/2."""
            top = row_top(row)
            per_y: list[str | None] = []
            for y in range(top, top + cell_h):
                classes = scanline(y)
                assert None not in classes, f"{what}: intermediate colors at y={y}"
                assert all(c == classes[0] for c in classes), (
                    f"{what}: mixed scanline at y={y}"
                )
                per_y.append(classes[0])
            assert per_y[0] == order[0] and per_y[-1] == order[1], (what, per_y)
            edge = _hard_split(per_y, what)
            # aligned_block rounds h/2 and draw_sprite snaps absolute
            # edges, so the step sits within a snap of the half cell.
            assert abs(edge - cell_h / 2) <= 1.5 * scale, (what, edge, cell_h)

        assert_half_row(UPPER_ROW, ("fg", "bg"), "▀ internal edge")
        assert_half_row(LOWER_ROW, ("bg", "fg"), "▄ internal edge")

        # (c) ▌ vertical internal edge, per cell: columns are uniform
        # over the full cell height, fg left half then bg right half,
        # one hard step at ~cell_w/2. No intermediate column anywhere.
        top = row_top(LEFT_ROW)
        for cell in range(RUN_CELLS):
            cx0 = x0 + cell * cell_w
            per_x: list[str | None] = []
            for x in range(cx0, cx0 + cell_w):
                col = [_classify(_pixel(shot, x, y), fg, bg) for y in range(top, top + cell_h)]
                assert None not in col, f"▌ cell {cell}: intermediate colors at x={x}"
                assert all(c == col[0] for c in col), f"▌ cell {cell}: mixed column at x={x}"
                per_x.append(col[0])
            assert per_x[0] == "fg" and per_x[-1] == "bg", (cell, per_x)
            edge = _hard_split(per_x, f"▌ cell {cell}")
            assert abs(edge - cell_w / 2) <= 1.5 * scale, (cell, edge, cell_w)

        def assert_line_row(row: int, stroke_bands: int, what: str) -> None:
            """Box lines tile seamlessly: every scanline in the row band
            is uniformly fg (a stroke, unbroken across 40 cells) or
            uniformly bg, with exactly `stroke_bands` contiguous fg
            bands (─ one stroke, ═ two strokes with a gap)."""
            top = row_top(row)
            per_y = []
            for y in range(top, top + cell_h):
                classes = scanline(y)
                assert None not in classes, f"{what}: intermediate colors at y={y}"
                assert all(c == classes[0] for c in classes), (
                    f"{what}: broken stroke at y={y}"
                )
                per_y.append(classes[0])
            bands = sum(
                1 for i, c in enumerate(per_y) if c == "fg" and (i == 0 or per_y[i - 1] != "fg")
            )
            assert bands == stroke_bands, (what, per_y)

        assert_line_row(LIGHT_ROW, 1, "─ row")
        assert_line_row(DOUBLE_ROW, 2, "═ row")
