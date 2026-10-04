"""Terminal cells to global screen points for the Mac real-input harness (plan 074 §D7).

Two sources, both in points (Retina never enters: AX and CGEvent share
top-left global points, and `app.window_metrics` is in logical points):

* the window's AX frame (``roost-input-mac window``): the whole window,
  titlebar included, top-left origin, possibly negative on a display left of or
  above the main one;
* ``app.window_metrics``: the content's size and the terminal grid inside it
  (``terminal_top``, ``terminal_left``, ``terminal_padding``, ``cell_width``,
  ``cell_height``).

A cell's centre is::

    frame.origin + (frame.height - window_height)          # the titlebar
                 + (terminal_left, terminal_top) + terminal_padding
                 + (col, row) * cell_size + cell_size / 2

The frame goes stale on any move, resize or full-screen change, so reacquire
both sources after one; `check` rejects a frame whose size no longer matches
the metrics.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

# How far an AX frame's width may differ from the content width before the pair
# is treated as stale: AX rounds to whole points, and a macOS window has no side
# borders.
_WIDTH_TOLERANCE = 1.0
# A titlebar (or a full-screen window's none) is well under this.
_MAX_TITLEBAR = 100.0


@dataclass(frozen=True)
class Frame:
    x: float
    y: float
    width: float
    height: float

    @classmethod
    def from_rect(cls, rect: dict) -> "Frame":
        """From the helper's `{x, y, width, height}`."""
        return cls(float(rect["x"]), float(rect["y"]), float(rect["width"]), float(rect["height"]))

    @classmethod
    def from_window(cls, window: dict) -> "Frame":
        """From `roost-input-mac window`'s result."""
        return cls.from_rect(window["frame"])

    def contains(self, x: float, y: float) -> bool:
        return self.x <= x < self.x + self.width and self.y <= y < self.y + self.height

    def within(self, other: "Frame") -> bool:
        """Whether all of this frame lies inside `other`."""
        return (
            other.x <= self.x
            and other.y <= self.y
            and self.x + self.width <= other.x + other.width
            and self.y + self.height <= other.y + other.height
        )


@dataclass(frozen=True)
class Metrics:
    window_width: float
    window_height: float
    terminal_top: float
    terminal_left: float
    terminal_padding: float
    cell_width: float
    cell_height: float

    @classmethod
    def from_window_metrics(cls, metrics: dict) -> "Metrics":
        """From `app.window_metrics`. The cell geometry is required: a UI that
        does not report it cannot be driven by real input, and guessing it
        would let the harness and the product drift together."""
        values = {}
        for name in (
            "window_width",
            "window_height",
            "terminal_top",
            "terminal_left",
            "terminal_padding",
            "cell_width",
            "cell_height",
        ):
            value = metrics.get(name)
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
                raise AssertionError(f"app.window_metrics has no usable {name}: {value!r}")
            values[name] = float(value)
        for name in ("window_width", "window_height", "cell_width", "cell_height"):
            if values[name] <= 0:
                raise AssertionError(f"app.window_metrics {name} must be positive: {values[name]}")
        for name in ("terminal_top", "terminal_left", "terminal_padding"):
            if values[name] < 0:
                raise AssertionError(f"app.window_metrics {name} must not be negative: {values[name]}")
        return cls(**values)


def titlebar(frame: Frame, metrics: Metrics) -> float:
    return frame.height - metrics.window_height


def check(frame: Frame, metrics: Metrics) -> None:
    """Refuse a frame and metrics that cannot describe the same window."""
    bar = titlebar(frame, metrics)
    if not -_WIDTH_TOLERANCE <= bar <= _MAX_TITLEBAR:
        raise AssertionError(
            f"AX frame height {frame.height} and window_height {metrics.window_height} leave a "
            f"{bar}-point titlebar: reacquire the frame after a move, resize or full-screen change"
        )
    if abs(frame.width - metrics.window_width) > _WIDTH_TOLERANCE:
        raise AssertionError(
            f"AX frame width {frame.width} is not window_width {metrics.window_width}: "
            "reacquire the frame after a move, resize or full-screen change"
        )


def window_point(frame: Frame, metrics: Metrics, x: float, y: float) -> tuple[float, float]:
    """A point in the window's content (logical points, top-left origin) as a
    global point."""
    check(frame, metrics)
    return frame.x + x, frame.y + max(titlebar(frame, metrics), 0.0) + y


def cell_origin(frame: Frame, metrics: Metrics, col: int, row: int) -> tuple[float, float]:
    """The global top-left of cell (`col`, `row`); it must lie inside the frame."""
    if col < 0 or row < 0:
        raise ValueError(f"cell ({col}, {row}) is negative")
    x, y = window_point(
        frame,
        metrics,
        metrics.terminal_left + metrics.terminal_padding + col * metrics.cell_width,
        metrics.terminal_top + metrics.terminal_padding + row * metrics.cell_height,
    )
    if not frame.contains(x, y):
        raise AssertionError(
            f"cell ({col}, {row}) starts at ({x}, {y}), outside the window frame {frame}"
        )
    return x, y


def cell_center(frame: Frame, metrics: Metrics, col: int, row: int) -> tuple[float, float]:
    """The global centre of cell (`col`, `row`): where a click on it goes."""
    x, y = cell_origin(frame, metrics, col, row)
    center = x + metrics.cell_width / 2, y + metrics.cell_height / 2
    if not frame.contains(*center):
        raise AssertionError(f"cell ({col}, {row})'s centre {center} is outside the window frame {frame}")
    return center


def displays(preflight: dict) -> list[Frame]:
    """The active displays' bounds from a `roost-input-mac preflight`
    report, the main display first."""
    found = preflight.get("displays")
    if not found:
        raise AssertionError(f"the preflight report names no displays: {found!r}")
    ordered = sorted(found, key=lambda display: not display.get("main"))
    return [Frame.from_rect(display["bounds"]) for display in ordered]


def _inset(display: dict, key: str) -> float:
    value = display.get(key)
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return 0.0
    return max(float(value), 0.0)


def full_screen_frames(preflight: dict) -> list[Frame]:
    """The frames a full-screen window may settle at, per display (plan 075 §D3).

    A display's bounds, and on a notched one (`safe_area_top` > 0) also its
    bounds inset at the top by the menu bar (`menu_bar_inset`, what the window
    measured on a notched MacBook sits below) or by the camera housing
    (`safe_area_top`). Exact candidates, not a tolerance: a missing or invalid
    inset is 0, which covers an older helper, an unnotched Mac and CI."""
    found = preflight.get("displays")
    if not found:
        raise AssertionError(f"the preflight report names no displays: {found!r}")
    frames: list[Frame] = []
    for display in sorted(found, key=lambda display: not display.get("main")):
        bounds = Frame.from_rect(display["bounds"])
        frames.append(bounds)
        safe = _inset(display, "safe_area_top")
        if safe > 0:
            for inset in (_inset(display, "menu_bar_inset"), safe):
                if 0 < inset < bounds.height:
                    frames.append(Frame(bounds.x, bounds.y + inset, bounds.width, bounds.height - inset))
    return frames


def on_one_display(frame: Frame, screens: list[Frame]) -> bool:
    """Whether the whole window lies on a single display: a frame split
    across two, or partly off every one, is not."""
    return any(frame.within(screen) for screen in screens)
