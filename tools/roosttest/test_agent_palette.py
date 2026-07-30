"""Agent palette E2E — plan 005 §3.10, both targets (`--roost-target
mac|gtk`). Drives `palette.open kind="agents"`, seeds lifecycles via
`tab.agent_report` (and, for the two paths that must exercise the real
adapter, the actual `roostctl claude-hook` binary), and asserts on the
wire's `PaletteItemView.agent` payload documented in
`tests/ipc-vectors/palette.state.agents.response.json`.

Isolation rules (plan 005 §3.10, the plan-004 harness convention):
assertions are scoped to rows THIS test seeded (by id) and to their
RELATIVE order — never to the absolute row list, since a dev UI may have
other agent-owned tabs already open. The one exception is the empty-state
test, which needs a fresh harness-owned instance to guarantee zero rows
and is skipped otherwise (`util.is_fresh`).

Every seeded lifecycle is reported only after the tab's shell reaches its
first prompt (`wait_shell_state(..., "at_prompt")`): a late startup A/B/D
OSC 133 mark drops the raw agent lifecycle back to `inactive` while
keeping ownership as a label (the plan-002 dead-agent failsafe), which
would silently reset a lifecycle seeded too early.
"""

from __future__ import annotations

import os
import re
import subprocess
import uuid
from pathlib import Path

import pytest

from client import RoostError
from test_agent_lifecycle import agent_tab, claude_hook
from util import is_fresh, wait_tab_attached

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

EM_DASH = "—"  # git_metrics::UNKNOWN / GitMetrics.swift's rendered "no metrics" value
TIME_TEXT_RE = re.compile(r"^\d+[smhd]$")


# ---------------------------------------------------------------------------
# Seeding helpers
# ---------------------------------------------------------------------------


def _agent_source(tag: str) -> str:
    """A source string that is neither `manual` nor `legacy` (so it
    passes the population filter) and is unique enough per test to avoid
    ownership collisions with anything else running in the session."""
    return f"e2e-{tag}-{uuid.uuid4().hex[:8]}"


def _seed(
    roost,
    tab_id: int,
    *,
    lifecycle: str,
    name: str | None = None,
    detail: str = "",
    metadata: dict[str, str] | None = None,
    source: str | None = None,
    session: str | None = None,
) -> str:
    """Claim ownership of a settled tab with the given lifecycle. Waits
    for the shell to reach its first prompt before reporting (module
    docstring). Returns the (source, session) pair's source, so callers
    that need to report again (live refresh, ordering ties) reuse it."""
    if name is not None:
        roost.set_title(tab_id, name)
    wait_tab_attached(roost, tab_id)
    if TEST_MODE:
        # CI shells have no OSC 133 integration, so waiting for a real
        # prompt mark hangs there. Feed the A mark ourselves — same
        # settled end-state, deterministic, and it still exercises the
        # late-mark-can't-reset property the passive wait was for.
        roost.tab_feed_pty_bytes(tab_id, b"\x1b]133;A\x07")
    roost.wait_shell_state(tab_id, "at_prompt", timeout=15.0)
    src = source or _agent_source("seed")
    roost.agent_report(
        tab_id,
        src,
        "claim",
        session_id=session or uuid.uuid4().hex,
        lifecycle=lifecycle,
        detail=detail,
        metadata=metadata,
    )
    return src


def _run_git(repo: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)


def _make_git_repo(root: Path) -> tuple[Path, str]:
    """A throwaway repo with a precomputed, deterministic diff: two
    tracked files modified (`a.txt` +2 insertions, `b.txt` -1 deletion)
    plus one untracked file — so `git shortstat` + `ls-files --others`
    compose to an exact `metrics_text` this test can assert byte for
    byte (plan 005 §3.7's file-count-is-shortstat-plus-untracked rule)."""
    repo = root / "repo"
    repo.mkdir()
    _run_git(repo, "init", "-q")
    (repo / "a.txt").write_text("one\n")
    (repo / "b.txt").write_text("alpha\nbeta\ngamma\n")
    _run_git(repo, "add", "a.txt", "b.txt")
    _run_git(repo, "-c", "user.name=t", "-c", "user.email=t@t.com", "commit", "-q", "-m", "init")
    (repo / "a.txt").write_text("one\ntwo\nthree\n")  # +2 insertions
    (repo / "b.txt").write_text("alpha\ngamma\n")  # -1 deletion
    (repo / "c.txt").write_text("untracked\n")  # +1 untracked file
    return repo, "3f +2 -1"


def _sidebar_collapsed(roost) -> bool:
    return bool(roost.window_metrics()["sidebar_collapsed"])


def _collapse_sidebar(roost) -> None:
    """Drive `toggle_sidebar` through the command palette. No-op if
    already collapsed. Leaves the command palette closed either way."""
    if _sidebar_collapsed(roost):
        return
    roost.palette_open()
    roost.palette_activate("toggle_sidebar")
    roost._wait(lambda: _sidebar_collapsed(roost), 5.0, "sidebar collapses via toggle_sidebar")


# ---------------------------------------------------------------------------
# Entry points + the invalid-kind error string
# ---------------------------------------------------------------------------


def test_open_kind_agents_and_unknown_kind_is_invalid_param(palette):
    st = palette.palette_open(kind="agents")
    assert st["open"] is True
    assert st["frame"] == "agents"
    palette.palette_dismiss()

    bad = "bogus-" + uuid.uuid4().hex[:6]
    with pytest.raises(RoostError) as ei:
        palette.palette_open(kind=bad)
    assert ei.value.code == "invalid-param"
    # Byte-identical across both UIs (crates/roost-linux/src/ipc.rs ~L678,
    # mac/Sources/Roost/IPCHandlerImpl.swift ~L511) — the parity assertion.
    assert ei.value.message == (
        f'unknown palette kind "{bad}" '
        '(want "commands", "launcher", "custom", or "agents")'
    ), ei.value.message


def test_empty_state_is_a_single_non_actionable_row(roost, palette):
    if not is_fresh():
        pytest.skip(
            "the empty-agents-palette state requires a fresh harness-owned "
            "instance (--roost-fresh) — an ad-hoc dev UI likely already has "
            "other agent-owned tabs open"
        )
    st = palette.palette_open(kind="agents")
    assert [it["id"] for it in st["items"]] == ["agents:empty"], st["items"]
    assert st["items"][0]["title"] == "No agent sessions"
    assert st["items"][0].get("agent") is None

    # Activating the sentinel is the existing non-actionable skip: it's
    # found (so no `not-found` error) but confirms nothing.
    st = palette.palette_activate("agents:empty")
    assert st["open"] is True, "activating the empty sentinel must leave the palette open"


# ---------------------------------------------------------------------------
# Row population + content
# ---------------------------------------------------------------------------


def test_seed_four_lifecycles_ranked_with_exact_status_and_title(roost, project):
    """Waiting + working + failed(+detail) + finished across two
    projects: relative rank order, exact `status_text` per lifecycle
    (the cross-UI parity assertion), `title` composition, and a
    well-formed `time_text`."""
    p2 = roost.create_project(name=f"pytest-agents2-{uuid.uuid4().hex[:6]}", cwd="/tmp")
    try:
        t_waiting = roost.open_tab(project, cwd="/tmp")
        t_working = roost.open_tab(project, cwd="/tmp")
        t_failed = roost.open_tab(p2, cwd="/tmp")
        t_finished = roost.open_tab(p2, cwd="/tmp")

        _seed(roost, t_waiting, lifecycle="waiting", name="waiting-agent")
        _seed(roost, t_working, lifecycle="working", name="working-agent")
        _seed(roost, t_failed, lifecycle="failed", name="failed-agent", detail="rate_limit")
        _seed(roost, t_finished, lifecycle="finished", name="finished-agent")

        proj1_name = roost.project(project)["name"]
        proj2_name = roost.project(p2)["name"]

        items = roost.palette_items("agents")
        by_id = {it["id"]: it for it in items}

        # (tab, project name, expected tab name, expected status_text),
        # in the expected urgency order: failed > waiting > working > finished.
        expect = [
            (t_failed, proj2_name, "failed-agent", "Failed · rate_limit"),
            (t_waiting, proj1_name, "waiting-agent", "Waiting for input"),
            (t_working, proj1_name, "working-agent", "Working"),
            (t_finished, proj2_name, "finished-agent", "Finished"),
        ]
        ids = [f"agent:{t}" for t, _, _, _ in expect]
        missing = [rid for rid in ids if rid not in by_id]
        assert not missing, f"seeded rows missing: {missing}; got {sorted(by_id)}"

        # RELATIVE order only: filter the full row list down to just our
        # seeded ids (preserving wire order) and compare — a dev UI's
        # other agent-owned tabs, interleaved anywhere, don't matter.
        seen_order = [it["id"] for it in items if it["id"] in set(ids)]
        assert seen_order == ids, seen_order

        for tab_id, proj_name, name, status in expect:
            row = by_id[f"agent:{tab_id}"]
            agent = row["agent"]
            assert agent["status_text"] == status, agent
            assert row["title"] == f"{proj_name} · {name}", row
            assert agent["project"] == proj_name, agent
            assert agent["name"] == name, agent
            assert TIME_TEXT_RE.match(agent["time_text"]), agent["time_text"]
    finally:
        roost.delete_project(p2)


def test_working_with_background_tasks_detail(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    _seed(roost, tab, lifecycle="working", detail="background_tasks:2")
    row = roost.palette_row("agents", f"agent:{tab}")
    assert row is not None
    assert row["agent"]["status_text"] == "Working · 2 bg tasks", row["agent"]


def test_session_title_metadata_preferred_over_tab_title(roost, project):
    tab = roost.open_tab(project, cwd="/tmp")
    _seed(
        roost,
        tab,
        lifecycle="working",
        name="fallback-tab-title",
        metadata={"session_title": "slauth-refactor"},
    )
    row = roost.palette_row("agents", f"agent:{tab}")
    assert row is not None
    assert row["agent"]["name"] == "slauth-refactor", row["agent"]
    assert row["title"].endswith(" · slauth-refactor"), row["title"]


def test_manual_claimed_tab_is_excluded(roost, project):
    """`tab.set_state` claims ownership as source `manual` (plan 002
    §3.3/§3.6) — one of the two internal sources the population filter
    excludes (plan 005 §3.2)."""
    tab = roost.open_tab(project, cwd="/tmp")
    roost.set_state(tab, "running")
    owner = roost.ownership(tab)
    assert owner is not None and owner["source"] == "manual", owner
    assert roost.palette_row("agents", f"agent:{tab}") is None


def test_filter_narrows_by_project_or_name_and_no_match_is_empty(roost, project, palette):
    tag = uuid.uuid4().hex[:8]
    tab = roost.open_tab(project, cwd="/tmp")
    _seed(roost, tab, lifecycle="working", name=f"zzz-{tag}")
    row_id = f"agent:{tab}"

    st = palette.palette_open(kind="agents")
    assert any(it["id"] == row_id for it in st["items"])

    st = palette.palette_query(tag)
    assert palette.palette_item_ids(st) == [row_id], palette.palette_item_ids(st)

    st = palette.palette_query("no-such-agent-" + uuid.uuid4().hex)
    assert palette.palette_item_ids(st) == []


# ---------------------------------------------------------------------------
# The raw-vs-effective lifecycle pin (test-mode gated)
# ---------------------------------------------------------------------------


@pytest.mark.skipif(
    not TEST_MODE,
    reason="feeding OSC 133 needs ROOST_TEST_MODE=1 in the UI's launch env "
    "(tab.feed_pty_bytes is gated)",
)
def test_effective_lifecycle_pin_via_osc133_marks(roost, project):
    """The raw-vs-effective distinguishing test (plan 005 §3.2): claim
    ownership with NO lifecycle (raw stays `inactive`), so the row reads
    off the *shell* axis until the agent actually reports a turn — an
    agent that just claimed a tab and is running a foreground process
    shows "Working", and a dead agent's tab reaching a prompt (the OSC
    133 `A`/`D` failsafe) shows "Idle", exactly like the tab pill."""
    tab = agent_tab(roost, project)  # bare bash, no shell integration of its own
    row_id = f"agent:{tab}"
    roost.agent_report(tab, _agent_source("eff"), "claim", session_id="sess-eff")
    assert roost.agent_lifecycle(tab) == "inactive"

    roost.tab_feed_pty_bytes(tab, b"\x1b]133;C\x07")
    roost.wait_shell_state(tab, "foreground_process")
    row = roost.wait_palette_row_field("agents", row_id, "agent.status_text", "Working")
    assert row["agent"]["effective_lifecycle"] == "working", row["agent"]

    roost.tab_feed_pty_bytes(tab, b"\x1b]133;A\x07")
    roost.wait_shell_state(tab, "at_prompt")
    row = roost.wait_palette_row_field("agents", row_id, "agent.status_text", "Idle")
    assert row["agent"]["effective_lifecycle"] == "inactive", row["agent"]


# ---------------------------------------------------------------------------
# Subagent immunity, driven through the real hook binary
# ---------------------------------------------------------------------------


def test_subagent_event_via_real_claude_hook_leaves_row_unchanged(roost, project, target):
    """The filter that keeps a subagent's own events from perturbing the
    owner's lifecycle is unit-covered in `roost-agent` (plan 002 §3.8);
    this is the one place the WHOLE chain — `roostctl claude-hook` ->
    the adapter -> `tab.agent_report` -> the workspace -> the palette
    row — is proven wired end to end, per plan 005 §3.3/§3.10."""
    tab = agent_tab(roost, project)
    session = f"sess-subagent-{uuid.uuid4().hex[:6]}"
    claude_hook(target, tab, "SessionStart", {"session_id": session, "source": "startup"})
    claude_hook(target, tab, "UserPromptSubmit", {"session_id": session, "prompt": "go"})
    roost.wait_lifecycle(tab, "working")

    row_id = f"agent:{tab}"
    before = roost.palette_row("agents", row_id)
    assert before is not None and before["agent"]["status_text"] == "Working", before

    # A synthetic agent_id-carrying payload — as if it fired inside a
    # subagent's own turn. Without `agent_id` this exact payload would
    # move the owner to "waiting".
    claude_hook(target, tab, "Notification", {
        "session_id": session,
        "agent_id": "subagent-e2e",
        "notification_type": "permission_prompt",
        "message": "the subagent wants Bash",
    })
    roost.wait_notification(tab, True)  # the event still fires a notification

    after = roost.palette_row("agents", row_id)
    assert after is not None
    assert after["agent"]["status_text"] == "Working", after


# ---------------------------------------------------------------------------
# Live refresh while the palette stays open
# ---------------------------------------------------------------------------


def test_live_refresh_flips_lifecycle_and_project_rename(roost, project, palette):
    tab = roost.open_tab(project, cwd="/tmp")
    session = uuid.uuid4().hex
    source = _seed(roost, tab, lifecycle="waiting", session=session)
    row_id = f"agent:{tab}"

    st = palette.palette_open(kind="agents")
    row = next(it for it in st["items"] if it["id"] == row_id)
    assert row["agent"]["status_text"] == "Waiting for input"

    # Flip the lifecycle with the palette still open — the event dispatch
    # (plan 005 §3.8) must rebuild the open frame's rows in place.
    roost.agent_report(tab, source, "preserve", session_id=session, lifecycle="working")

    def _status() -> str | None:
        st = palette.palette_state()
        row = next((it for it in st["items"] if it["id"] == row_id), None)
        return (row or {}).get("agent", {}).get("status_text")

    palette._wait(lambda: _status() == "Working", 5.0, "row live-refreshes to Working while open")

    # Renaming the row's project (still with the palette open) updates
    # the project cell too — pins the "any event that can change row
    # content" amendment, not just an agent_report.
    new_name = f"{roost.project(project)['name']}-renamed"
    roost.rename_project(project, new_name)

    def _project_cell() -> str | None:
        st = palette.palette_state()
        row = next((it for it in st["items"] if it["id"] == row_id), None)
        return (row or {}).get("agent", {}).get("project")

    palette._wait(
        lambda: _project_cell() == new_name, 5.0, "row project cell updates on rename while open"
    )


# ---------------------------------------------------------------------------
# Activation, escape/dismiss, reopen
# ---------------------------------------------------------------------------


def test_activate_agent_row_focuses_tab_and_closes_palette(roost, project, target, palette):
    tab = roost.open_tab(project, cwd="/tmp")
    roost.open_tab(project, cwd="/tmp")  # steals active
    _seed(roost, tab, lifecycle="waiting")
    row_id = f"agent:{tab}"

    if target == "gtk":
        # §3.11 sidebar parity fix: agent-row activation must reveal a
        # collapsed sidebar, same as the notification-jump path.
        _collapse_sidebar(roost)

    st = palette.palette_open(kind="agents")
    assert any(it["id"] == row_id for it in st["items"])
    st = palette.palette_activate(row_id)
    assert st["open"] is False  # jumping confirms + closes the palette

    roost._wait(
        lambda: roost.identify()["active_tab_id"] == tab, 5.0, "agent row activation focuses its tab"
    )

    if target == "gtk":
        roost._wait(
            lambda: not _sidebar_collapsed(roost), 5.0, "sidebar reappears after the jump (§3.11)"
        )


def test_activate_row_for_a_closed_tab_does_not_crash(roost, project, palette):
    """The tab closes while its row is still on the (pre-refresh)
    visible list. Either the live refresh has already dropped the row
    (`palette.activate` then raises `not-found`, same as any unknown id)
    or it hasn't yet (the row's tab id still parses, so the confirm
    closes the palette and the deferred focus silently no-ops) — both
    are the harness-visible "no crash" outcomes plan 005 §3.10 asks for.
    A real crash would surface as a transport-level RoostError
    (`disconnected`), which this does NOT catch."""
    tab = roost.open_tab(project, cwd="/tmp")
    _seed(roost, tab, lifecycle="working")
    row_id = f"agent:{tab}"

    st = palette.palette_open(kind="agents")
    assert any(it["id"] == row_id for it in st["items"])
    roost.close_tab(tab)
    try:
        st = palette.palette_activate(row_id)
        assert st["open"] is False, "a successful activate on a vanished tab still closes the palette"
    except RoostError as e:
        assert e.code == "not-found", e


def test_dismiss_closes_and_reopen_rebuilds(roost, project, palette):
    """Hotkey chords (Esc included) aren't injectable via roosttest
    (plan 005 §3.10's unit+manual note); `palette.dismiss` drives the
    same close path Esc triggers, and a reopen rebuilds fresh."""
    tab = roost.open_tab(project, cwd="/tmp")
    _seed(roost, tab, lifecycle="working")
    row_id = f"agent:{tab}"

    st = palette.palette_open(kind="agents")
    assert any(it["id"] == row_id for it in st["items"])
    st = palette.palette_dismiss()
    assert st["open"] is False
    assert palette.palette_item_ids(st) == []

    st = palette.palette_open(kind="agents")
    assert st["open"] is True
    assert any(it["id"] == row_id for it in st["items"])


# ---------------------------------------------------------------------------
# Command palette entry point
# ---------------------------------------------------------------------------


def test_command_palette_view_agents_directly_after_select_font(palette):
    st = palette.palette_open()
    ids = palette.palette_item_ids(st)
    i = ids.index("select_font")
    assert ids[i + 1] == "view_agents", ids

    st = palette.palette_activate("view_agents")
    assert st["open"] is True
    assert st["frame"] == "agents"


# ---------------------------------------------------------------------------
# Git metrics, end to end
# ---------------------------------------------------------------------------


def test_git_metrics_wired_end_to_end(roost, project, tmp_path, palette):
    repo, expected = _make_git_repo(tmp_path)

    tab1 = roost.open_tab(project, cwd=str(repo))
    _seed(roost, tab1, lifecycle="working")
    row_id1 = f"agent:{tab1}"

    st = roost.palette_open(kind="agents")
    row = next(it for it in st["items"] if it["id"] == row_id1)
    # Pending is legal immediately after open — metrics resolve async
    # (plan 005 §3.7); either state is valid here, but it must settle.
    assert row["agent"].get("metrics_text") in (None, expected), row["agent"]

    def _metrics(row_id: str) -> str | None:
        st = roost.palette_state()
        row = next((it for it in st["items"] if it["id"] == row_id), None)
        return (row or {}).get("agent", {}).get("metrics_text")

    roost._wait(
        lambda: _metrics(row_id1) == expected, 8.0, f"tab1 metrics_text becomes {expected!r}"
    )

    # A second tab on the identical repo must resolve to the identical
    # string (the dedupe-by-root cache reuse), while the palette stays
    # open the whole time (live refresh picks up the new row + its cwd).
    tab2 = roost.open_tab(project, cwd=str(repo))
    _seed(roost, tab2, lifecycle="working")
    row_id2 = f"agent:{tab2}"
    roost._wait(
        lambda: _metrics(row_id2) == expected, 8.0, "tab2 (same repo) gets the identical metrics_text"
    )

    # A non-repo cwd resolves to the em-dash sentinel.
    non_repo = tmp_path / "not-a-repo"
    non_repo.mkdir()
    tab3 = roost.open_tab(project, cwd=str(non_repo))
    _seed(roost, tab3, lifecycle="working")
    row_id3 = f"agent:{tab3}"
    roost._wait(
        lambda: _metrics(row_id3) == EM_DASH, 8.0, "non-repo tab resolves to the em-dash sentinel"
    )

    roost.palette_dismiss()
