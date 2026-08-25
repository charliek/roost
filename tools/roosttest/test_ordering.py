"""Tab/project position ordering — the dual-target parity guard for #262.

Plan 004 §3.4: ordering is an allocator over the workspace's own storage,
and its result (`position`) is directly observable through `tab.list` on
both UIs already. So the parity check for the Swift/Rust divergence in
#262 — Swift handed out `position` from `count` instead of `max+1`, so a
mid-list close produced duplicate positions, and closing from the front
put a brand-new tab to the *left* of a tab that predates it — is this
file: one dual-target e2e (`--roost-target mac|iced`, both required CI
gates) instead of a hand-written golden-fixture corpus (§2.4: no generic
loader exists, and a new group would cost ~250-650 lines for something
already reachable through the op set). Behavioral coverage against the
real op surface, rather than a duplicated golden-fixture corpus, is the
parity strategy for anything reachable as an op.

What this file pins is the **observable invariant** — positions unique
within a parent, and display order matching creation order — not the
`max+1` rule itself. An allocator handing out `max+2` would pass every
test here, and that is deliberate: §3.1 makes uniqueness the invariant
and treats gaps as legal, so the rule is pinned where it is written, by
`positionIsMaxPlusOneAfterDelete` (Swift) and
`position_is_max_plus_one_after_delete` (Rust). Two altitudes, one
behavior.

Two hazards this file has to respect:

* The UI fixture is **session-scoped** (`conftest.py:59`) — other tests'
  projects and tabs are already sitting in the workspace when this file
  runs. Positions are `max+1` across the *whole* workspace, so we only
  ever assert uniqueness/ordering **relative to the rows this test
  created**, never an absolute position value.
* On Mac, closing a project's **last** tab cascades the project away
  (`Workspace.swift` `closeTab`). Every tab test below leaves at least
  one tab alive at all times so the fixture project survives the test.
"""

from __future__ import annotations

import uuid


def test_tab_close_mid_list_then_open_keeps_unique_positions(roost, project):
    """Open three, close the middle one, open a fourth.

    Pre-#262-fix, Swift allocated `position` from the sibling *count*, so
    closing the middle tab and opening a new one reused a position still
    held by a survivor. Positions in the project must stay unique, and
    the newly opened tab — having no reason to sort anywhere else — must
    land last in `tab.list` display order.
    """
    t1 = roost.open_tab(project, cwd="/tmp")
    t2 = roost.open_tab(project, cwd="/tmp")
    t3 = roost.open_tab(project, cwd="/tmp")
    roost.close_tab(t2)  # mid-list close; t1 and t3 stay alive
    t4 = roost.open_tab(project, cwd="/tmp")

    ids = roost.project_tab_ids(project)
    assert set(ids) == {t1, t3, t4}, ids

    positions = [roost.tab(tid)["position"] for tid in ids]
    assert len(set(positions)) == len(positions), (
        f"duplicate positions after a mid-list close: {list(zip(ids, positions, strict=True))}"
    )
    assert ids[-1] == t4, f"newest tab must sort last in display order, got {ids}"


def test_tab_close_front_then_open_sorts_new_after_survivor(roost, project):
    """Open three, close the *first two*, open a fourth — the user-visible
    symptom (#262): "I was asked for input on the third tab and the
    second tab turned orange."

    Pre-fix, closing from the front dropped the sibling count *below* the
    surviving tab's position, so the newly opened tab was allocated a
    position that sorted before the survivor — producing display order
    `[(new, 1), (survivor, 2)]`. No collision is even required for this
    one; `count < max+1` is enough. The new tab must sort *after* the
    survivor.
    """
    t1 = roost.open_tab(project, cwd="/tmp")
    t2 = roost.open_tab(project, cwd="/tmp")
    t3 = roost.open_tab(project, cwd="/tmp")
    roost.close_tab(t1)
    roost.close_tab(t2)  # front close; t3 stays alive throughout
    t4 = roost.open_tab(project, cwd="/tmp")

    ids = roost.project_tab_ids(project)
    assert set(ids) == {t3, t4}, ids
    assert ids.index(t4) > ids.index(t3), (
        f"new tab {t4} must sort after survivor {t3} in display order, got {ids}"
    )


def test_project_delete_mid_list_then_create_keeps_unique_positions(roost):
    """Create three throwaway projects, delete the middle one, create a
    fourth. Same defect as the tab case (§2.1b), one level up: Swift's
    project position was also `count`-derived.

    Projects, unlike tabs, aren't scoped by a single `project` fixture,
    so this test creates and tears down its own — cascade-cleaned in a
    `finally` the way the `project` fixture cleans its own row.
    """
    stem = uuid.uuid4().hex[:8]
    created: list[int] = []
    try:
        p1 = roost.create_project(name=f"pytest-ord-{stem}-1", cwd="/tmp")
        created.append(p1)
        p2 = roost.create_project(name=f"pytest-ord-{stem}-2", cwd="/tmp")
        created.append(p2)
        p3 = roost.create_project(name=f"pytest-ord-{stem}-3", cwd="/tmp")
        created.append(p3)

        roost.delete_project(p2)  # mid-list delete
        created.remove(p2)

        p4 = roost.create_project(name=f"pytest-ord-{stem}-4", cwd="/tmp")
        created.append(p4)

        by_id = {int(p["id"]): p for p in roost.list()}
        ours = [p1, p3, p4]
        positions = [by_id[pid]["position"] for pid in ours]

        assert len(set(positions)) == len(positions), (
            f"duplicate project positions: {list(zip(ours, positions, strict=True))}"
        )
        assert positions == sorted(positions), (
            f"project creation order not preserved: {list(zip(ours, positions, strict=True))}"
        )
    finally:
        for pid in created:
            try:
                roost.delete_project(pid)
            except Exception:
                pass  # already gone, or never fully created
