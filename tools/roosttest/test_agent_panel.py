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

Condition waits only — except the one read the cycling case makes
straight after the palette activation, which is the point of it.
"""

from __future__ import annotations

import time
import uuid

import pytest

import session as sessionlib
import ui
from test_agent_palette import _seed
from test_sidebar_agents import _palette_agent_tab_ids, _set_agents_visible
from util import BARE_SHELL_ARGV, is_fresh, wait_for_config_line

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
        return f"LOCAL{SEPARATOR}{name}" if multi_host else name

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
    assert _panel(roost)["fraction_permille"] == 450


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
