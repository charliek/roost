"""The sidebar's Agents panel E2E — plan 080 (Roost-Iced only).

Every agent the sidebar draws, on every host, in one list at the bottom of
the sidebar, in one of three orders (`agent-panel-sort`). The panel is read
through `app.sidebar_dump`'s `agent_panel`, which is derived from the same
row cache the panel paints from, in the same order, with every text field
in full (the panel draws them elided to the sidebar's width).

Iced-only: the Swift app has no panel and answers the dump without
`agent_panel`, and `make e2e-mac` collects this whole directory — so the
module skips off `--roost-target`, as `test_view_perf.py` does.

Assertions are scoped to the agents a test seeded, by tab, and to their
relative order: a reused dev UI may have other agents open. The sort and
the toggle are written back to `config.conf`, so the cases that change
them need a harness-owned config and skip on a reused instance.

The split between the projects and the panel is the divider's to drag;
its share is set here through the test-mode `sidebar.set_agents_fraction`
(plan 080 D8), read back as `fraction_permille`, off `state.json`, and —
for the floors, which clamp only what is drawn — off the divider's own
fill in a capture. Those cases need `ROOST_TEST_MODE=1`, and put the share
back as they found it.

Condition waits only — except the one read the cycling case makes
straight after the palette activation, which is the point of it.
"""

from __future__ import annotations

import json
import math
import os
import struct
import time
import uuid

import pytest

import session as sessionlib
import ui
from client import Roost, RoostError
from conftest import skip_on_live_wayland_desktop
from test_agent_palette import _seed
from test_sidebar_agents import _palette_agent_tab_ids, _set_agents_visible
from test_sidebar_collapse_persistence import _toggle_to_visible
from test_sidebar_pixels import _blobs, _capture
from util import BARE_SHELL_ARGV, is_fresh, skip_on_ci, wait_for_config_line

SEPARATOR = " › "


@pytest.fixture(autouse=True)
def _iced_only(target):
    if target != "iced":
        pytest.skip(
            "the sidebar Agents panel is Roost-Iced only (plan 080); the Swift "
            "app answers app.sidebar_dump without agent_panel"
        )


@pytest.fixture
def project2(roost, project):
    """A second throwaway project, after `project` in sidebar order."""
    pid = roost.create_project(name=f"pytest-panel2-{uuid.uuid4().hex[:6]}", cwd="/tmp")
    yield pid
    roost.delete_project(pid)


@pytest.fixture
def owned_config(roost):
    """The harness-owned `config.conf`, with the panel's sort and toggle
    put back as they were. Skips on a reused developer instance, whose
    own config these cases must not rewrite."""
    config = ui.owned_session_config_path()
    if config is None:
        pytest.skip("cycling the sort or the toggle writes config.conf; needs a harness-owned UI")
    panel = _panel(roost)
    try:
        yield config
    finally:
        _set_sort(roost, panel["sort"])
        _set_agents_visible(roost, panel["visible"])


def _panel(roost) -> dict:
    return roost.sidebar_dump()["agent_panel"]


def _agents(panel: dict, tabs: set[str] | None = None) -> list[dict]:
    return [
        entry
        for entry in panel["entries"]
        if entry["kind"] == "agent" and (tabs is None or entry["tab"] in tabs)
    ]


def _order(panel: dict, tabs: set[str]) -> list[str]:
    return [entry["tab"] for entry in _agents(panel, tabs)]


def _cycle(roost) -> None:
    """The palette's Cycle Agent Panel Sort. Leaves the palette closed."""
    roost.palette_dismiss()
    roost.palette_open()
    st = roost.palette_activate("cycle_agent_panel_sort")
    assert st["open"] is False, st


def _set_sort(roost, want: str) -> None:
    for _ in range(3):
        if _panel(roost)["sort"] == want:
            return
        _cycle(roost)
    assert _panel(roost)["sort"] == want


def _wait_listed(roost, tabs: set[str]) -> dict:
    """The panel once every one of `tabs` is in it."""

    def listed() -> dict | None:
        panel = _panel(roost)
        return panel if tabs <= {entry["tab"] for entry in _agents(panel)} else None

    return sessionlib.wait_until(listed, 10.0, f"agents {sorted(tabs)} reach the Agents panel")


def _location(roost, project_name: str) -> str:
    """A row's location outside `grouped`: the bare project name while the
    sidebar draws one band, `local › name` once it draws more (D3)."""
    if len(roost.sidebar_sections()) > 1:
        return f"local{SEPARATOR}{project_name}"
    return project_name


def _seed_two_projects(roost, project, p2) -> dict[str, int]:
    """Mixed lifecycles across two projects, laid out so that every sort
    orders them differently: the first project's agents are the least
    urgent, so `grouped` (project order) and `priority` (urgency across
    projects) disagree. Returns the tabs by lifecycle."""
    tabs: dict[str, int] = {}
    for where, lifecycle in (
        (project, "working"),
        (project, "finished"),
        (p2, "failed"),
        (p2, "waiting"),
    ):
        tab = roost.open_tab(where, cwd="/tmp", argv=BARE_SHELL_ARGV)
        _seed(roost, tab, lifecycle=lifecycle, name=f"panel-{lifecycle}")
        tabs[lifecycle] = tab
    return tabs


def _keys(tabs: dict[str, int]) -> set[str]:
    return {str(tab) for tab in tabs.values()}


def _palette_order(roost, keys: set[str]) -> list[str]:
    ids = [str(tab) for tab in _palette_agent_tab_ids(roost.palette_items("agents"))]
    return [key for key in ids if key in keys]


def test_priority_lists_live_agents_in_the_palettes_order(roost, project, project2, owned_config):
    tabs = _seed_two_projects(roost, project, project2)
    keys = _keys(tabs)
    _set_sort(roost, "priority")
    panel = _wait_listed(roost, keys)
    assert panel["sort"] == "priority"
    assert not [entry for entry in panel["entries"] if entry["kind"] == "group"], panel
    order = _order(panel, keys)
    assert order == _palette_order(roost, keys)
    assert order[0] == str(tabs["failed"]) and order[-1] == str(tabs["finished"]), order

    names = {pid: roost.project(pid)["name"] for pid in (project, project2)}
    where = {"working": project, "finished": project, "failed": project2, "waiting": project2}
    for lifecycle, tab in tabs.items():
        (entry,) = _agents(panel, {str(tab)})
        assert entry["name"] == f"panel-{lifecycle}", entry
        assert entry["lifecycle"] == lifecycle, entry
        assert entry["location_text"] == _location(roost, names[where[lifecycle]]), entry
        assert entry["dimmed"] is False, entry


def test_cycling_the_sort_reorders_the_dump_at_once_and_writes_the_config(
    roost, project, project2, owned_config
):
    keys = _keys(_seed_two_projects(roost, project, project2))
    _set_sort(roost, "grouped")
    grouped = _order(_wait_listed(roost, keys), keys)
    priority = _palette_order(roost, keys)
    assert grouped != priority, "the seed must order the two sorts differently"

    _cycle(roost)
    # One read, no wait: the activation's own turn leaves the new order
    # in the dump.
    panel = _panel(roost)
    assert panel["sort"] == "priority"
    assert _order(panel, keys) == priority
    wait_for_config_line(owned_config, "agent-panel-sort", "agent-panel-sort = priority")


def _groups(panel: dict) -> list[tuple[str, int, list[str]]]:
    """`grouped`'s heads in order, each with its count and the agents
    listed under it."""
    groups: list[tuple[str, int, list[str]]] = []
    for entry in panel["entries"]:
        if entry["kind"] == "group":
            groups.append((entry["label"], entry["count"], []))
        else:
            assert groups, f"an agent before any head: {entry}"
            groups[-1][2].append(entry["tab"])
    return groups


def test_grouped_heads_each_project_with_its_count(roost, project, project2, owned_config):
    tabs = _seed_two_projects(roost, project, project2)
    _set_sort(roost, "grouped")
    panel = _wait_listed(roost, _keys(tabs))
    assert all(e["location_text"] == "" for e in _agents(panel)), panel
    multi_host = len(roost.sidebar_sections()) > 1

    def head(pid: int) -> str:
        name = roost.project(pid)["name"]
        return f"local{SEPARATOR}{name}" if multi_host else name

    groups = _groups(panel)
    labels = [label for label, _, _ in groups]
    for pid, ours in (
        (project, [tabs["working"], tabs["finished"]]),
        (project2, [tabs["failed"], tabs["waiting"]]),
    ):
        label, count, listed = groups[labels.index(head(pid))]
        # Inside a project: most urgent first.
        assert listed == [str(tab) for tab in ours], (label, listed)
        assert count == len(ours), (label, count)
    assert labels.index(head(project)) < labels.index(head(project2)), labels


def test_recent_lists_the_newest_event_first(roost, project, project2, owned_config):
    """The least urgent agent reports again a second later, so `recent`
    puts first the agent `priority` puts last; ties in a second fall
    through to the priority order."""
    tabs: dict[str, int] = {}
    session = uuid.uuid4().hex
    sources: dict[str, str] = {}
    for where, lifecycle in ((project, "finished"), (project2, "failed"), (project2, "working")):
        tab = roost.open_tab(where, cwd="/tmp", argv=BARE_SHELL_ARGV)
        sources[lifecycle] = _seed(
            roost, tab, lifecycle=lifecycle, name=f"recent-{lifecycle}", session=session
        )
        tabs[lifecycle] = tab
    keys = _keys(tabs)
    _set_sort(roost, "recent")
    _wait_listed(roost, keys)

    newest = max(roost.ownership(tab)["last_event_at"] for tab in tabs.values())
    roost._wait(lambda: int(time.time()) > int(newest), 3.0, "the clock to pass the seeds' second")
    roost.agent_report(
        tabs["finished"], sources["finished"], "preserve", session_id=session, lifecycle="finished"
    )
    at = {key: roost.ownership(int(key))["last_event_at"] for key in keys}
    assert at[str(tabs["finished"])] > max(
        stamp for key, stamp in at.items() if key != str(tabs["finished"])
    ), at

    priority = [str(tabs[lifecycle]) for lifecycle in ("failed", "working", "finished")]
    expected = sorted(keys, key=lambda key: (-at[key], priority.index(key)))
    assert expected[0] == str(tabs["finished"])
    roost._wait(
        lambda: _order(_panel(roost), keys) == expected,
        5.0,
        f"recent order {expected}",
    )


def test_hiding_the_panel_keeps_its_entries(roost, project, owned_config):
    tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    _seed(roost, tab, lifecycle="working", name="panel-hidden")
    _set_agents_visible(roost, True)
    _wait_listed(roost, {str(tab)})

    _set_agents_visible(roost, False)
    panel = _panel(roost)
    assert panel["visible"] is False
    assert roost.sidebar_dump()["agents_visible"] is False
    assert _agents(panel, {str(tab)}), "a hidden panel still lists its agents"

    _set_agents_visible(roost, True)
    assert _panel(roost)["visible"] is True


def test_is_active_follows_tab_focus(roost, project):
    first = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    second = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    _seed(roost, first, lifecycle="working", name="panel-first")
    _seed(roost, second, lifecycle="working", name="panel-second")
    keys = {str(first), str(second)}
    _wait_listed(roost, keys)

    for focused, other in ((first, second), (second, first)):
        roost.focus(focused)

        def active() -> bool:
            entries = {entry["tab"]: entry for entry in _agents(_panel(roost), keys)}
            return entries[str(focused)]["is_active"] and not entries[str(other)]["is_active"]

        roost._wait(active, 5.0, f"tab {focused} is the panel's active row")


def test_the_split_reports_its_share(roost):
    if not is_fresh():
        pytest.skip(
            "the default share needs a fresh harness-owned instance (--roost-fresh); "
            "a dev UI remembers whatever its divider was dragged to"
        )
    assert _panel(roost)["fraction_permille"] == 370


def test_no_agents_means_no_entries(roost):
    if not is_fresh():
        pytest.skip(
            "an empty Agents panel needs a fresh harness-owned instance "
            "(--roost-fresh); a dev UI likely has agents open"
        )
    roost._wait(lambda: _panel(roost)["entries"] == [], 10.0, "the Agents panel to empty")


def test_long_and_unicode_text_round_trips_untruncated(roost, owned_config):
    project_name = "プロジェクト-" + "長い名前-" * 6 + "end"
    pid = roost.create_project(name=project_name, cwd="/tmp")
    try:
        name = "日本語のエージェント — " + "very-long-agent-name-" * 6 + "\U0001f980"
        tab = roost.open_tab(pid, cwd="/tmp", argv=BARE_SHELL_ARGV)
        _seed(roost, tab, lifecycle="waiting", name=name)
        _set_sort(roost, "grouped")
        panel = _wait_listed(roost, {str(tab)})
        (entry,) = _agents(panel, {str(tab)})
        assert entry["name"] == name
        heads = [e["label"] for e in panel["entries"] if e["kind"] == "group"]
        assert any(label.endswith(project_name) for label in heads), heads

        _set_sort(roost, "priority")
        (entry,) = _agents(_panel(roost), {str(tab)})
        assert entry["location_text"] == _location(roost, project_name)
    finally:
        roost.delete_project(pid)


# ---------------------------------------------------------------------------
# The divider's share (plan 080 D8)
# ---------------------------------------------------------------------------

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

needs_test_mode = pytest.mark.skipif(
    not TEST_MODE,
    reason="sidebar.set_agents_fraction requires ROOST_TEST_MODE=1 in the UI's launch env",
)

_skip_wayland = skip_on_live_wayland_desktop(
    "test_the_floors_clamp_only_what_is_drawn[panel]",
    "test_the_floors_clamp_only_what_is_drawn[projects]",
)

# The workspace's clamp band (`AGENTS_PANEL_{MIN,MAX}_FRACTION`).
MIN_PERMILLE = 150
MAX_PERMILLE = 850
# The bound that presses each floor: the largest share squeezes the project
# list, the smallest the panel.
FLOOR_SHARES = {"projects": MAX_PERMILLE, "panel": MIN_PERMILLE}

# Sidebar geometry, in logical points, verbatim from `crates/roost-iced/
# src/chrome.rs`: the divider's height and fill, the band a project list
# keeps at least two rows under, and the footer below everything.
DIVIDER_HEIGHT = 6
DIVIDER_FILL = (0x1A, 0x1D, 0x1E)
BAND_HEIGHT = 32
ROW_HEIGHT = 32
# `agents_split.rs`'s `PROJECTS_FLOOR`.
PROJECTS_FLOOR = BAND_HEIGHT + 2 * ROW_HEIGHT
# `app/agents_panel.rs`'s `panel_height` under `grouped`: the header, the
# list padding, a group head and one whole agent row.
GROUPED_PANEL_FLOOR = BAND_HEIGHT + 3 + 27 + 42
# The panel's header (`AGENT_PANEL_HEADER_HEIGHT`), which stays even where
# the project list's floor leaves less.
PANEL_HEADER = BAND_HEIGHT
FOOTER_HEIGHT = 44
# A column through the divider's fill, clear of its centered nub.
PROBE_X = 40


@pytest.fixture
def kept_share(roost, target):
    """The panel's share as this test found it, put back afterwards —
    through a client of its own, since the relaunch case outlives the
    `roost` one."""
    before = _permille(roost)
    yield before
    with Roost(ui.socket_path(target)) as client:
        client.sidebar_set_agents_fraction(before / 1000)


def _permille(roost) -> int:
    return _panel(roost)["fraction_permille"]


def _saved_fraction() -> float | None:
    """`state.json`'s `agents_panel_fraction`, or None while it has none.
    The UI writes the file with tmp + rename, so a read never sees half."""
    state_dir = ui.session_state_dir()
    assert state_dir is not None, "needs a harness-owned instance"
    try:
        state = json.loads((state_dir / "state.json").read_text())
    except FileNotFoundError:
        return None
    return state.get("agents_panel_fraction")


def _wait_saved(roost, fraction: float) -> None:
    roost._wait(
        lambda: _saved_fraction() == fraction,
        5.0,
        f"state.json to remember agents_panel_fraction {fraction}",
    )


@needs_test_mode
def test_set_agents_fraction_reports_clamps_and_persists(roost, kept_share):
    roost.sidebar_set_agents_fraction(0.6)
    assert _permille(roost) == 600
    roost.sidebar_set_agents_fraction(0.01)
    assert _permille(roost) == MIN_PERMILLE, "an out-of-band share lands on the bound"
    roost.sidebar_set_agents_fraction(5.0)
    assert _permille(roost) == MAX_PERMILLE
    for bad in (0, -0.5):
        with pytest.raises(RoostError) as exc:
            roost.sidebar_set_agents_fraction(bad)
        assert exc.value.code == "invalid-param", exc.value
    assert _permille(roost) == MAX_PERMILLE, "a rejected share moved nothing"

    if ui.session_state_dir() is not None:
        _wait_saved(roost, MAX_PERMILLE / 1000)


@needs_test_mode
def test_hiding_the_panel_keeps_its_share(roost, owned_config, kept_share):
    _set_agents_visible(roost, True)
    roost.sidebar_set_agents_fraction(0.3)
    _set_agents_visible(roost, False)
    panel = _panel(roost)
    assert panel["visible"] is False
    assert panel["fraction_permille"] == 300
    _set_agents_visible(roost, True)
    assert _permille(roost) == 300


def _split_drawn(roost, path) -> tuple[int, int, int] | None:
    """The project list's and the panel's drawn heights, and the window's,
    in logical points, off one scale-1 capture (which is in logical
    points) — None while it is mid-relayout, or draws no divider: the
    panel is hidden or the sidebar collapsed. The
    capture, not `app.window_metrics`, says how big the window is: a
    compositor may refuse a `window.resize` the UI has already recorded.
    The divider is the only run of its fill this tall in the probe column
    (the sidebar's hairline is a column, not a run)."""
    shot = _capture(roost, path)
    if shot is None:
        return None
    _width, height, _bpp, _px = shot
    runs = [
        top
        for _x0, top, _x1, bottom in _blobs(shot, DIVIDER_FILL, x0=PROBE_X, x1=PROBE_X + 1)
        if bottom - top + 1 == DIVIDER_HEIGHT
    ]
    if not runs:
        return None
    assert len(runs) == 1, f"expected one divider run at x={PROBE_X}, got {runs}"
    top = runs[0]
    grip_top = BAND_HEIGHT if not roost.sidebar_sections() else 0
    return top - grip_top, height - FOOTER_HEIGHT - (top + DIVIDER_HEIGHT), height


def _resize(roost, path, width: float, height: int) -> None:
    """Asks for a window `height` tall, and gives the capture a moment to
    show it. A compositor may keep a height of its own instead."""
    roost.window_resize(width, height)
    try:
        sessionlib.wait_until(
            lambda: (drawn := _split_drawn(roost, path)) and drawn[2] == height,
            1.5,
            f"the split drawn at a {height}pt window",
        )
    except TimeoutError:
        pass


def _f32(value: float) -> float:
    """`value` as an f32 holds it: the UI lays the split out in f32."""
    return struct.unpack("f", struct.pack("f", value))[0]


def _expected_panel(permille: int, below: int) -> tuple[int, str | None]:
    """The panel `agents_split.rs`'s `shown_panel` draws for a stored
    `permille` share of the `below` points under the divider, and which
    floor, if either, holds it there: the share rounded, then raised to the
    panel's floor, then cut to what the project list's floor leaves."""
    wanted = math.floor(_f32(_f32(permille / 1000) * below) + 0.5)
    most = min(max(below - PROJECTS_FLOOR, PANEL_HEADER), below)
    if max(wanted, GROUPED_PANEL_FLOOR) > most:
        return most, "projects"
    if wanted < GROUPED_PANEL_FLOOR:
        return GROUPED_PANEL_FLOOR, "panel"
    return wanted, None


def _drawn_share(roost, path, permille: int) -> tuple[int, str | None]:
    """Sets a `permille` share and asserts the split it draws, exactly, at
    whatever height the window has: that height, and the floor holding the
    panel there, if one does."""
    roost.sidebar_set_agents_fraction(permille / 1000)
    # A capture can trail the op by a frame, so wait for the split this
    # share draws; the last one is judged either way.
    seen: list[tuple[int, int, int]] = []

    def settled():
        drawn = _split_drawn(roost, path)
        if drawn is None:
            return False
        seen.append(drawn)
        projects, panel, _window = drawn
        return panel == _expected_panel(permille, projects + panel)[0]

    try:
        sessionlib.wait_until(settled, 10.0, f"the split a {permille}‰ share draws")
    except TimeoutError:
        pass
    assert seen, "no capture showed the divider"
    projects, panel, window = seen[-1]
    below = projects + panel
    expected, floor = _expected_panel(permille, below)
    assert panel == expected, (
        f"a {permille}‰ share of {below}pt (a {window}pt window) drew a "
        f"{panel}pt panel, not {expected}pt (floor: {floor})"
    )
    assert _permille(roost) == permille, "the floors never touch the stored share"
    return window, floor


@needs_test_mode
@pytest.mark.parametrize("floor", sorted(FLOOR_SHARES))
def test_the_floors_clamp_only_what_is_drawn(
    roost, project, tmp_path, owned_config, kept_share, floor
):
    """At a short window, the bound share that presses `floor` leaves
    exactly the floor: a band and two rows for the project list, a head and
    one row for the panel. The stored share is untouched, and a window with
    room draws all of it again. Each case is judged at the height the
    compositor keeps, and the floor is skipped where no share reaches it
    there."""
    permille = FLOOR_SHARES[floor]
    tab = roost.open_tab(project, cwd="/tmp", argv=BARE_SHELL_ARGV)
    _seed(roost, tab, lifecycle="working", name="panel-floors")
    _wait_listed(roost, {str(tab)})
    _set_agents_visible(roost, True)
    _set_sort(roost, "grouped")
    _toggle_to_visible(roost)
    before = roost.window_metrics()
    path = tmp_path / "split.png"
    held = False
    kept = []
    try:
        for height in (480, 900):
            _resize(roost, path, before["window_width"], height)
            window, at = _drawn_share(roost, path, permille)
            kept.append(window)
            held = held or at == floor
    finally:
        roost.window_resize(before["window_width"], before["window_height"])
    if not held:
        pytest.skip(
            f"the compositor kept {kept}pt windows, where no share reaches the {floor} "
            "floor; each drew its split exactly"
        )


@needs_test_mode
def test_an_empty_panel_keeps_its_divider_at_the_share(
    roost, tmp_path, owned_config, kept_share
):
    """With no agents running, the panel keeps its divider and its height
    (plan 080 R1): the divider is drawn at the share it found, and a share
    set through the op moves it, held by the floor a panel with agents has
    — the smallest share presses it at the default window."""
    if not is_fresh():
        pytest.skip(
            "an empty Agents panel needs a fresh harness-owned instance "
            "(--roost-fresh); a dev UI likely has agents open"
        )
    _set_agents_visible(roost, True)
    _set_sort(roost, "grouped")
    _toggle_to_visible(roost)
    roost._wait(lambda: _panel(roost)["entries"] == [], 10.0, "the Agents panel to empty")
    path = tmp_path / "split.png"
    for permille in (kept_share, MIN_PERMILLE, 600):
        _drawn_share(roost, path, permille)
    assert _panel(roost)["entries"] == [], "an agent reached the panel mid-test"


@needs_test_mode
def test_the_share_survives_relaunch(roost, target, kept_share):
    if ui.owned_session_config_path() is None:
        pytest.skip(
            "quit + relaunch would close a developer's own UI; requires a "
            "harness-owned instance (--roost-fresh / ROOST_TEST_FRESH=1, or no "
            "UI already running)"
        )
    skip_on_ci(
        "quit + relaunch is unreliable on CI: iced xvfb has no WM",
        alt_coverage="Rust agents_panel_fraction_persists_across_reopen + "
        "test_set_agents_fraction_reports_clamps_and_persists",
    )
    roost.sidebar_set_agents_fraction(0.6)
    _wait_saved(roost, 0.6)

    roost.close()
    ui.quit(target)
    ui.launch(target)

    # The workspace restores it as it opens `state.json`, before the
    # socket `launch` waits on answers.
    fresh = Roost(ui.socket_path(target))
    try:
        assert _permille(fresh) == 600
    finally:
        fresh.close()

