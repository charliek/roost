"""Rust UI adapter guards for the shared terminal typography model.

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


def _resize_and_wait(roost, width: int, height: int) -> None:
    """Fence the fixed geometry required by these typography assertions."""
    roost.window_resize(width, height)
    roost._wait(
        lambda: (
            round(float((metrics := roost.window_metrics())["window_width"])) == width
            and round(float(metrics["window_height"])) == height
        ),
        5.0,
        f"typography fixture window allocation {width}x{height}",
    )


def _config_lines(path, key: str) -> list[str]:
    return [
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.partition("=")[0].strip() == key
    ]


def _require_owned_rust(target: str):
    if target not in {"gtk", "iced"}:
        pytest.skip("shared typography adapters are exercised by the Rust UIs")
    config_path = ui.owned_session_config_path()
    if config_path is None:
        pytest.skip("typography persistence requires a harness-owned config copy")
    assert config_path.parent == ui._SESSION_STATE_DIR.resolve()
    assert config_path != ui.SEED_CONFIG.resolve()
    return config_path


@pytest.fixture
def owned_rust_config(target):
    """Prove ownership before any typography test mutates shared UI state."""
    return _require_owned_rust(target)


@pytest.fixture
def rust_project(owned_rust_config, roost):
    """Rust UI project created after the harness ownership guard succeeds."""
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
def rust_palette(owned_rust_config, roost):
    """Rust UI palette guard whose setup cannot touch a reused UI."""
    roost.palette_dismiss()
    try:
        yield roost
    finally:
        roost.palette_dismiss()


def _activate_command(palette, command: str) -> None:
    palette.palette_open()
    state = palette.palette_activate(command)
    assert state["open"] is False


def test_rust_shared_font_size_reflows_all_tabs_and_persists(
    owned_rust_config, roost, rust_project, rust_palette
):
    config_path = owned_rust_config
    seed_before = ui.SEED_CONFIG.read_bytes()

    _resize_and_wait(roost, 960, 640)
    first = roost.open_tab(rust_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    second = roost.open_tab(rust_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, first)
    wait_tab_attached(roost, second)
    baseline_grid = _grid(roost, second)
    roost.focus(first)
    roost._wait(
        lambda: _grid(roost, first) == baseline_grid,
        5.0,
        "first Rust UI tab receives the fixed-window baseline allocation",
    )
    roost.focus(second)

    _activate_command(rust_palette, "font_increase")
    roost._wait(
        lambda: _grid(roost, second) != baseline_grid,
        5.0,
        "shared font-size transition reflows the active Rust UI tab",
    )
    enlarged = _grid(roost, second)
    roost.focus(first)
    roost._wait(
        lambda: _grid(roost, first) == enlarged,
        5.0,
        "shared font-size transition reaches the hidden Rust UI tab",
    )
    assert _config_lines(config_path, "font-size") == ["font-size = 14"]

    third = roost.open_tab(rust_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, third)
    roost._wait(
        lambda: _grid(roost, third) == enlarged,
        5.0,
        "new Rust UI tab inherits the shared live font size after allocation",
    )

    _activate_command(rust_palette, "font_reset")
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
            f"font reset restores the launch baseline on Rust UI tab {tab_id}",
        )
    assert _config_lines(config_path, "font-size") == ["font-size = 13"]

    before_second_reset = config_path.read_bytes()
    before_stat = config_path.stat()
    before_identity = (before_stat.st_dev, before_stat.st_ino, before_stat.st_mtime_ns)
    _activate_command(rust_palette, "font_reset")
    time.sleep(0.25)
    assert config_path.read_bytes() == before_second_reset
    after_stat = config_path.stat()
    assert (after_stat.st_dev, after_stat.st_ino, after_stat.st_mtime_ns) == before_identity
    assert ui.SEED_CONFIG.read_bytes() == seed_before


def test_rust_font_preview_dismiss_and_confirmation_are_commit_bounded(
    owned_rust_config, roost, rust_project, rust_palette
):
    config_path = owned_rust_config
    seed_before = ui.SEED_CONFIG.read_bytes()
    config_before = config_path.read_bytes()
    active_tab = roost.open_tab(rust_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    hidden_tab = roost.open_tab(rust_project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    wait_tab_attached(roost, active_tab)
    wait_tab_attached(roost, hidden_tab)
    _resize_and_wait(roost, 960, 640)
    baseline_grid = _grid(roost, hidden_tab)
    roost.focus(active_tab)
    roost._wait(
        lambda: _grid(roost, active_tab) == baseline_grid,
        5.0,
        "both live tabs receive the same opening family metrics",
    )
    roost.focus(hidden_tab)

    rust_palette.palette_open()
    fonts = rust_palette.palette_activate("select_font")
    assert len(fonts["items"]) >= 2, (
        "required Rust-UI font parity lane needs two installed monospace families; "
        f"discovered {fonts['items']}"
    )
    original = fonts["items"][fonts["selection"]]
    candidates = [item for item in reversed(fonts["items"]) if item["id"] != original["id"]]
    target_font = candidates[0]
    preview = rust_palette.palette_query(target_font["title"])
    assert preview["items"][preview["selection"]]["id"] == target_font["id"]
    roost._wait(
        lambda: roost.window_metrics().get("terminal_font_family")
        == target_font["id"],
        5.0,
        "font preview reaches the live renderer family token",
    )
    target_grid = _grid(roost, active_tab)
    if target_grid != baseline_grid:
        roost._wait(
            lambda: _grid(roost, hidden_tab) == target_grid,
            5.0,
            "font preview reflows a hidden live tab when metrics differ",
        )
    assert config_path.read_bytes() == config_before

    dismissed = rust_palette.palette_dismiss()
    assert dismissed["open"] is False
    roost._wait(
        lambda: roost.window_metrics().get("terminal_font_family") == original["id"],
        5.0,
        "font dismissal restores the opening renderer family token",
    )
    if target_grid != baseline_grid:
        for tab_id in (active_tab, hidden_tab):
            roost._wait(
                lambda tab_id=tab_id: _grid(roost, tab_id) == baseline_grid,
                5.0,
                f"font dismissal restores opening metrics on tab {tab_id}",
            )
    assert config_path.read_bytes() == config_before

    try:
        rust_palette.palette_open()
        rust_palette.palette_activate("select_font")
        confirmed = rust_palette.palette_activate(target_font["id"])
        assert confirmed["open"] is False
        assert roost.window_metrics()["terminal_font_family"] == target_font["id"]
        if target_grid != baseline_grid:
            for tab_id in (active_tab, hidden_tab):
                roost._wait(
                    lambda tab_id=tab_id: _grid(roost, tab_id) == target_grid,
                    5.0,
                    f"font confirmation reflows live tab {tab_id}",
                )
        assert _config_lines(config_path, "font-family") == [
            f'font-family = "{target_font["id"]}"'
        ]
        inherited_tab = roost.open_tab(
            rust_project, cwd="/tmp", argv=BARE_SHELL_ARGV
        )
        wait_tab_attached(roost, inherited_tab)
        assert roost.window_metrics()["terminal_font_family"] == target_font["id"]
        if target_grid != baseline_grid:
            roost._wait(
                lambda: _grid(roost, inherited_tab) == target_grid,
                5.0,
                "a new tab inherits the confirmed live family metrics",
            )
        assert ui.SEED_CONFIG.read_bytes() == seed_before
    finally:
        rust_palette.palette_dismiss()
        rust_palette.palette_open()
        rust_palette.palette_activate("select_font")
        restored = rust_palette.palette_activate(original["id"])
        assert restored["open"] is False
        assert _config_lines(config_path, "font-family") == [
            f'font-family = "{original["id"]}"'
        ]
        assert ui.SEED_CONFIG.read_bytes() == seed_before
