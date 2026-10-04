"""Cells to global points for the Mac real-input harness (`tools/input/mac/coords.py`, plan 074 §D7).

Pure arithmetic, so it is pinned here on any OS: the conversion, the check
that a derived cell lies inside the window's AX frame, and a window on a
display left of and above the main one (negative global coordinates).
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "input" / "mac"))

import coords  # noqa: E402

# An iced window_metrics answer: 1100x720 content, the 220-point sidebar, the
# 32-point tab band, an edge-pinned grid of 8x16 cells.
METRICS = {
    "window_width": 1100.0,
    "window_height": 720.0,
    "sidebar_width": 220.0,
    "sidebar_collapsed": False,
    "terminal_top": 32.0,
    "terminal_left": 220.0,
    "terminal_padding": 0.0,
    "cell_width": 8.0,
    "cell_height": 16.0,
}
# Its AX frame: a 28-point titlebar above the content.
WINDOW = {"frame": {"x": 100.0, "y": 50.0, "width": 1100.0, "height": 748.0}}


def metrics(**overrides) -> coords.Metrics:
    return coords.Metrics.from_window_metrics({**METRICS, **overrides})


class ConversionTests(unittest.TestCase):
    def test_a_cell_centre_adds_titlebar_band_sidebar_and_half_a_cell(self) -> None:
        frame = coords.Frame.from_window(WINDOW)
        self.assertEqual(coords.titlebar(frame, metrics()), 28.0)
        # x: 100 + 220 + 0*8 + 4; y: 50 + 28 + 32 + 0*16 + 8
        self.assertEqual(coords.cell_center(frame, metrics(), 0, 0), (324.0, 118.0))
        # col 10, row 3: x 100 + 220 + 80 + 4, y 50 + 28 + 32 + 48 + 8
        self.assertEqual(coords.cell_center(frame, metrics(), 10, 3), (404.0, 166.0))

    def test_padding_insets_the_grid_on_both_axes(self) -> None:
        frame = coords.Frame.from_window(WINDOW)
        self.assertEqual(
            coords.cell_origin(frame, metrics(terminal_padding=6.0), 0, 0), (326.0, 116.0)
        )

    def test_a_collapsed_sidebar_starts_the_grid_at_the_window_edge(self) -> None:
        frame = coords.Frame.from_window(WINDOW)
        self.assertEqual(
            coords.cell_origin(frame, metrics(terminal_left=0.0), 0, 0), (100.0, 110.0)
        )

    def test_a_window_point_is_content_relative(self) -> None:
        frame = coords.Frame.from_window(WINDOW)
        self.assertEqual(coords.window_point(frame, metrics(), 40.0, 16.0), (140.0, 94.0))

    def test_a_full_screen_window_has_no_titlebar(self) -> None:
        frame = coords.Frame(0.0, 0.0, 1100.0, 720.0)
        self.assertEqual(coords.cell_origin(frame, metrics(), 0, 0), (220.0, 32.0))


class NegativeOriginTests(unittest.TestCase):
    def test_a_display_left_of_and_above_the_main_one_passes_through(self) -> None:
        frame = coords.Frame(-1440.0, -300.0, 1100.0, 748.0)
        self.assertEqual(coords.cell_center(frame, metrics(), 0, 0), (-1216.0, -232.0))
        self.assertEqual(coords.cell_center(frame, metrics(), 2, 1), (-1200.0, -216.0))

    def test_the_inside_check_holds_on_a_negative_frame(self) -> None:
        frame = coords.Frame(-1440.0, -300.0, 1100.0, 748.0)
        last_col = int((1100 - 220) / 8) - 1
        last_row = int((720 - 32) / 16) - 1
        x, y = coords.cell_center(frame, metrics(), last_col, last_row)
        self.assertTrue(frame.contains(x, y))
        with self.assertRaisesRegex(AssertionError, "outside the window frame"):
            coords.cell_origin(frame, metrics(), last_col + 1, 0)


class SanityTests(unittest.TestCase):
    def test_a_cell_past_the_frame_is_refused(self) -> None:
        frame = coords.Frame.from_window(WINDOW)
        with self.assertRaisesRegex(AssertionError, "outside the window frame"):
            coords.cell_origin(frame, metrics(), 0, 200)
        with self.assertRaisesRegex(AssertionError, "outside the window frame"):
            coords.cell_origin(frame, metrics(), 110, 0)

    def test_a_cell_whose_centre_falls_off_the_last_edge_is_refused(self) -> None:
        # The origin is the frame's last point (219 + 440 * 2 = 1099); its
        # centre (1100) is not in it.
        frame = coords.Frame(0.0, 0.0, 1100.0, 748.0)
        with self.assertRaisesRegex(AssertionError, "centre"):
            coords.cell_center(frame, metrics(terminal_left=219.0, cell_width=2.0), 440, 0)

    def test_a_stale_frame_is_refused(self) -> None:
        moved_and_resized = coords.Frame(100.0, 50.0, 900.0, 748.0)
        with self.assertRaisesRegex(AssertionError, "reacquire"):
            coords.cell_center(moved_and_resized, metrics(), 0, 0)
        shorter_than_content = coords.Frame(100.0, 50.0, 1100.0, 600.0)
        with self.assertRaisesRegex(AssertionError, "reacquire"):
            coords.cell_center(shorter_than_content, metrics(), 0, 0)

    def test_negative_cells_are_a_caller_error(self) -> None:
        frame = coords.Frame.from_window(WINDOW)
        with self.assertRaises(ValueError):
            coords.cell_origin(frame, metrics(), -1, 0)

    def test_missing_or_unusable_geometry_is_refused(self) -> None:
        for name, value in (
            ("cell_width", None),
            ("cell_height", 0.0),
            ("terminal_left", True),
            ("terminal_top", float("nan")),
            ("terminal_padding", -1.0),
        ):
            with self.subTest(name=name, value=value):
                with self.assertRaisesRegex(AssertionError, name):
                    coords.Metrics.from_window_metrics({**METRICS, name: value})
        absent = {key: value for key, value in METRICS.items() if key != "cell_height"}
        with self.assertRaisesRegex(AssertionError, "cell_height"):
            coords.Metrics.from_window_metrics(absent)


class DisplayTests(unittest.TestCase):
    """A window "on a screen" for the window-frame scenario: all of it on one
    active display, which may sit at negative global coordinates."""

    MAIN = coords.Frame(0.0, 0.0, 1920.0, 1080.0)
    LEFT = coords.Frame(-1440.0, -300.0, 1440.0, 900.0)

    def test_a_window_inside_one_display_is_on_it(self) -> None:
        self.assertTrue(coords.on_one_display(coords.Frame(100.0, 50.0, 900.0, 600.0), [self.MAIN]))
        self.assertTrue(
            coords.on_one_display(coords.Frame(-1300.0, -200.0, 900.0, 600.0), [self.MAIN, self.LEFT])
        )
        self.assertTrue(coords.on_one_display(self.MAIN, [self.MAIN]), "edges are inclusive")

    def test_a_window_off_every_display_or_split_across_two_is_not(self) -> None:
        for frame in (
            coords.Frame(5000.0, 100.0, 900.0, 600.0),
            coords.Frame(1500.0, 100.0, 900.0, 600.0),
            coords.Frame(-200.0, 100.0, 900.0, 600.0),
            coords.Frame(100.0, 1000.0, 900.0, 600.0),
        ):
            with self.subTest(frame=frame):
                self.assertFalse(coords.on_one_display(frame, [self.MAIN, self.LEFT]))

    def test_displays_come_from_a_preflight_report(self) -> None:
        report = {
            "displays": [
                {"id": 2, "main": False, "bounds": {"x": -1440, "y": -300, "width": 1440, "height": 900}},
                {"id": 1, "main": True, "bounds": {"x": 0, "y": 0, "width": 1920, "height": 1080}},
            ]
        }
        self.assertEqual(coords.displays(report), [self.MAIN, self.LEFT], "the main display first")
        for empty in ({}, {"displays": None}, {"displays": []}):
            with self.subTest(report=empty):
                with self.assertRaisesRegex(AssertionError, "no displays"):
                    coords.displays(empty)


if __name__ == "__main__":
    unittest.main()
