"""GTK adapter guard for the shared Rust typography model.

The file sorts last intentionally: family confirmation persists a real picker
row into the harness-owned config. The test restores the originally selected
row through the UI, but an initially unconfigured default chain cannot be
represented byte-for-byte by a single picker row. Session teardown immediately
discards the temporary copy.
"""

from __future__ import annotations

import time
import uuid

import pytest
import ui
from util import BARE_SHELL_ARGV, wait_tab_attached


def _grid(roost, tab_id: int) -> tuple[int, int]:
    dump = roost.dump(tab_id)
    return dump["cols"], dump["rows"]


def _config_lines(path, key: str) -> list[str]:
    return [
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.partition("=")[0].strip() == key
    ]


def _require_owned_gtk(target: str):
    if target != "gtk":
        pytest.skip("shared typography adapter migration is GTK-only until Iced adopts it")
    config_path = ui.owned_session_config_path()
    if config_path is None:
        pytest.skip("typography persistence requires a harness-owned config copy")
    assert config_path.parent == ui._SESSION_STATE_DIR.resolve()
    assert config_path != ui.SEED_CONFIG.resolve()
    return config_path


@pytest.fixture
def owned_gtk_config(target):
    """Prove ownership before any typography test mutates shared UI state."""
    return _require_owned_gtk(target)


@pytest.fixture
def gtk_project(owned_gtk_config, roost):
    """GTK-only project created after the harness ownership guard succeeds."""
    project_id = roost.create_project(
        name=f"pytest-typography-{uuid.uuid4().hex[:8]}", cwd="/tmp"
    )
    try:
        yield project_id
    finally:
        try:
            roost.delete_project(project_id)
        except Exception:
            pass


@pytest.fixture
def gtk_palette(owned_gtk_config, roost):
    """GTK-only palette guard whose setup cannot touch a reused UI."""
    roost.palette_dismiss()
    try:
        yield roost
    finally:
        roost.palette_dismiss()


def _activate_command(palette, command: str) -> None:
    palette.palette_open()
    state = palette.palette_activate(command)
    assert state["open"] is False


def test_gtk_shared_font_size_reflows_all_tabs_and_persists(
    owned_gtk_config, roost, gtk_project, gtk_palette
):
    config_path = owned_gtk_config
    seed_before = ui.SEED_CONFIG.read_bytes()

    roost.window_resize(960, 640)
    first = roost.open_tab(gtk_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    second = roost.open_tab(gtk_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, first)
    wait_tab_attached(roost, second)
    baseline_grid = _grid(roost, second)
    roost.focus(first)
    roost._wait(
        lambda: _grid(roost, first) == baseline_grid,
        5.0,
        "first GTK tab receives the fixed-window baseline allocation",
    )
    roost.focus(second)

    _activate_command(gtk_palette, "font_increase")
    roost._wait(
        lambda: _grid(roost, second) != baseline_grid,
        5.0,
        "shared font-size transition reflows the active GTK tab",
    )
    enlarged = _grid(roost, second)
    roost.focus(first)
    roost._wait(
        lambda: _grid(roost, first) == enlarged,
        5.0,
        "shared font-size transition reaches the hidden GTK tab on allocation",
    )
    assert _config_lines(config_path, "font-size") == ["font-size = 14"]

    third = roost.open_tab(gtk_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, third)
    roost._wait(
        lambda: _grid(roost, third) == enlarged,
        5.0,
        "new GTK tab inherits the shared live font size after allocation",
    )

    _activate_command(gtk_palette, "font_reset")
    roost._wait(
        lambda: _grid(roost, third) != enlarged,
        5.0,
        "font reset reflows the active GTK tab away from the enlarged grid",
    )
    reset_grid = _grid(roost, third)
    for tab_id in (first, second):
        roost.focus(tab_id)
        roost._wait(
            lambda tab_id=tab_id: _grid(roost, tab_id) == reset_grid,
            5.0,
            f"font reset restores the launch baseline on GTK tab {tab_id}",
        )
    assert _config_lines(config_path, "font-size") == ["font-size = 13"]

    before_second_reset = config_path.read_bytes()
    before_stat = config_path.stat()
    before_identity = (before_stat.st_dev, before_stat.st_ino, before_stat.st_mtime_ns)
    _activate_command(gtk_palette, "font_reset")
    time.sleep(0.25)
    assert config_path.read_bytes() == before_second_reset
    after_stat = config_path.stat()
    assert (after_stat.st_dev, after_stat.st_ino, after_stat.st_mtime_ns) == before_identity
    assert ui.SEED_CONFIG.read_bytes() == seed_before


def test_gtk_font_preview_dismiss_and_confirmation_are_commit_bounded(
    owned_gtk_config, roost, gtk_project, gtk_palette
):
    config_path = owned_gtk_config
    seed_before = ui.SEED_CONFIG.read_bytes()
    config_before = config_path.read_bytes()
    active_tab = roost.open_tab(gtk_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, active_tab)

    gtk_palette.palette_open()
    fonts = gtk_palette.palette_activate("select_font")
    if len(fonts["items"]) < 2:
        pytest.skip("GTK font preview requires at least two installed monospace families")
    original = fonts["items"][fonts["selection"]]
    candidates = [item for item in reversed(fonts["items"]) if item["id"] != original["id"]]
    target_font = candidates[0]

    preview = gtk_palette.palette_query(target_font["title"])
    assert preview["items"][preview["selection"]]["id"] == target_font["id"]
    assert config_path.read_bytes() == config_before

    dismissed = gtk_palette.palette_dismiss()
    assert dismissed["open"] is False
    assert config_path.read_bytes() == config_before

    try:
        gtk_palette.palette_open()
        gtk_palette.palette_activate("select_font")
        confirmed = gtk_palette.palette_activate(target_font["id"])
        assert confirmed["open"] is False
        assert _config_lines(config_path, "font-family") == [
            f'font-family = "{target_font["id"]}"'
        ]
        assert ui.SEED_CONFIG.read_bytes() == seed_before
    finally:
        gtk_palette.palette_dismiss()
        gtk_palette.palette_open()
        gtk_palette.palette_activate("select_font")
        restored = gtk_palette.palette_activate(original["id"])
        assert restored["open"] is False
        assert _config_lines(config_path, "font-family") == [
            f'font-family = "{original["id"]}"'
        ]
        assert ui.SEED_CONFIG.read_bytes() == seed_before
