"""`tab.dump_resolved.images`: the Kitty placements a tab's terminal would
draw (plan 088 D2).

Each case feeds a Kitty graphics command through `tab.feed_pty_bytes` (the
real PTY-output path into libghostty) and reads the placement list back
off `tab.dump_resolved`, which an in-process tab answers from the list its
last snapshot refresh stored. The session half (a host tab's list equals
its session's, live and after a snapshot attach) is the host-client lane's
(`test_host_client.py`).

The PTY's winsize carries the same cell size in pixels (plan 088 D4), which
is what `kitten icat` sizes an image from.

Iced only: the frozen Swift app neither stores nor lists images.

Condition waits only.
"""

from __future__ import annotations

import base64
import math
import os
import re
import time

import pytest
from client import scaled_timeout
from util import wait_shell_ready, wait_tab_attached

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
    scale = metrics["scale_factor"]
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


#: Kitty's row and column diacritics: the cell's index is the
#: diacritic's position in this list (`rowcolumn-diacritics.txt`).
DIACRITICS = "\u0305\u030d\u030e\u0310"
PLACEHOLDER = "\U0010eeee"


def virtual(image_id: int, rgba: bytes, width: int, height: int, cols: int, rows: int) -> bytes:
    """Transmit `rgba` as `image_id` with a `U=1` virtual placement
    `cols` × `rows` cells large, chunked as kitty requires. Nothing is
    drawn until placeholder cells name it."""
    data = base64.b64encode(rgba)
    chunks = [data[i : i + 4096] for i in range(0, len(data), 4096)] or [b""]
    keys = f"a=T,U=1,t=d,f=32,q=2,i={image_id},s={width},v={height},c={cols},r={rows}"
    out = b""
    for index, chunk in enumerate(chunks):
        more = int(index < len(chunks) - 1)
        head = f"{keys},m={more}" if index == 0 else f"m={more}"
        out += f"\x1b_G{head};".encode() + chunk + b"\x1b\\"
    return out


def placeholders(image_id: int, row: int, col: int, cols: int, rows: int) -> bytes:
    """Placeholder cells showing all of virtual placement `image_id`
    (< 256, named by the 256-colour foreground) from the 0-based
    `row`/`col`. Each row's first cell carries its row and column
    diacritics; the rest continue the run."""
    out = b""
    for r in range(rows):
        cells = PLACEHOLDER + DIACRITICS[r] + DIACRITICS[0] + PLACEHOLDER * (cols - 1)
        out += f"\x1b[{row + r + 1};{col + 1}H\x1b[38;5;{image_id}m".encode()
        out += cells.encode() + b"\x1b[39m"
    return out


def placeholder_runs(image_id: int, row: int, col: int, cols: int, rows: int, cell) -> list[dict]:
    """What `placeholders` lists for an image exactly `cols` × `rows`
    cells of `cell` px: one placement per row, each its own slice of
    the image, under the text."""
    width, height = cols * cell[0], rows * cell[1]
    return [
        {
            "image_id": image_id,
            "placement_id": 0,
            "layer": "below-text",
            "z": -1,
            "col": col,
            "row": row + r,
            "grid_cols": cols,
            "grid_rows": 1,
            "offset_x": 0,
            "offset_y": 0,
            "pixel_width": width,
            "pixel_height": cell[1],
            "source": {"x": 0, "y": r * cell[1], "width": width, "height": cell[1]},
            "image_width": width,
            "image_height": height,
        }
        for r in range(rows)
    ]


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


def test_unicode_placeholders_list_one_placement_per_row(roost, project):
    """A `U=1` virtual placement draws where its placeholder cells are,
    as yazi and tmux use it: libghostty resolves each row of cells into
    its own placement showing that row's slice of the image."""
    tab = quiet_tab(roost, project)
    cell = cell_px(roost.window_metrics())
    read = lambda: images_of(roost.tab_dump_resolved(tab))  # noqa: E731
    width, height = 3 * cell[0], 2 * cell[1]

    roost.tab_feed_pty_bytes(tab, virtual(61, bytes(4 * width * height), width, height, 3, 2))
    roost.tab_feed_pty_bytes(tab, b"\x1b[1;1Hvirtual-sent")
    roost.wait_text(tab, "virtual-sent")
    assert read() == [], "a virtual placement with no placeholder cells draws nothing"

    roost.tab_feed_pty_bytes(tab, placeholders(61, row=4, col=2, cols=3, rows=2))
    settled_images(read, placeholder_runs(61, 4, 2, 3, 2, cell))


#: Prints the shell's own `TIOCGWINSZ`, `rows,cols,xpixel,ypixel`, between
#: markers the echoed command line itself cannot match.
WINSIZE_PROBE = (
    "python3 -c 'import fcntl,termios,struct; "
    'print("WINSIZE=%d,%d,%d,%d=" % struct.unpack("HHHH", '
    'fcntl.ioctl(0, termios.TIOCGWINSZ, b"\\0"*8)))\''
)
WINSIZE_LINE = re.compile(r"WINSIZE=(\d+),(\d+),(\d+),(\d+)=")


def test_an_in_process_tabs_winsize_states_its_cells_in_device_px(roost, project):
    """The tab's first layout sets the PTY's pixel size even though the
    tab was spawned at the window's grid, so a program asking the kernel
    is told the grid times the terminal's cell, in device px."""
    tab = roost.open_tab(project, cwd="/tmp")
    wait_tab_attached(roost, tab)
    wait_shell_ready(roost, tab)
    roost.run(tab, WINSIZE_PROBE)
    deadline = time.monotonic() + scaled_timeout(10.0)
    found = WINSIZE_LINE.search(roost.dump_text(tab))
    while found is None and time.monotonic() < deadline:
        time.sleep(0.05)
        found = WINSIZE_LINE.search(roost.dump_text(tab))
    assert found is not None, roost.dump_text(tab)
    dumped = roost.dump(tab)
    cell = cell_px(roost.window_metrics())
    assert tuple(int(value) for value in found.groups()) == (
        dumped["rows"],
        dumped["cols"],
        dumped["cols"] * cell[0],
        dumped["rows"] * cell[1],
    )
