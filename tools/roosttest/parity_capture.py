"""Opt-in deterministic visual-parity capture for one Roost UI target.

This file intentionally does not start with ``test_``: the ordinary functional
suite should not mutate its shared session into a visual fixture. Run it through
``tools/screenshot/parity.py``, which gives every target a fresh harness-owned
state/config session and unique artifact identity.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from client import Roost, RoostError, Timeout
from test_agent_palette import _seed
from test_sidebar_agents import _agent_row, _set_agents_visible
from test_sidebar_collapse_persistence import _toggle_to_visible
from util import BARE_SHELL_ARGV, is_fresh, wait_tab_attached

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "screenshot"))
import parity  # noqa: E402

WINDOW_WIDTH = 1100.0
WINDOW_HEIGHT = 700.0
PROJECT_NAME = "Parity Project"
TAB_LIFECYCLES = (
    ("Working", "working"),
    ("Waiting", "waiting"),
    ("Finished", "finished"),
    ("Failed", "failed"),
)
FONT_VISUAL_TEXT = "Latin normal | bold | italic | e\u0301 | \u754c"


def _semantic_fixture_observation(roost, project_id: int, tabs: dict[str, int]) -> dict:
    projects = roost.list()
    sidebar = roost.sidebar_dump()
    sidebar_rows = next(
        (
            item["agents"]
            for item in sidebar["projects"]
            if int(item["project_id"]) == project_id
        ),
        [],
    )
    return {
        "projects": projects,
        "sidebar_rows": sidebar_rows,
        "all_sidebar_tabs_present": all(
            _agent_row(sidebar, project_id, tab_id) is not None
            for tab_id in tabs.values()
        ),
    }


def _semantic_fixture_ready(observation: dict, project_id: int, tabs: dict[str, int]) -> bool:
    projects = observation["projects"]
    if len(projects) != 1:
        return False
    project = projects[0]
    if int(project["id"]) != project_id or project["name"] != PROJECT_NAME:
        return False
    rendered_tabs = project["tabs"]
    expected_titles = [title for title, _ in TAB_LIFECYCLES]
    expected_lifecycles = [lifecycle for _, lifecycle in TAB_LIFECYCLES]
    return (
        [tab["title"] for tab in rendered_tabs] == expected_titles
        and [tab["agent_lifecycle"] for tab in rendered_tabs] == expected_lifecycles
        and any(
            int(tab["id"]) == tabs["Failed"] and tab["is_active"]
            for tab in rendered_tabs
        )
        and any(
            int(tab["id"]) == tabs["Waiting"] and tab["has_notification"]
            for tab in rendered_tabs
        )
        and observation["all_sidebar_tabs_present"]
        and len(observation["sidebar_rows"]) == len(TAB_LIFECYCLES)
    )


def _capture_settled(
    roost,
    path: Path,
    central_region_different_from: str | set[str] | None = None,
) -> tuple[int, int]:
    previous: tuple[int, int] | None = None
    settled: tuple[int, int] | None = None

    def capture() -> bool:
        nonlocal previous, settled
        try:
            png, width, height = roost.screenshot(scale=parity.SCALE)
        except RoostError as error:
            if error.code == "internal" and "empty snapshot" in error.message:
                return False
            raise
        path.write_bytes(png)
        central_digest = parity.central_region_digest(path)
        excluded_digests = (
            central_region_different_from
            if isinstance(central_region_different_from, set)
            else {central_region_different_from}
        )
        if central_digest in excluded_digests:
            previous = None
            return False
        current = (width, height)
        if current == previous:
            settled = current
            return True
        previous = current
        return False

    Roost._wait(capture, 10.0, f"two stable renderer extents for {path.name}")
    assert settled is not None
    return settled


def _capture_palette_variant(
    roost,
    output: Path,
    filename: str,
    excluded_digests: set[str],
) -> tuple[Path, tuple[int, int], str]:
    path = output / filename
    extent = _capture_settled(
        roost,
        path,
        central_region_different_from=excluded_digests,
    )
    return path, extent, parity.central_region_digest(path)


def _write_measurements(
    output: Path,
    metadata: dict,
    shell_path: Path,
    palette_path: Path | None,
    palette_variants: dict[str, Path],
    chrome_variants: dict[str, Path],
    chrome_unavailable_reason: str | None,
    metrics: dict,
    fixture: dict,
    font_comparison: dict,
) -> dict:
    sidebar_width = round(float(metrics["sidebar_width"]))
    shell = parity.measure_shell(
        shell_path,
        sidebar_width=sidebar_width,
    )
    if palette_path is None:
        palette_measurement = {
            "available": False,
            "reason": "AppKit app.screenshot excludes the palette child NSPanel",
        }
    else:
        palette_image = parity.pngtool.load(str(palette_path))
        palette_measurement = {
            "available": True,
            "png": palette_path.name,
            "sha256": parity.sha256(palette_path),
            "width": palette_image[0],
            "height": palette_image[1],
        }
    palette_measurement["variants"] = {
        name: {
            "png": path.name,
            "sha256": parity.sha256(path),
            "width": parity.pngtool.load(str(path))[0],
            "height": parity.pngtool.load(str(path))[1],
        }
        for name, path in sorted(palette_variants.items())
    }
    if agent_path := palette_variants.get("agents"):
        agent_geometry = parity.measure_agent_palette(agent_path, sidebar_width)
        gap = agent_geometry["name_status_gap"]
        if not 0 <= gap <= 20:
            raise AssertionError(
                f"agent status must trail its name (measured gap: {gap}px)"
            )
        if agent_geometry["status_ink_span"] < 140:
            raise AssertionError("long agent status was truncated despite available row width")
        if agent_geometry["trailing_ink_pixels"] == 0:
            raise AssertionError("agent metrics/time are not visible after a long status")
        palette_measurement["agent_geometry"] = agent_geometry
    document = {
        "metadata": metadata,
        "fixture": fixture,
        "window_metrics": metrics,
        "shell": shell,
        "palette": palette_measurement,
        "chrome": (
            {"available": False, "reason": chrome_unavailable_reason}
            if chrome_unavailable_reason
            else {
                "available": True,
                "variants": {
                    name: {
                        "png": path.name,
                        "sha256": parity.sha256(path),
                        "width": parity.pngtool.load(str(path))[0],
                        "height": parity.pngtool.load(str(path))[1],
                    }
                    for name, path in sorted(chrome_variants.items())
                },
            }
        ),
        "font_comparison": font_comparison,
    }
    parity.atomic_json(output / "measurements.json", document)
    return document


@pytest.mark.skipif(
    os.environ.get("ROOST_TEST_MODE") != "1",
    reason="visual parity capture requires deterministic window/test operations",
)
def test_capture_visual_parity_fixture(roost, target):
    assert is_fresh(), "visual parity capture must own disposable harness state"
    run_id = os.environ["ROOST_PARITY_RUN_ID"]
    commit = os.environ["ROOST_PARITY_COMMIT"]
    output_base = Path(os.environ["ROOST_PARITY_OUTPUT_BASE"])
    metadata = parity.environment_metadata(target, run_id, commit)
    output = output_base / run_id / parity.environment_key(metadata)
    output.mkdir(parents=True, exist_ok=False)

    previous_projects = [int(project["id"]) for project in roost.list()]
    project_id = roost.create_project(name=PROJECT_NAME, cwd="/tmp")
    for stale_id in previous_projects:
        roost.delete_project(stale_id)

    _set_agents_visible(roost, True)
    _toggle_to_visible(roost)
    tabs: dict[str, int] = {}
    for index, (title, lifecycle) in enumerate(TAB_LIFECYCLES):
        tab_id = roost.open_tab(
            project_id,
            cwd="/tmp",
            title=title,
            argv=BARE_SHELL_ARGV,
        )
        _seed(
            roost,
            tab_id,
            lifecycle=lifecycle,
            name=title,
            source="parity-capture",
            session=f"parity-{index}-{lifecycle}",
        )
        tabs[title] = tab_id

    # AppKit asynchronously creates a starter tab with a new project; the Rust
    # targets do not. By now the named open operations have allowed it to
    # materialize, so discard every tab that is not part of the shared fixture.
    fixture_tab_ids = set(tabs.values())
    for starter_tab_id in roost.project_tab_ids(project_id):
        if starter_tab_id not in fixture_tab_ids:
            roost.close_tab(starter_tab_id)

    roost.focus(tabs["Failed"])
    roost.notify(tabs["Waiting"], "Parity notification", "Waiting needs input")
    observation: dict = {}

    def semantic_fixture_ready() -> bool:
        nonlocal observation
        observation = _semantic_fixture_observation(roost, project_id, tabs)
        return _semantic_fixture_ready(observation, project_id, tabs)

    try:
        roost._wait(
            semantic_fixture_ready,
            15.0,
            "fixed parity workspace, lifecycle rows, focus, and notification",
        )
    except Timeout as error:
        raise AssertionError(
            "parity fixture never reached its semantic contract; last observation: "
            f"{json.dumps(observation, sort_keys=True)}"
        ) from error
    wait_tab_attached(roost, tabs["Failed"])
    roost.window_resize(WINDOW_WIDTH, WINDOW_HEIGHT)

    metrics: dict = {}

    def geometry_ready() -> bool:
        nonlocal metrics
        metrics = roost.window_metrics()
        return (
            not metrics["sidebar_collapsed"]
            and abs(float(metrics["sidebar_width"]) - 220.0) <= 1.0
            and float(metrics["window_width"]) >= 1000.0
            and float(metrics["window_height"]) >= 600.0
        )

    roost._wait(geometry_ready, 10.0, "parity window and visible 220pt sidebar")
    roost._wait(
        lambda: bool(roost.dump_text(tabs["Failed"]).strip()),
        10.0,
        "active parity terminal paints a prompt",
    )
    # DEC cursor style 2 is a steady block. Blink phase is otherwise a dynamic
    # pixel region that can make equally-correct captures look different.
    roost.tab_feed_pty_bytes(tabs["Failed"], b"\x1b[2 q")
    roost.tab_feed_pty_bytes(
        tabs["Failed"],
        ("\r\nLatin normal | \x1b[1mbold\x1b[0m | \x1b[3mitalic\x1b[0m | e\u0301 | \u754c").encode(),
    )
    roost._wait(
        lambda: FONT_VISUAL_TEXT in roost.dump_text(tabs["Failed"]),
        10.0,
        "font comparison text reaches the active renderer",
    )

    shell_path = output / "shell.png"
    shell_extent = _capture_settled(roost, shell_path)

    state = roost.palette_open("commands")

    def command_palette_ready() -> bool:
        nonlocal state
        state = roost.palette_state()
        return (
            state.get("open") is True
            and state.get("frame") == "commands"
            and state.get("query") == ""
            and state.get("selection") == 0
            and any(item.get("id") == "new_project" for item in state.get("items", []))
        )

    roost._wait(command_palette_ready, 10.0, "root command palette semantic state")
    command_state = state
    palette_path: Path | None = None
    palette_extent: tuple[int, int] | None = None
    palette_variants: dict[str, Path] = {}
    palette_variant_extents: dict[str, tuple[int, int]] = {}
    previous_palette_digest = parity.central_region_digest(shell_path)
    if target != "mac":
        palette_path, palette_extent, previous_palette_digest = _capture_palette_variant(
            roost,
            output,
            "palette.png",
            {previous_palette_digest},
        )
        palette_variants["commands"] = palette_path
        palette_variant_extents["commands"] = palette_extent

    state = roost.palette_query("project")
    assert state["frame"] == "commands" and state["query"] == "project"
    if target != "mac":
        path, extent, previous_palette_digest = _capture_palette_variant(
            roost, output, "palette-query.png", {previous_palette_digest}
        )
        palette_variants["query"] = path
        palette_variant_extents["query"] = extent

    roost.palette_dismiss()
    _seed(
        roost,
        tabs["Working"],
        lifecycle="working",
        name="Working through an intentionally long agent name",
        source="parity-long-agent",
        session="parity-2-working",
    )
    _seed(
        roost,
        tabs["Failed"],
        lifecycle="failed",
        name="Failed",
        detail="an intentionally long failure detail for layout pressure",
        source="parity-long-failure",
        session="parity-1-failed-detail",
    )
    state = roost.palette_open("agents")
    assert state["frame"] == "agents" and len(state["items"]) == len(TAB_LIFECYCLES)
    if target != "mac":
        path, extent, previous_palette_digest = _capture_palette_variant(
            roost, output, "palette-agents.png", {previous_palette_digest}
        )
        palette_variants["agents"] = path
        palette_variant_extents["agents"] = extent

    roost.palette_dismiss()
    roost.palette_open("commands")
    state = roost.palette_activate("view_notifications")
    assert state["frame"] == "notifications"
    assert any(item["id"].startswith("notif:") for item in state["items"])
    if target != "mac":
        path, extent, previous_palette_digest = _capture_palette_variant(
            roost, output, "palette-notifications.png", {previous_palette_digest}
        )
        palette_variants["notifications"] = path
        palette_variant_extents["notifications"] = extent

    roost.palette_dismiss()
    state = roost.palette_open("custom")
    provider = next(
        (item for item in state["items"] if item["title"] == "Fixture Provider"),
        None,
    )
    assert provider is not None, state["items"]
    roost.palette_activate(provider["id"])
    roost._wait(
        lambda: any(
            item["title"] == "Disabled row"
            for item in roost.palette_state().get("items", [])
        ),
        8.0,
        "provider palette with a disabled row",
    )
    state = roost.palette_state()
    if target != "mac":
        path, extent, previous_palette_digest = _capture_palette_variant(
            roost, output, "palette-provider.png", {previous_palette_digest}
        )
        palette_variants["provider"] = path
        palette_variant_extents["provider"] = extent

    roost.palette_dismiss()
    roost.palette_open("commands")
    state = roost.palette_activate("select_font")
    assert state["frame"] == "fonts" and state["items"]
    if target != "mac":
        path, extent, previous_palette_digest = _capture_palette_variant(
            roost, output, "palette-fonts.png", {previous_palette_digest}
        )
        palette_variants["fonts"] = path
        palette_variant_extents["fonts"] = extent

    original_font = state["items"][state["selection"]]["id"]
    alternate_fonts = [
        item
        for item in reversed(state["items"])
        if item["id"] != original_font
        and item["id"].casefold() != "monospace"
    ]
    alternate_fonts.extend(
        item
        for item in state["items"]
        if item["id"] != original_font and item["id"].casefold() == "monospace"
    )
    font_comparison = {
        "fixture": FONT_VISUAL_TEXT,
        "baseline_font": original_font,
        "baseline_png": shell_path.name,
        "baseline_sha256": parity.sha256(shell_path),
        "alternate_available": False,
    }
    terminal_top = metrics.get("terminal_top")
    if not isinstance(terminal_top, (int, float)):
        terminal_top = parity.measure_shell(
            shell_path, round(float(metrics["sidebar_width"]))
        )["terminal_top"]
    assert isinstance(terminal_top, int | float)
    terminal_text_bounds = (
        round(float(metrics["sidebar_width"])),
        round(float(terminal_top)),
        shell_extent[0],
        min(shell_extent[1], round(float(terminal_top)) + 120),
    )
    baseline_text_digest = parity.region_digest(shell_path, terminal_text_bounds)
    font_comparison["terminal_text_bounds"] = list(terminal_text_bounds)
    font_comparison["baseline_text_sha256"] = baseline_text_digest
    if not alternate_fonts:
        font_comparison["reason"] = "no alternate installed monospace family"
        roost.palette_dismiss()
    else:
        alternate_path = output / "terminal-font-alternate.png"
        for index, alternate_font in enumerate(alternate_fonts):
            if index > 0:
                roost.palette_open("commands")
                state = roost.palette_activate("select_font")
                assert state["frame"] == "fonts"
            state = roost.palette_activate(alternate_font["id"])
            assert state["open"] is False
            alternate_extent = _capture_settled(roost, alternate_path)
            alternate_sha = parity.sha256(alternate_path)
            alternate_text_digest = parity.region_digest(
                alternate_path, terminal_text_bounds
            )
            if alternate_text_digest == baseline_text_digest:
                continue
            font_comparison.update(
                {
                    "alternate_available": True,
                    "alternate_font": alternate_font["id"],
                    "alternate_png": alternate_path.name,
                    "alternate_sha256": alternate_sha,
                    "alternate_text_sha256": alternate_text_digest,
                    "alternate_extent": list(alternate_extent),
                }
            )
            break
        if not font_comparison["alternate_available"]:
            alternate_path.unlink(missing_ok=True)
            font_comparison["reason"] = (
                "no installed alternate produced distinct terminal glyphs; "
                "generic aliases may resolve to the same physical face"
            )

    chrome_variants = {}
    chrome_unavailable_reason = None
    rename_extent = None
    tab_rename_extent = None
    if target == "mac":
        # AppKit's product screenshot path does not reliably composite native
        # inline text fields at their window coordinates. Naming the limitation
        # is more useful than publishing a detached field as parity evidence.
        chrome_unavailable_reason = (
            "AppKit product screenshots do not reliably place native inline "
            "text editors; compare the Iced editor capture instead"
        )
    else:
        # A command-palette rename activation enters application-owned chrome
        # and closes the overlay. Capture the compact selected-all editor itself;
        # the next harness launch owns cleanup, so no synthetic keyboard path is
        # mixed into this visual fixture.
        roost.palette_open("commands")
        rename_state = roost.palette_activate("rename_project")
        assert rename_state["open"] is False
        rename_path = output / "project-rename.png"
        rename_extent = _capture_settled(roost, rename_path)
        roost.palette_open("commands")
        tab_rename_state = roost.palette_activate("rename_tab")
        assert tab_rename_state["open"] is False
        tab_rename_path = output / "tab-rename.png"
        tab_rename_extent = _capture_settled(roost, tab_rename_path)
        chrome_variants = {
            "project_rename": rename_path,
            "tab_rename": tab_rename_path,
        }

    fixture = {
        "project": PROJECT_NAME,
        "tabs": [
            {
                "title": title,
                "lifecycle": lifecycle,
                "active": title == "Failed",
                "notification": title == "Waiting",
            }
            for title, lifecycle in TAB_LIFECYCLES
        ],
        "palette": {
            "frame": command_state["frame"],
            "query": command_state["query"],
            "selection": command_state["selection"],
            "variants": [
                "commands",
                "query",
                "agents",
                "notifications",
                "provider",
                "fonts",
            ],
        },
    }
    document = _write_measurements(
        output,
        metadata,
        shell_path,
        palette_path,
        palette_variants,
        chrome_variants,
        chrome_unavailable_reason,
        metrics,
        fixture,
        font_comparison,
    )

    # Validate reusable measurement availability after both PNGs and the atomic
    # JSON are durable. Visual parity remains a human comparison against the
    # reference captures, not a growing set of temporary pixel expectations.
    shell = document["shell"]
    assert (shell["width"], shell["height"]) == shell_extent
    if palette_extent is not None:
        assert (
            document["palette"]["width"],
            document["palette"]["height"],
        ) == palette_extent
        for name, extent in palette_variant_extents.items():
            assert (
                document["palette"]["variants"][name]["width"],
                document["palette"]["variants"][name]["height"],
            ) == extent
    else:
        assert document["palette"]["available"] is False
    if chrome_unavailable_reason:
        assert document["chrome"] == {
            "available": False,
            "reason": chrome_unavailable_reason,
        }
    else:
        assert (
            document["chrome"]["variants"]["project_rename"]["width"],
            document["chrome"]["variants"]["project_rename"]["height"],
        ) == rename_extent
        assert (
            document["chrome"]["variants"]["tab_rename"]["width"],
            document["chrome"]["variants"]["tab_rename"]["height"],
        ) == tab_rename_extent
    assert tuple(shell["terminal_sample"]) == parity.TERMINAL_BACKGROUND
    assert shell["terminal_top"] is not None
    assert shell["terminal_left"] is not None
    assert all(shell["lifecycle_components"][name] for name in parity.LIFECYCLE_COLORS)
