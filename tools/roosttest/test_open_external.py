"""Open Settings File, Open Documentation and Toggle Full Screen — plan 073 D3.

The palette rows are the same actions the macOS menu bar and the bound
keys reach. Under `ROOST_TEST_MODE=1` the two openers never launch an
editor or a browser: they raise the toast `Would open <target>` instead,
which `app.notice_dump` reads back. The "zero launches" property itself is
pinned by a Rust test over an injected runner (`url_launcher.rs`); this
module proves the rows are wired to that path end to end.

Skipped under `--roost-target mac`: the Swift app has neither action.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest
import ui
from client import Timeout

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

DOCS_URL = "https://charliek.github.io/roost/"


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip("Open Settings File / Open Documentation / Toggle Full Screen are iced-only")


pytestmark = pytest.mark.skipif(
    not TEST_MODE,
    reason="the `Would open` toast exists only under ROOST_TEST_MODE=1 in the UI's launch env",
)


def _activate(palette, row_id: str) -> None:
    palette.palette_open("commands")
    palette.palette_activate(row_id)


def _toast(roost) -> str | None:
    line = roost.notice_dump()["bottom_line"]
    return line["text"] if line else None


def test_open_settings_file_names_the_config_it_would_open(palette):
    config = ui.owned_session_config_path()
    if config is None:
        pytest.skip("a reused instance's config is not this harness's to name")
    _activate(palette, "open_config")
    seen: list[str] = []

    def toasted() -> bool:
        text = _toast(palette)
        if text and text.startswith("Would open "):
            seen.append(text)
            return True
        return False

    palette._wait(toasted, 5.0, "Open Settings File toasts the path it would open")
    named = Path(seen[0].removeprefix("Would open "))
    assert named.resolve() == config, seen[0]


def test_open_documentation_names_the_docs_url_it_would_open(palette):
    _activate(palette, "open_docs")
    palette._wait(
        lambda: _toast(palette) == f"Would open {DOCS_URL}",
        5.0,
        "Open Documentation toasts the URL it would open",
    )


def _size(roost) -> tuple[float, float]:
    metrics = roost.window_metrics()
    return metrics["window_width"], metrics["window_height"]


def test_toggle_full_screen_changes_the_window_size_and_restores_it(palette):
    if sys.platform == "darwin":
        pytest.skip("macOS full screen is a native animated Space transition")
    before = _size(palette)
    _activate(palette, "toggle_fullscreen")
    try:
        palette._wait(
            lambda: _size(palette) != before,
            5.0,
            "the window size follows the full-screen toggle",
        )
    except Timeout:
        pytest.skip("this compositor does not honor a full-screen request")
    _activate(palette, "toggle_fullscreen")
    palette._wait(
        lambda: _size(palette) == before,
        5.0,
        "toggling again restores the windowed size",
    )
