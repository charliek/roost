"""Project lifecycle E2E — create/delete/reorder over the raw IPC op set.

Plan 010 C4. Most assertions are **target-agnostic parity checks**: they
drive `project.create` / `project.delete` / `project.reorder` directly
(no palette, no keybind) and assert on `tab.list` / `identify` /
`app.sidebar_dump`, so the same test runs against mac/iced and pins
op-level behavior both engines share. A couple of tests are
Iced-only because they exercise UI-flow behavior that has no cross-target
analog (the full palette-dispatch path) or that diverges by UI (Mac quits
the whole process when the last project's window closes — see
`Workspace.deleteProject`/App.swift's
`applicationShouldTerminateAfterLastWindowClosed`).

Nothing here empties the workspace. Iced now exits when the last project
goes (plan 026 D8, mac parity), and this suite shares ONE session-scoped
UI with every other module in the invocation — an exit mid-suite would
strand them. The last-project case therefore lives in
`test_exit_on_empty.py`, which runs in its own pytest invocation
(Makefile `e2e-iced-exit`) against its own instance.

Engine references (verified this session, see plan 010 §2):
- `project.create` never changes the active selection on ANY UI — the
  Rust engine's `create_project` (workspace.rs:636) only emits
  `ProjectCreated`, and the Mac `.projectCreated` arm only calls
  `insertProjectLocallyIfMissing` (App.swift). Mirrors the note already
  pinned in `test_sidebar_collapse_persistence.py:180-183`. Activation is
  UI-flow behavior (iced's `new_project()` does an explicit
  `focus_tab`), not raw-op behavior.
- `project.delete`'s active-fallback pick differs by engine: the Rust
  workspace (Iced) falls back to `projects.keys().next()` — the
  lowest remaining project id, a BTreeMap (workspace.rs:714). Mac's
  `Workspace.deleteProject` picks the first project in DISPLAY order
  (`(position, id)` sort, Workspace.swift:320-326 — deliberately not by
  id, per a PR #78 CodeRabbit finding). So the cross-target assertion
  here is "active is some remaining project," never "active is the
  lowest id."
- `project.reorder`'s partial-list semantics (listed ids as a prefix in
  the given order, unlisted ids appended after in their PRIOR relative
  order) are the same on the Rust engine (workspace.rs:1317-1358,
  pinned by `reorder_projects_appends_unlisted_by_position_then_id`) and
  on Mac (`Workspace.swift:385-405`), so that shape IS asserted
  cross-target — against the FULL project list, not just our own ids,
  so a misplacement relative to a pre-existing project would fail too.

Every assertion polls the surface it reads (tab.list / sidebar_dump /
identify are independently-refreshed views — sidebar_dump in particular
lags tab.list by one UI tick), never piggybacks on a wait against a
different surface.
"""

from __future__ import annotations

import re

import pytest

from client import RoostError


def _project_ids(roost) -> list[int]:
    return [int(p["id"]) for p in roost.list()]


def _cleanup_project(roost, project_id: int, timeout: float = 5.0) -> None:
    """Best-effort teardown delete.

    Tolerates only `not-found` (a prior step in the test already deleted
    it) — any other error code is a real regression and must propagate,
    not be swallowed. On a successful delete, waits for the project to
    actually disappear from `tab.list` before returning, so a caller that
    assumes "this id is gone now" (e.g. the next cleanup call, or a
    length assertion right after) isn't racing the delete's cascade.
    """
    try:
        roost.delete_project(project_id)
    except RoostError as e:
        if e.code != "not-found":
            raise
        return  # already gone
    roost._wait(
        lambda: roost.project(project_id) is None,
        timeout,
        f"project {project_id} disappears after cleanup delete",
    )


# -- project.create -------------------------------------------------------


def test_project_create_appears_untitled_and_does_not_activate(roost):
    """An empty-name `project.create` shows up with an engine-chosen
    "Untitled N" name and leaves the active selection untouched — that's
    `new_project()`'s explicit `focus_tab` (a UI-flow decision), not
    behavior of the raw op itself."""
    before_active = roost.identify()["active_project_id"]

    pid = roost.create_project(name="", cwd="")
    try:
        roost._wait(
            lambda: pid in _project_ids(roost),
            4.0,
            "project.create appears in tab.list",
        )
        proj = roost.project(pid)
        assert proj is not None
        assert re.match(r"^Untitled \d+$", proj["name"]), proj["name"]

        # Also present in the sidebar snapshot — "all projects appear,
        # including ones with zero agents" (roost-ipc's SidebarDumpProject
        # contract; no `name` field there, so tab.list carries the name
        # assertion above and this just pins membership). sidebar_dump is
        # a UI-side cache refreshed on a tick, independent of tab.list, so
        # it gets its own poll rather than reusing the tab.list wait above.
        roost._wait(
            lambda: str(pid) in {p["project_id"] for p in roost.sidebar_dump()["projects"]},
            4.0,
            "sidebar_dump reflects the newly created project",
        )

        assert roost.identify()["active_project_id"] == before_active, (
            "project.create must not change the active selection — mirrors "
            "test_sidebar_collapse_persistence.py's note that the Mac "
            "`.projectCreated` arm only inserts locally without switching "
            "active; the Rust engine's create_project likewise only emits "
            "ProjectCreated"
        )
    finally:
        _cleanup_project(roost, pid)


# -- project.delete --------------------------------------------------------


def test_project_delete_cascades_tabs_and_active_falls_back_to_remaining(roost):
    """Deleting a project with live tabs removes the project AND its tabs
    from `tab.list`, and the active selection falls back to some
    remaining project.

    Deliberately does NOT assert "the lowest remaining id" — the Rust
    engine picks lowest-id (BTreeMap `.keys().next()`), Mac picks first
    in display order (position, id); both are legitimate per-engine
    fallbacks, so the cross-target bar is "some remaining project."
    """
    keep = roost.create_project(name="", cwd="/tmp")
    doomed = roost.create_project(name="", cwd="/tmp")
    try:
        doomed_tabs = [
            roost.open_tab(doomed, cwd="/tmp"),
            roost.open_tab(doomed, cwd="/tmp"),
        ]
        # Focus a tab in the doomed project so it's the active one —
        # exercises the fallback path, not just an incidental delete.
        roost.focus(doomed_tabs[0])
        roost._wait(
            lambda: roost.identify()["active_project_id"] == doomed,
            4.0,
            "focus makes the doomed project active",
        )

        before_ids = set(_project_ids(roost))
        assert {keep, doomed} <= before_ids

        roost.delete_project(doomed)

        roost._wait(
            lambda: doomed not in _project_ids(roost),
            5.0,
            "project.delete removes the project from tab.list",
        )
        for tab_id in doomed_tabs:
            roost.wait_gone(tab_id)

        after_ids = set(_project_ids(roost))
        assert after_ids == before_ids - {doomed}, (
            "deleting one project must not disturb any other project"
        )

        # `identify` is its own surface (the active selection can settle
        # after the cascade above has already landed) — poll it directly
        # rather than assuming it's already consistent.
        roost._wait(
            lambda: roost.identify()["active_project_id"] in after_ids,
            5.0,
            "active project falls back to SOME remaining project after delete",
        )
    finally:
        _cleanup_project(roost, doomed)
        _cleanup_project(roost, keep)


# -- project.reorder -------------------------------------------------------


def test_project_reorder_full_list_matches_requested_order(roost):
    """A full `project_ids` list (every project currently known, not just
    the three under test) rewrites `tab.list`'s order to match exactly.

    Passing the COMPLETE id set (pre-existing ids + the three created
    here) and asserting the COMPLETE resulting order — rather than
    filtering the result down to our own ids before comparing — is
    deliberate: a filtered comparison would pass even if the engine
    misplaced our ids relative to a pre-existing project, which is
    exactly the bug class a "full list" reorder test exists to catch.
    """
    existing = _project_ids(roost)
    a = roost.create_project(name="", cwd="/tmp")
    b = roost.create_project(name="", cwd="/tmp")
    c = roost.create_project(name="", cwd="/tmp")
    try:
        # Baseline: fresh creations append after `existing`, in creation
        # order — poll rather than assert immediately, since this reads
        # the same tab.list surface the reorder assertion below does and
        # must not race it.
        baseline = existing + [a, b, c]
        roost._wait(
            lambda: _project_ids(roost) == baseline,
            4.0,
            "projects settle at the creation-order baseline before reordering",
        )

        full_order = existing + [c, a, b]
        roost.reorder_projects(full_order)
        roost._wait(
            lambda: _project_ids(roost) == full_order,
            4.0,
            "project.reorder full list rewrites the complete order",
        )
    finally:
        for pid in (a, b, c):
            _cleanup_project(roost, pid)


def test_project_reorder_partial_list_prefixes_then_appends_rest(roost):
    """A partial `project_ids` list moves the listed ids to the front in
    the given order; unlisted ids — INCLUDING pre-existing ones outside
    the three created here — are appended after, keeping their prior
    relative order (both engines: workspace.rs:1317-1358,
    Workspace.swift:385-405).

    Asserts the COMPLETE resulting order (pre-existing prefix/suffix ids
    included), not a filtered view of just (a, b, c) — a filtered
    comparison can't catch the listed id landing ahead of a pre-existing
    project it shouldn't have jumped, or a pre-existing project's
    relative order being disturbed.
    """
    existing = _project_ids(roost)
    a = roost.create_project(name="", cwd="/tmp")
    b = roost.create_project(name="", cwd="/tmp")
    c = roost.create_project(name="", cwd="/tmp")
    try:
        # Baseline relative order is `existing` then creation order
        # (a, b, c) — poll it (same tab.list surface as the reorder
        # assertion below) before relying on it.
        baseline = existing + [a, b, c]
        roost._wait(
            lambda: _project_ids(roost) == baseline,
            4.0,
            "projects settle at the creation-order baseline before reordering",
        )

        roost.reorder_projects([b])
        expected = [b] + existing + [a, c]
        roost._wait(
            lambda: _project_ids(roost) == expected,
            4.0,
            "project.reorder partial list: listed first, rest appended in prior order",
        )
    finally:
        for pid in (a, b, c):
            _cleanup_project(roost, pid)


# -- Iced-only: full UI dispatch path --------------------------------------


def test_iced_palette_new_project_creates_active_project_with_one_tab(roost, target):
    """`palette.open` + `palette.activate("new_project")` drives the full
    UI dispatch path (`new_project()`), unlike the raw `project.create`
    op tested above: it seeds one shell tab AND activates the project.
    Iced-only — mac has its own native create affordances (footer
    button / menu), not a cross-target op-level behavior."""
    if target != "iced":
        pytest.skip(
            "palette-driven new_project exercises iced's UI dispatch path "
            "(new_project()); mac creates through its own native "
            "affordances, not this op sequence"
        )

    before_ids = set(_project_ids(roost))
    roost.palette_open(kind="commands")
    state = roost.palette_activate("new_project")
    assert state["open"] is False, "new_project confirms + closes the palette"

    roost._wait(
        lambda: set(_project_ids(roost)) - before_ids,
        5.0,
        "palette new_project creates a project",
    )
    new_ids = set(_project_ids(roost)) - before_ids
    assert len(new_ids) == 1, f"expected exactly one new project, got {new_ids}"
    pid = next(iter(new_ids))

    try:
        # The seeded tab and the activation are each their own surface
        # (tab.list membership vs. identify) — poll both independently
        # rather than assuming they land in the same tick as creation.
        roost._wait(
            lambda: len((roost.project(pid) or {"tabs": []})["tabs"]) == 1,
            5.0,
            "new_project seeds exactly one shell tab",
        )
        roost._wait(
            lambda: roost.identify()["active_project_id"] == pid,
            4.0,
            "new_project activates the created project",
        )
    finally:
        _cleanup_project(roost, pid)


def test_iced_deleting_a_project_keeps_the_remaining_workspace_live(roost, target):
    """Deleting projects down to the LAST remaining one leaves the UI
    running and serviceable — the counterpart to `test_exit_on_empty.py`,
    which owns the last-project case (the app exits there, so it cannot be
    asserted from inside this shared-session suite).

    Iced-only: this walks the same delete path the exit policy hangs off,
    and pins that the exit is gated on the workspace becoming EMPTY rather
    than on any project deletion. Mac terminates on the last project's
    window close (the now-removed GTK UI kept its empty-workspace state
    instead — recorded divergence).
    """
    if target != "iced":
        pytest.skip(
            "pins iced's exit-on-empty gate (empty workspace, not any "
            "delete); mac terminates on the last window close instead"
        )

    a = roost.create_project(name="pytest-live-a", cwd="/tmp")
    b = roost.create_project(name="pytest-live-b", cwd="/tmp")
    try:
        roost._wait(
            lambda: {a, b} <= set(_project_ids(roost)),
            4.0,
            "both throwaway projects appear",
        )
        _cleanup_project(roost, a)
        _cleanup_project(roost, b)

        # Projects remain (this suite never empties the workspace), so the
        # UI must still be answering — no exit, no cascade past the delete.
        assert _project_ids(roost), "the shared session must keep a project"
        roost._wait(
            lambda: a not in _project_ids(roost) and b not in _project_ids(roost),
            5.0,
            "the deleted projects are gone from the workspace snapshot",
        )
        assert roost.identify()["active_project_id"] in _project_ids(roost), (
            "active selection falls back to a remaining project"
        )
    finally:
        for pid in (a, b):
            _cleanup_project(roost, pid)
