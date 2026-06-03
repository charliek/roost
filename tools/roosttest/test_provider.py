"""Custom-palette provider E2E (Cmd/Alt+Shift+E) + palette.present.

Drives the provider palette off a *discovered* fixture script
(`fixtures/providers/fixture-provider.sh`, beside the seeded
`ROOST_CONFIG`), exercising the full contract end-to-end:

  * directory discovery surfaces the script as a row,
  * activating it spawns `list` off-main and drills into its items,
  * activating an item spawns `activate` with `$ROOST_SELECTED_ID` set and
    drills into a row echoing that id — proving the selection round-trips
    through the subprocess env/stdin path.

Plus `palette.present` (the programmatic twin): a blocking op driven from a
second connection while the first picks a row.

Skips when the seed config/providers dir isn't active (a developer's
already-running UI keeps its own config); CI always launches fresh.
"""

from __future__ import annotations

import threading

import ui
from client import Roost
from util import precondition


def _provider_root(palette) -> dict:
    """Open the custom palette; return its {title: id} provider rows."""
    st = palette.palette_open(kind="custom")
    assert st["frame"] == "custom"
    return {it["title"]: it["id"] for it in st["items"]}


def _require_seed(items):
    precondition(
        "Fixture Provider" in items,
        "seed config / providers dir not active (UI not launched by the harness)",
    )


def _titles(state: dict) -> set[str]:
    return {it["title"] for it in state.get("items", [])}


def test_custom_palette_lists_discovered_provider(palette):
    items = _provider_root(palette)
    _require_seed(items)
    assert "Fixture Provider" in items, list(items)


def test_provider_list_then_activate_round_trips_selection(roost, palette):
    items = _provider_root(palette)
    _require_seed(items)

    # Activate the provider row → async `list` spawn → drill into its rows.
    palette.palette_activate(items["Fixture Provider"])
    roost._wait(
        lambda: "Provider Alpha" in _titles(roost.palette_state()),
        8.0,
        "provider list populated",
    )
    by_title = {it["title"]: it["id"] for it in roost.palette_state()["items"]}

    # Activate an item → async `activate` spawn with ROOST_SELECTED_ID=alpha
    # → drill into the confirmation row proving the id round-tripped.
    roost.palette_activate(by_title["Provider Alpha"])
    roost._wait(
        lambda: "picked alpha" in _titles(roost.palette_state()),
        8.0,
        "activate echoed the selected id",
    )


def test_palette_present_returns_selection(palette, target):
    """`palette.present` blocks until a pick; drive the selection from a
    second connection while the present call is in flight."""
    presenter = Roost(ui.socket_path(target))
    result: dict = {}

    def present():
        result["r"] = presenter.palette_present(
            items=[{"id": "x", "title": "Pick X"}, {"id": "y", "title": "Pick Y"}],
            title="Choose one",
        )

    t = threading.Thread(target=present, daemon=True)
    t.start()
    try:
        palette._wait(
            lambda: palette.palette_state().get("frame") == "present",
            5.0,
            "present palette open",
        )
        st = palette.palette_activate("y")
        assert st["open"] is False  # picking confirms + closes
    finally:
        t.join(timeout=5)
        presenter.close()

    assert result.get("r", {}).get("selected_id") == "y"
    assert result["r"].get("dismissed") is False
