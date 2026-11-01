"""`tab.dump_resolved.images`: the Kitty placements a tab's terminal would
draw (plan 088 D2).

Each case feeds a Kitty graphics command through `tab.feed_pty_bytes` (the
real PTY-output path into libghostty) and reads the placement list back
off `tab.dump_resolved`, which an in-process tab answers from the list its
last snapshot refresh stored. The session half (a host tab's list equals
its session's, live and after a snapshot attach) is the host-client lane's
(`test_host_client.py`).

Iced only: the frozen Swift app neither stores nor lists images.

Condition waits only.
"""

from __future__ import annotations

import base64
import math
import os
import time

import pytest
from client import scaled_timeout
from util import wait_tab_attached

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="Kitty image tests require ROOST_TEST_MODE=1 in the UI's launch env",
)

QUIET_ARGV = ["/bin/sh", "-c", "exec sleep 300"]

#: A 2x2 RGBA image, every pixel different.
RGBA_2X2 = bytes(
    [255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 10, 20, 30, 40]
)


@pytest.fixture(autouse=True)
def iced_only(target):
    if target != "iced":
        pytest.skip("only roost-iced stores and lists Kitty images (plan 088)")


def place(image_id: int, row: int, col: int) -> bytes:
    """Move to the 0-based `row`/`col`, then transmit `RGBA_2X2` as
    `image_id` and place it there two cells wide and one tall."""
    payload = base64.b64encode(RGBA_2X2)
    return (
        f"\x1b[{row + 1};{col + 1}H".encode()
        + f"\x1b_Ga=T,t=d,f=32,s=2,v=2,c=2,r=1,i={image_id},q=2;".encode()
        + payload
        + b"\x1b\\"
    )


def free(image_id: int) -> bytes:
    """Delete `image_id`'s placements and the image itself (`d=I`)."""
    return f"\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\".encode()


def cell_px(metrics: dict) -> tuple[int, int]:
    """The terminal cell size a window gives its terminals and advertises
    as a session owner: its cell metrics in device px, rounded half away
    from zero as Rust's `f32::round` does."""
    scale = metrics.get("scale_factor", 1.0)
    return (
        max(1, math.floor(metrics["cell_width"] * scale + 0.5)),
        max(1, math.floor(metrics["cell_height"] * scale + 0.5)),
    )


def placed(image_id: int, row: int, col: int, cell: tuple[int, int]) -> dict:
    """The entry `place(image_id, row, col)` lists on a terminal whose
    cells are `cell` px."""
    return {
        "image_id": image_id,
        "placement_id": 0,
        "layer": "above-text",
        "z": 0,
        "col": col,
        "row": row,
        "grid_cols": 2,
        "grid_rows": 1,
        "offset_x": 0,
        "offset_y": 0,
        "pixel_width": 2 * cell[0],
        "pixel_height": cell[1],
        "source": {"x": 0, "y": 0, "width": 2, "height": 2},
        "image_width": 2,
        "image_height": 2,
    }


def images_of(dumped: dict) -> list[dict]:
    return dumped.get("images", [])


def settled_images(read, want: list[dict], timeout: float = 10.0) -> None:
    """Poll `read()` until it lists `want`, then assert it does — so a list
    that never arrives fails on the assertion, with both lists shown."""
    deadline = time.monotonic() + scaled_timeout(timeout)
    got = read()
    while got != want and time.monotonic() < deadline:
        time.sleep(0.05)
        got = read()
    assert got == want


def quiet_tab(roost, project) -> int:
    tab = roost.open_tab(project, cwd="/tmp", argv=QUIET_ARGV)
    wait_tab_attached(roost, tab)
    return tab


def test_a_placed_image_is_listed_and_freeing_it_empties_the_list(roost, project):
    tab = quiet_tab(roost, project)
    cell = cell_px(roost.window_metrics())
    read = lambda: images_of(roost.tab_dump_resolved(tab))  # noqa: E731

    roost.tab_feed_pty_bytes(tab, place(41, row=3, col=5))
    settled_images(read, [placed(41, 3, 5, cell)])

    roost.tab_feed_pty_bytes(tab, free(41))
    settled_images(read, [])
    assert "images" not in roost.tab_dump_resolved(tab), "an empty list is omitted"


def test_a_hidden_tab_keeps_its_list(roost, project):
    """The list is stored on the tab and rebuilt only when its placements
    change, so output that reaches the tab while it is hidden, and the
    switch back, leave it as it was."""
    tab = quiet_tab(roost, project)
    cell = cell_px(roost.window_metrics())
    read = lambda: images_of(roost.tab_dump_resolved(tab))  # noqa: E731
    want = [placed(42, 2, 1, cell)]
    roost.tab_feed_pty_bytes(tab, place(42, row=2, col=1))
    settled_images(read, want)

    other = quiet_tab(roost, project)
    roost.focus(other)
    assert roost.app_selected_tab_id() == other
    roost.tab_feed_pty_bytes(tab, b"\x1b[10;1Hwhile-hidden")
    roost.wait_text(tab, "while-hidden")
    assert read() == want

    roost.focus(tab)
    assert roost.app_selected_tab_id() == tab
    # `tab.dump` refreshes the tab's snapshot before it reads it.
    roost.dump(tab)
    assert read() == want
