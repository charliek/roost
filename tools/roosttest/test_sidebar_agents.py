"""Sidebar agent rows E2E — plan 007 §7 A9/A10, both targets
(`--roost-target mac|gtk`). Drives `app.sidebar_dump` (plan 007 §3.8),
the same per-project `rendered_agents` cache both UIs paint the sidebar
from, so a missed refresh is a stale dump rather than an invisible bug.

Isolation rules mirror `test_agent_palette.py`: assertions are scoped to
projects/tabs THIS test seeded, never to the dump's absolute row list —
a dev UI driving the same instance may have other agents live. Each
project this file creates is asserted on individually (`_project_row`).

No `time.sleep`: every assertion polls via `roost._wait`. This matters
more here than in the palette tests — the GTK refresh path
(`refresh_agent_rows`) bails out soft on a re-entrant
`ui.tabs.try_borrow()` failure (plan 007 §3.5/§9 R5), so a dropped
refresh is a flaky single read, not a deterministic one. A single-shot
assert right after a mutating call would intermittently see the
pre-refresh cache; polling absorbs the retry the refresh itself doesn't
guarantee.

`time_text` is elapsed-derived (`elapsed_text()`) and drifts second to
second between a seed and a read, so it is checked only against the
shared `TIME_TEXT_RE` shape from `test_agent_palette.py`, never for an
exact value.
"""

from __future__ import annotations

import uuid

from test_agent_palette import TIME_TEXT_RE, _seed


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _project_row(dump: dict, project_id: int) -> dict | None:
    return next(
        (p for p in dump["projects"] if int(p["project_id"]) == project_id), None
    )


def _agent_row(dump: dict, project_id: int, tab_id: int) -> dict | None:
    proj = _project_row(dump, project_id)
    if proj is None:
        return None
    return next((a for a in proj["agents"] if int(a["tab_id"]) == tab_id), None)


def _wait_agent_row(roost, project_id: int, tab_id: int, timeout: float = 5.0) -> dict:
    box: dict = {"row": None}

    def pred() -> bool:
        row = _agent_row(roost.sidebar_dump(), project_id, tab_id)
        box["row"] = row
        return row is not None

    roost._wait(pred, timeout, f"sidebar_dump row for tab {tab_id} in project {project_id}")
    return box["row"]


def _wait_agent_row_gone(roost, project_id: int, tab_id: int, timeout: float = 5.0) -> None:
    roost._wait(
        lambda: _agent_row(roost.sidebar_dump(), project_id, tab_id) is None,
        timeout,
        f"sidebar_dump row for tab {tab_id} in project {project_id} disappears",
    )


def _toggle_sidebar_agents(roost) -> dict:
    """Drive the toggle through the command palette, the same way a user
    or `roostctl` would (plan 007 §3.7 — `palette.activate` is the one
    reachable-from-everywhere lane). Leaves the palette closed."""
    roost.palette_dismiss()
    roost.palette_open()
    st = roost.palette_activate("toggle_sidebar_agents")
    roost.palette_dismiss()
    return st


def _agents_visible(roost) -> bool:
    return bool(roost.sidebar_dump()["agents_visible"])


def _set_agents_visible(roost, want: bool) -> None:
    """Toggle until `agents_visible` matches `want`, restoring the
    instance to a known state (used at test start/end so this file never
    leaves the toggle flipped for whatever runs next)."""
    if _agents_visible(roost) == want:
        return
    _toggle_sidebar_agents(roost)
    roost._wait(
        lambda: _agents_visible(roost) == want,
        5.0,
        f"agents_visible becomes {want}",
    )


# ---------------------------------------------------------------------------
# A9: population, ordering, content
# ---------------------------------------------------------------------------


def test_seeded_agents_appear_under_their_project_in_palette_order(roost, project):
    """Two agents in one project, ranked opposite their seed order
    (failed outranks working — `rank()`'s urgency order), plus a second
    project with its own single agent. Asserts project grouping, the
    palette's ordering (rank desc, then recency), and name/lifecycle/
    status_text per row."""
    p2 = roost.create_project(name=f"pytest-sidebar-agents2-{uuid.uuid4().hex[:6]}", cwd="/tmp")
    try:
        t_working = roost.open_tab(project, cwd="/tmp")
        t_failed = roost.open_tab(project, cwd="/tmp")
        t_other = roost.open_tab(p2, cwd="/tmp")

        _seed(roost, t_working, lifecycle="working", name="sb-working")
        _seed(roost, t_failed, lifecycle="failed", name="sb-failed", detail="rate_limit")
        _seed(roost, t_other, lifecycle="waiting", name="sb-other")

        def _ready() -> bool:
            dump = roost.sidebar_dump()
            return (
                _agent_row(dump, project, t_working) is not None
                and _agent_row(dump, project, t_failed) is not None
                and _agent_row(dump, p2, t_other) is not None
            )

        roost._wait(_ready, 5.0, "all three seeded rows appear in the sidebar dump")

        dump = roost.sidebar_dump()
        proj1 = _project_row(dump, project)
        proj2 = _project_row(dump, p2)
        assert proj1 is not None and proj2 is not None

        # Ordering within project 1: failed outranks working, matching
        # the agent palette's rank() ordering (plan 007 §3.1/§3.3).
        ids_in_proj1 = [int(a["tab_id"]) for a in proj1["agents"]]
        seen = [t for t in ids_in_proj1 if t in {t_working, t_failed}]
        assert seen == [t_failed, t_working], seen

        # project 2's agent must not leak into project 1's list.
        assert t_other not in ids_in_proj1

        row_working = _agent_row(dump, project, t_working)
        row_failed = _agent_row(dump, project, t_failed)
        row_other = _agent_row(dump, p2, t_other)

        assert row_working["name"] == "sb-working", row_working
        assert row_working["lifecycle"] == "working", row_working
        assert row_working["status_text"] == "Working", row_working
        assert TIME_TEXT_RE.match(row_working["time_text"]), row_working["time_text"]

        assert row_failed["name"] == "sb-failed", row_failed
        assert row_failed["lifecycle"] == "failed", row_failed
        assert row_failed["status_text"] == "Failed · rate_limit", row_failed

        assert row_other["name"] == "sb-other", row_other
        assert row_other["lifecycle"] == "waiting", row_other
        assert row_other["status_text"] == "Waiting for input", row_other
    finally:
        roost.delete_project(p2)


def test_project_with_zero_agents_still_appears_with_empty_list(roost, project):
    """A9's zero-agent-project case: the project row is present with an
    empty `agents` list, not simply absent from the dump (plan 007 §3.8
    — "all projects appear ... including those with zero agents")."""

    def _present() -> bool:
        return _project_row(roost.sidebar_dump(), project) is not None

    roost._wait(_present, 5.0, "empty project appears in sidebar_dump")
    proj = _project_row(roost.sidebar_dump(), project)
    assert proj is not None
    assert proj["agents"] == [], proj


# ---------------------------------------------------------------------------
# A3/A9: exactly one active row, following tab.focus
# ---------------------------------------------------------------------------


def test_exactly_one_row_is_active_and_follows_tab_focus(roost, project):
    tab_a = roost.open_tab(project, cwd="/tmp")
    tab_b = roost.open_tab(project, cwd="/tmp")
    _seed(roost, tab_a, lifecycle="working", name="sb-active-a")
    _seed(roost, tab_b, lifecycle="working", name="sb-active-b")

    roost.focus(tab_a)

    def _active_matches(want_tab: int) -> bool:
        dump = roost.sidebar_dump()
        row_a = _agent_row(dump, project, tab_a)
        row_b = _agent_row(dump, project, tab_b)
        if row_a is None or row_b is None:
            return False
        active = [t for t, r in ((tab_a, row_a), (tab_b, row_b)) if r["is_active"]]
        return active == [want_tab]

    roost._wait(lambda: _active_matches(tab_a), 5.0, "sidebar active row follows tab_a")

    # `is_active` mirrors the single globally-active tab (plan 007 §3.3),
    # so across the WHOLE dump — not just our two seeded rows — at most
    # one row may be active, and it must be ours.
    all_active = [
        int(a["tab_id"])
        for p in roost.sidebar_dump()["projects"]
        for a in p["agents"]
        if a["is_active"]
    ]
    assert all_active == [tab_a], all_active

    roost.focus(tab_b)
    roost._wait(lambda: _active_matches(tab_b), 5.0, "sidebar active row follows tab_b")


# ---------------------------------------------------------------------------
# A6/§3.7: toggle via palette.activate, rows stay populated while hidden
# ---------------------------------------------------------------------------


def test_toggle_flips_agents_visible_and_keeps_rows_populated_when_off(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    _seed(roost, tab, lifecycle="waiting", name="sb-toggle")
    _wait_agent_row(roost, project, tab)

    before = _agents_visible(roost)
    try:
        _toggle_sidebar_agents(roost)
        roost._wait(
            lambda: _agents_visible(roost) == (not before),
            5.0,
            "agents_visible flips after toggle_sidebar_agents",
        )
        # Rows stay populated when the toggle is off — plan 007 §3.8:
        # "projects[].agents stays populated when the toggle is off".
        row = _wait_agent_row(roost, project, tab)
        assert row["name"] == "sb-toggle", row
    finally:
        # Restore to what the instance had before this test, regardless
        # of outcome, so this test never leaves the toggle flipped for
        # whatever runs next.
        _set_agents_visible(roost, before)

    assert _agents_visible(roost) == before


# ---------------------------------------------------------------------------
# A10: rows disappear on non-live / tab close, no stale entries
# ---------------------------------------------------------------------------


def test_row_disappears_when_agent_releases_ownership(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    session = uuid.uuid4().hex
    source = _seed(roost, tab, lifecycle="working", name="sb-release", session=session)
    _wait_agent_row(roost, project, tab)

    # `release` must carry the SAME (source, session_id) the claim used —
    # `owner_matches` in roost-ipc/src/agent.rs rejects a release whose
    # identity doesn't match the current owner, same as `preserve`.
    roost.agent_report(tab, source, "release", session_id=session)
    _wait_agent_row_gone(roost, project, tab)

    # The tab itself is unaffected — only the agent row vanishes.
    assert roost.tab(tab) is not None


def test_row_disappears_when_tab_closes(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    _seed(roost, tab, lifecycle="waiting", name="sb-close")
    _wait_agent_row(roost, project, tab)

    roost.close_tab(tab)
    _wait_agent_row_gone(roost, project, tab)


def test_row_disappears_when_project_deleted(roost):
    p = roost.create_project(name=f"pytest-sidebar-agents-del-{uuid.uuid4().hex[:6]}", cwd="/tmp")
    tab = roost.open_tab(p, cwd="/tmp")
    _seed(roost, tab, lifecycle="waiting", name="sb-project-delete")
    _wait_agent_row(roost, p, tab)

    roost.delete_project(p)

    def _project_gone() -> bool:
        return _project_row(roost.sidebar_dump(), p) is None

    roost._wait(_project_gone, 5.0, "deleted project's row leaves the sidebar dump")
