"""End-to-end smoke suite — drives the real UI over IPC and asserts on
the op set (tab.dump / tab.list / identify). Runs against either UI via
`--roost-target`. Mirrors the manual checklist in
docs/development/claude-testing.md.
"""

from __future__ import annotations

import uuid


def _wait_flag(roost, tab_id, key, value, timeout=4.0):
    roost._wait(lambda: (roost.tab(tab_id) or {}).get(key) == value,
                timeout, f"tab {tab_id} {key} == {value}")


def test_open_tab_and_dump_content(roost, project):
    """tab.dump reads back exact terminal content (the determinism
    backbone). Assert on a marker that appears only in the OUTPUT, not
    the echoed command."""
    tab = roost.open_tab(project, cwd="/tmp", title="t")
    marker = uuid.uuid4().hex[:8]
    # `run` waits for the prompt first; assert on the marker, which
    # appears only in the OUTPUT (printf), never in the command echo.
    roost.run(tab, f"printf 'OUT=%s\\n' {marker}")
    roost.wait_text(tab, f"OUT={marker}", timeout=8)
    assert f"OUT={marker}" in roost.dump_text(tab)


def test_state_progression(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    for state in ("running", "needs_input", "idle", "none"):
        roost.set_state(tab, state)
        roost.wait_state(tab, state, timeout=3)
        assert roost.tab(tab)["state"] == state


def test_notification_set_and_clear(roost, project):
    a = roost.open_tab(project, cwd="/tmp")
    roost.open_tab(project, cwd="/tmp")  # steals active, so `a` is inactive
    roost.notify(a, "hi", "body")
    _wait_flag(roost, a, "has_notification", True)
    roost.clear_notification(a)
    _wait_flag(roost, a, "has_notification", False)


def test_focus_sets_active_tab(roost, project):
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")
    roost.focus(a)
    assert roost.identify()["active_tab_id"] == a
    roost.focus(b)
    assert roost.identify()["active_tab_id"] == b


def test_set_title_locks_against_osc(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    roost.set_title(tab, "PINNED")
    assert roost.tab(tab)["title"] == "PINNED"
    assert roost.tab(tab)["user_titled"] is True


def test_reorder_tabs_persists_order(roost, project):
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")
    c = roost.open_tab(project, cwd="/tmp")
    assert roost.project_tab_ids(project) == [a, b, c]
    roost.reorder_tabs(project, [c, a, b])
    roost._wait(
        lambda: roost.project_tab_ids(project) == [c, a, b],
        4.0,
        "tab.reorder updated the workspace order",
    )


def test_rename_project(roost, project):
    new_name = f"renamed-{uuid.uuid4().hex[:6]}"
    roost.rename_project(project, new_name)
    roost._wait(
        lambda: (roost.project(project) or {}).get("name") == new_name,
        4.0,
        "project.rename took",
    )


def test_close_non_last_tab_keeps_project(roost, project):
    """Closing a non-last tab removes just that tab; the project survives
    (the complement of cascade-close)."""
    a = roost.open_tab(project, cwd="/tmp")
    b = roost.open_tab(project, cwd="/tmp")
    roost.close_tab(a)
    roost.wait_gone(a)
    assert roost.project(project) is not None
    assert roost.project_tab_ids(project) == [b]


def test_cascade_close_removes_project(roost):
    pid = roost.create_project(name=f"pytest-cc-{uuid.uuid4().hex[:6]}", cwd="/tmp")
    tab = roost.open_tab(pid, cwd="/tmp")
    roost.close_tab(tab)
    # Closing a project's last tab cascade-closes the project.
    roost._wait(lambda: all(int(p["id"]) != pid for p in roost.list()),
                4.0, f"project {pid} cascade-closed")
