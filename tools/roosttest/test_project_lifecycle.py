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
- `project.delete` and `tab.close` move the active selection by ONE rule
  on both engines (plan 069 §3.1), and only when the selection pointed at
  what went away. The project is gone → the nearest surviving project
  ABOVE it in sidebar order (`(position, id)`), else the nearest below,
  skipping any project with no tabs (landing there would seat the
  selection over a blank pane); within it, its first tab in display
  order. The project survives → the nearest surviving tab to the RIGHT
  of the closed one, else the nearest to its left. The asymmetry is
  deliberate. So the assertions here name the neighbour, not "some
  remaining project" — but they COMPUTE it from the order the app just
  reported, because this suite shares one session that already holds
  projects it did not create.
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

import json
import os
import re
import subprocess
import uuid

import pytest

import ui
from client import RoostError, scaled_timeout
from util import roostctl_path, wait_shell_ready, wait_tab_attached


def _project_ids(roost) -> list[int]:
    return [int(p["id"]) for p in roost.list()]


def _neighbour_project(projects: list[dict], doomed: int) -> int:
    """Plan 069 §3.1 rule 2 applied to the project list the app just
    reported: the nearest survivor ABOVE `doomed`, else the nearest
    below, skipping any project with no tabs, and falling through to the
    nearest row of any kind when every survivor is tabless.

    Computed rather than written down because this suite drives a shared
    session whose other projects — their count, their order and whether
    they hold tabs — belong to whoever launched it.
    """
    ids = [int(p["id"]) for p in projects]
    index = ids.index(doomed)
    candidates = list(reversed(projects[:index])) + projects[index + 1:]
    assert candidates, "the workspace must keep a project besides the doomed one"
    with_tabs = [int(p["id"]) for p in candidates if p["tabs"]]
    return with_tabs[0] if with_tabs else int(candidates[0]["id"])


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


# -- project.ensure -------------------------------------------------------


def test_project_ensure_creates_once_then_finds_without_activating(roost, target):
    """A missing name is created at `cwd`; asking again, with no `cwd`,
    answers that same project with `created: false` and its cwd as it
    was. Neither call moves the active selection."""
    if target == "mac":
        pytest.skip("the Mac app has no project.ensure: it answers unknown-op")

    with pytest.raises(RoostError) as refused:
        roost.ensure_project("  ", cwd="/tmp")
    assert refused.value.code == "invalid-param", refused.value

    name = f"pytest-ensure-{uuid.uuid4().hex[:8]}"
    before_active = roost.identify()["active_project_id"]
    made = roost.ensure_project(name, cwd="/tmp")
    pid = int(made["project"]["id"])
    try:
        assert made["created"] is True, made

        found = roost.ensure_project(name)
        assert found["created"] is False, found
        assert int(found["project"]["id"]) == pid
        assert found["project"]["cwd"] == "/tmp"

        assert [int(p["id"]) for p in roost.list() if p["name"] == name] == [pid]
        assert roost.identify()["active_project_id"] == before_active
    finally:
        _cleanup_project(roost, pid)


# -- `roostctl open` (plan 066 §3.2, C10) -----------------------------------


def _open(target: str, *args: str, timeout: float = 30.0) -> subprocess.CompletedProcess:
    """`roostctl --socket <target's socket> --json open <args…>`, run as a
    subprocess — `open` is a CLI-level composition (`project.ensure` then
    `tab.open`), not a raw IPC op `client.Roost` can drive directly.
    `--json` goes before `open`: after a `-- cmd…` it would be part of the
    tab's command."""
    argv = [
        roostctl_path(),
        "--socket",
        str(ui.socket_path(target)),
        "--json",
        "open",
        *args,
    ]
    return subprocess.run(
        argv, capture_output=True, text=True, timeout=scaled_timeout(timeout)
    )


def test_open_finds_or_creates_the_project_then_opens_a_tab(roost, target):
    """`open` is `project.ensure` + `tab.open` in one call (#221's
    fix): a first call creates the project and a tab; a second call with
    the same name reuses the project (`created: false`, same id) and adds
    a second tab. `open` itself never raises a follow-up `tab.focus`
    unless `--focus` is given (which this test never passes) — but
    `tab.open` "steals" the active selection on its own unless it is sent
    `activate: false` (`Workspace::open_tab`; `--no-activate`, which this
    test never passes either), the same way the plain `tab open` verb
    does, so each `open` call here still leaves the newly opened tab
    active. That is existing `tab.open` behavior, not something this verb
    changes.

    Target-gated the same way C4's `project.ensure` case is: the Mac app
    has no `project.ensure` yet, so `open` must **degrade** there rather
    than falling back to a list-then-create race — exit 1 `unsupported`
    under `--json`, project count unchanged. That degradation is asserted
    here rather than skipped, per plan 066's automated Mac-degradation
    check; `make e2e-mac` collects this module against the Swift app."""
    name = f"pytest-open-{uuid.uuid4().hex[:8]}"
    before_count = len(roost.list())
    argv = ["--project", name, "--cwd", "/tmp", "--", "sh", "-c", "echo hi; exec sleep 30"]

    first = _open(target, *argv)

    if target == "mac":
        assert first.returncode == 1, first.stdout + first.stderr
        error = json.loads(first.stderr)
        assert error["error"]["code"] == "unsupported", first.stderr
        assert len(roost.list()) == before_count, "unsupported must not create a project"
        return

    assert first.returncode == 0, first.stdout + first.stderr
    made = json.loads(first.stdout)
    assert made["created"] is True, made
    pid = int(made["project"]["id"])
    tab1 = int(made["tab"]["id"])
    try:
        roost._wait(
            lambda: roost.project(pid) is not None,
            4.0,
            "open's project appears in tab.list",
        )
        assert tab1 in roost.project_tab_ids(pid)
        roost._wait(
            lambda: roost.identify()["active_tab_id"] == tab1,
            4.0,
            "tab.open's own steal-the-active-selection makes tab1 active",
        )

        second = _open(target, *argv)
        assert second.returncode == 0, second.stdout + second.stderr
        found = json.loads(second.stdout)
        assert found["created"] is False, found
        assert int(found["project"]["id"]) == pid
        tab2 = int(found["tab"]["id"])
        assert tab2 != tab1

        roost._wait(
            lambda: tab2 in roost.project_tab_ids(pid),
            4.0,
            "open's second tab appears in tab.list",
        )
        assert {tab1, tab2} <= set(roost.project_tab_ids(pid))
        roost._wait(
            lambda: roost.identify()["active_tab_id"] == tab2,
            4.0,
            "the second open's tab.open likewise becomes active",
        )
    finally:
        _cleanup_project(roost, pid)


# -- tab.open activate (#503) -----------------------------------------------


def test_tab_open_with_activate_false_leaves_the_selection(roost, target, project):
    """`tab.open {activate: false}` appends the tab without selecting it:
    the core's selection, the tab the window shows and the new row's own
    `is_active` — in the reply and in `tab.list` — all stay where they
    were. On both targets (#551); only iced serves `app.selected_tab_id`,
    so the window's half is read there alone."""

    def open_tab(**extra) -> dict:
        params = {"project_id": str(project), "cwd": "/tmp", **extra}
        return roost.call("tab.open", params)["tab"]

    shown = open_tab()
    assert shown["is_active"] is True, shown
    shown_id = int(shown["id"])
    assert roost.identify()["active_tab_id"] == shown_id
    if target == "iced":
        roost._wait(
            lambda: roost.app_selected_tab_id() == shown_id,
            4.0,
            "the window to show the plainly opened tab",
        )

    quiet = open_tab(activate=False)
    assert quiet["is_active"] is False, quiet
    quiet_id = int(quiet["id"])
    assert roost.project_tab_ids(project) == [shown_id, quiet_id], "appended at the end"
    assert roost.identify()["active_tab_id"] == shown_id
    if target == "iced":
        assert roost.app_selected_tab_id() == shown_id
    assert roost.tab(quiet_id)["is_active"] is False


# -- tab.open cwd_from_tab (plan 070, #532) ---------------------------------


def test_tab_open_cwd_from_tab_starts_where_the_source_tab_is(roost):
    """`cwd_from_tab` opens the new tab in the named tab's directory,
    replacing the `cwd` sent beside it; naming a tab that does not exist
    is not an error and leaves `cwd` as sent. On both targets (#532).

    The source's shell `cd`s and then `exec`s, so no prompt reports the
    move: its own cwd is `/usr/bin` while its tracked one is still
    `/usr`, and only the native-first read lands the new tab in the
    right one. Neither is a symlink on either target (the #266 test
    below gives why that matters)."""
    tracked_cwd, native_cwd = "/usr", "/usr/bin"
    pid = roost.create_project(name=f"pytest-inherit-{uuid.uuid4().hex[:8]}", cwd="/")
    try:
        source = roost.open_tab(pid, cwd=tracked_cwd)
        wait_tab_attached(roost, source)
        wait_shell_ready(roost, source)
        roost.run(source, f"cd {native_cwd} && echo MOVED$((6*7)) && exec sleep 300")
        roost.wait_text(source, "MOVED42", timeout=8)
        assert (roost.tab(source) or {}).get("cwd") == tracked_cwd, "no prompt reported the cd"

        inherited = roost.open_tab(pid, cwd="/", cwd_from_tab=source)
        roost._wait(
            lambda: (roost.tab(inherited) or {}).get("cwd") == native_cwd,
            4.0,
            "the inheriting tab's row to carry the source shell's own cwd",
        )
        wait_tab_attached(roost, inherited)
        wait_shell_ready(roost, inherited)
        marker = f"INHERIT_PWD_{uuid.uuid4().hex[:8]}"
        roost.run(inherited, f"echo {marker}=$(pwd -P)")
        roost.wait_text(inherited, f"{marker}={os.path.realpath(native_cwd)}", timeout=8)

        unresolved = roost.open_tab(pid, cwd=tracked_cwd, cwd_from_tab=2**63 - 1)
        roost._wait(
            lambda: (roost.tab(unresolved) or {}).get("cwd") == tracked_cwd,
            4.0,
            "a tab that does not exist to leave the requested cwd",
        )
    finally:
        _cleanup_project(roost, pid)


# -- tab.open cwd default (#266) -------------------------------------------


def test_tab_open_with_no_cwd_resolves_to_the_projects_cwd(roost):
    """A bare `tab.open` (no `cwd`) resolves through `Workspace::open_tab`
    to the project's cwd on both targets — Mac already did this via
    `LocalClient.openTab`; this pins the Rust engine catching up (#266).

    `/usr` stands in for the project's cwd rather than the `project`
    fixture's `/tmp`: `/tmp` is a symlink to `/private/tmp` on macOS
    (`conftest.py:97`), which would make the reply-cwd and the spawned
    shell's real `pwd` diverge for a reason that has nothing to do with
    this test. `/usr` exists on both targets and isn't a symlink.
    """
    project_cwd = "/usr"
    pid = roost.create_project(name=f"pytest-cwd-{uuid.uuid4().hex[:8]}", cwd=project_cwd)
    try:
        # Raw call, bypassing client.py's `open_tab` convenience
        # wrapper, entirely omitting `cwd` — exactly what a caller
        # that never mentions cwd sends. The reply is read directly
        # off this synchronous result, before the shell has even
        # started, so it cannot be racing OSC 7 (cwd) or OSC 0/1/2
        # (title).
        reply = roost.call(
            "tab.open",
            {"project_id": str(pid), "title": "", "cols": 80, "rows": 24},
        )
        tab = reply["tab"]
        assert tab["cwd"] == project_cwd, tab
        # derive_title's basename of the resolved cwd — the "title is
        # right too" half of the claim.
        assert tab["title"] == "usr", tab

        tab_id = int(tab["id"])
        wait_tab_attached(roost, tab_id)
        wait_shell_ready(roost, tab_id)

        # Prove the *spawn*, not just the reply row: ask the live shell
        # where it actually landed, and compare realpaths (belt-and-
        # suspenders — `/usr` isn't a symlink, but a future project_cwd
        # swap shouldn't silently stop proving this).
        marker = f"LIFECYCLE_PWD_{uuid.uuid4().hex[:8]}"
        roost.run(tab_id, f"echo {marker}=$(pwd -P)")
        roost.wait_text(
            tab_id, f"{marker}={os.path.realpath(project_cwd)}", timeout=8
        )
    finally:
        _cleanup_project(roost, pid)


# -- tab.open cwd that is not a directory (#541) ---------------------------


def test_tab_open_with_a_missing_cwd_starts_in_the_projects_cwd(roost):
    """A requested cwd that is not a directory falls back as an empty one
    does: to the project's cwd, by the row and by the shell's own
    `pwd -P`. On both targets (#541). `/usr` for the reason the #266 test
    above gives."""
    project_cwd = "/usr"
    pid = roost.create_project(name=f"pytest-gone-{uuid.uuid4().hex[:8]}", cwd=project_cwd)
    try:
        missing = f"/roost-no-such-dir-{uuid.uuid4().hex[:8]}"
        assert not os.path.exists(missing)
        tab = roost.call(
            "tab.open",
            {"project_id": str(pid), "cwd": missing, "title": "", "cols": 80, "rows": 24},
        )["tab"]
        assert tab["cwd"] == project_cwd, tab

        tab_id = int(tab["id"])
        wait_tab_attached(roost, tab_id)
        wait_shell_ready(roost, tab_id)
        marker = f"GONE_PWD_{uuid.uuid4().hex[:8]}"
        roost.run(tab_id, f"echo {marker}=$(pwd -P)")
        roost.wait_text(tab_id, f"{marker}={os.path.realpath(project_cwd)}", timeout=8)
    finally:
        _cleanup_project(roost, pid)


def test_tab_open_in_a_directory_the_shell_cannot_enter_closes_the_tab(roost, target, tmp_path):
    """A directory the shell cannot enter (mode 000) is still a directory,
    so nothing falls back: the spawn fails and the tab closes, on both
    targets (#541). Only the reply differs. Rust's spawn fails inside the
    call, which answers an error; the Mac's forked child exits on the
    failed `chdir` after the call has answered, and that exit closes the
    tab. The project keeps the tab it already had, because closing a
    project's last tab deletes the project."""
    pid = roost.create_project(name=f"pytest-locked-{uuid.uuid4().hex[:8]}", cwd="/usr")
    locked = tmp_path / "locked"
    locked.mkdir()
    try:
        kept = roost.open_tab(pid, cwd="/usr")
        wait_tab_attached(roost, kept)
        locked.chmod(0)
        if target == "mac":
            roost.wait_gone(roost.open_tab(pid, cwd=str(locked)))
        else:
            with pytest.raises(RoostError) as failed:
                roost.open_tab(pid, cwd=str(locked))
            assert failed.value.code == "internal", failed.value
        roost._wait(
            lambda: roost.project_tab_ids(pid) == [kept],
            5.0,
            "the project to hold only the tab it already had",
        )
    finally:
        locked.chmod(0o755)
        _cleanup_project(roost, pid)


# -- tab.close active fallback ---------------------------------------------


def test_closing_the_focused_middle_tab_lands_on_its_right_hand_neighbour(roost, project):
    """Plan 069 §4.1 case 1, cross-target: three tabs, the MIDDLE one
    focused and closed. The strip goes to the tab on its right — the
    layout is deliberately discriminating, since the leftmost tab and
    the lowest id are both the *first* tab, which is what both engines
    used to answer."""
    first = roost.open_tab(project, cwd="/tmp")
    middle = roost.open_tab(project, cwd="/tmp")
    last = roost.open_tab(project, cwd="/tmp")
    for tab_id in (first, middle, last):
        wait_tab_attached(roost, tab_id)
    roost._wait(
        lambda: roost.project_tab_ids(project) == [first, middle, last],
        5.0,
        "the three tabs settle in open order",
    )

    roost.focus(middle)
    roost._wait(
        lambda: roost.identify()["active_tab_id"] == middle,
        5.0,
        "focus makes the middle tab active",
    )

    roost.close_tab(middle)
    roost.wait_gone(middle)
    # `identify` is its own surface and the selection can settle after
    # the row has gone — poll it, never read it once.
    roost._wait(
        lambda: roost.identify()["active_tab_id"] == last,
        5.0,
        "the tab to the right of the closed one takes the strip",
    )


# -- project.delete --------------------------------------------------------


def test_project_delete_cascades_tabs_and_active_falls_back_to_the_project_above(roost):
    """Deleting a project with live tabs removes the project AND its tabs
    from `tab.list`, and the active selection lands on the neighbour the
    module docstring's rule names — here the nearest project above the
    doomed one that has a tab to show, so the tabless `keep` created
    beside it is walked straight past.
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

        # The order + per-project tab counts as they stand at the moment
        # of the delete — the rule reads exactly this.
        before_projects = roost.list()
        before_ids = {int(p["id"]) for p in before_projects}
        assert {keep, doomed} <= before_ids
        expected_active = _neighbour_project(before_projects, doomed)
        assert expected_active != keep, "the tabless project must be skipped, not chosen"

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
            lambda: roost.identify()["active_project_id"] == expected_active,
            5.0,
            f"active project falls back to {expected_active}, the nearest "
            "project above the deleted one that has a tab",
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


# -- non-canonical ids are refused, not normalized (#402) ------------------


def test_reorder_ops_refuse_a_non_canonical_id(roost, project):
    """A non-canonical integer spelling (`"+4"`) is refused with
    `invalid-param` on both `tab.reorder` and `project.reorder`, and on
    both UI sockets.

    Rust's `WireTabRef`/`WireProjectRef::parse` round-trip the decoded
    id (`to_string() == text`) and refuse `"+4"`; Swift's plain
    `Int64("+4")` succeeds, so before #402's fix the Mac socket silently
    normalized `"+4"` to `4` instead of answering `invalid-param` like
    iced (and a session socket). This is a parity test and deliberately
    NOT target-gated — it must pass identically under `--roost-target
    mac` and `--roost-target iced` (both are required CI gates), unlike
    the iced-only tests above.
    """
    with pytest.raises(RoostError) as tab_exc:
        roost.reorder_tabs(project, ["+4"])
    assert tab_exc.value.code == "invalid-param", (
        f"tab.reorder with a non-canonical id: {tab_exc.value}"
    )

    with pytest.raises(RoostError) as project_exc:
        roost.reorder_projects(["+4"])
    assert project_exc.value.code == "invalid-param", (
        f"project.reorder with a non-canonical id: {project_exc.value}"
    )


def test_dump_ops_refuse_a_non_canonical_id(roost):
    """`tab.dump` and `tab.dump_resolved` are `WireTabRef`-backed too
    (Rust `messages.rs`'s `TabDumpParams`/`TabDumpResolvedParams`), so
    the same #402 narrowing from the test above applies to them —
    that plan's discovery record only named `tab.reorder`/
    `project.reorder`, missing these two (and `tab.capture_pty_input`,
    covered separately in `test_test_ops.py` since it's gated behind
    `ROOST_TEST_MODE=1`).

    Both handlers decode params before ever looking up the tab, so a
    non-canonical id never needs a real tab to trip this — no `project`
    fixture required. Parity test, not target-gated, like the one
    above.
    """
    for bad_id in ("+4", "04"):
        with pytest.raises(RoostError) as dump_exc:
            roost.dump(bad_id)
        assert dump_exc.value.code == "invalid-param", (
            f"tab.dump with non-canonical id {bad_id!r}: {dump_exc.value}"
        )

        with pytest.raises(RoostError) as resolved_exc:
            roost.tab_dump_resolved(bad_id)
        assert resolved_exc.value.code == "invalid-param", (
            f"tab.dump_resolved with non-canonical id {bad_id!r}: {resolved_exc.value}"
        )


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

    The second project carries the shown tab, so its delete is also the
    module's rule-2 assertion over a project the window was actually
    looking at.
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
        shown = roost.open_tab(b, cwd="/tmp")
        wait_tab_attached(roost, shown)
        roost._wait(
            lambda: roost.identify()["active_project_id"] == b,
            5.0,
            "the opened tab makes project b the shown one",
        )

        _cleanup_project(roost, a)
        before_projects = roost.list()
        expected_active = _neighbour_project(before_projects, b)
        _cleanup_project(roost, b)

        # Projects remain (this suite never empties the workspace), so the
        # UI must still be answering — no exit, no cascade past the delete.
        assert _project_ids(roost), "the shared session must keep a project"
        roost._wait(
            lambda: a not in _project_ids(roost) and b not in _project_ids(roost),
            5.0,
            "the deleted projects are gone from the workspace snapshot",
        )
        roost._wait(
            lambda: roost.identify()["active_project_id"] == expected_active,
            5.0,
            f"active falls back to {expected_active}, the nearest project "
            "above the deleted one that has a tab",
        )
    finally:
        for pid in (a, b):
            _cleanup_project(roost, pid)
