"""Custom-command launcher E2E (Cmd/Alt+Shift+T). Drives the launcher
palette frame off a seeded config (`fixtures/launcher.conf`, pointed at
via `ROOST_CONFIG` when the harness launches the UI) and verifies that
activating a row spawns a tab *actually running* the command — the
foundation for the planned Lua / complex-action launches.

Skips when the seed config isn't active (a developer's already-running UI
keeps its own config); CI always launches fresh, so it runs there.
"""

from __future__ import annotations

from util import precondition, spawned_tab_id, wait_spawned_output

SEED_LABELS = ("Echo Marker", "List Tmp")


def _launcher_items(palette):
    """Open the launcher frame; return its {title: id} rows."""
    st = palette.palette_open(kind="launcher")
    assert st["frame"] == "launcher"
    return {it["title"]: it["id"] for it in st["items"]}


def _require_seed(items):
    precondition("Echo Marker" in items, "seed config not active (UI not launched by the harness)")


def test_launcher_lists_seeded_commands(palette):
    items = _launcher_items(palette)
    _require_seed(items)
    for label in SEED_LABELS:
        assert label in items, list(items)


def test_launcher_launches_command_in_new_tab(roost, project, palette):
    """Activating a launcher row spawns a new tab that runs the command —
    proving the launcher routes to a real shell launch, not just UI."""
    seed_tab = roost.open_tab(project, cwd="/tmp")
    roost.focus(seed_tab)  # make `project` active so the launch lands here
    items = _launcher_items(palette)
    _require_seed(items)

    before = {int(t["id"]) for t in roost.tabs()}
    st = palette.palette_activate(items["Echo Marker"])
    assert st["open"] is False  # launching confirms + closes the palette

    new_id = spawned_tab_id(roost, before, "launcher spawned a tab")
    # `hold=true` keeps the shell open, so the command's output stays on
    # screen and dumpable.
    wait_spawned_output(roost, new_id, "LAUNCH_MARKER=ok")
