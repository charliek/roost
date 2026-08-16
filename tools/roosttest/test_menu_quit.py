"""Menu-Quit E2E — plan 028 C3, the app-ending half of `app.menu_activate`.

The App menu's Quit item is a deliberate deviation from Swift parity
(plan 028 § 3.2): rather than `NSApplication.terminate:` (which would
bypass iced's exit path and skip `Workspace::flush()`'s clean-exit
fsync), it fires `MenuEvent::Quit` -> `ExitState::request()` -> the same
`UiTask::Exit` / graceful-shutdown path `test_exit_on_empty.py` already
covers for the empty-workspace trigger. This module is Quit's own
trigger: no need to empty the workspace first, since `ExitState::request`
is unconditional (`app.rs:1775`) — this test asserts the workspace can be
non-empty and Quit still ends the process cleanly.

**Its own pytest invocation, separate from `test_exit_on_empty.py`, on
purpose** (Makefile `ICED_MENU_QUIT_E2E_TESTS` / `e2e-iced-menu-quit`,
its own CI steps). Both are "exit lane" tests in the sense that each
kills the UI it drives, but they cannot share ONE pytest invocation
with each other any more than either can share one with
`ICED_E2E_TESTS`: the session-scoped harness fixture launches exactly
one UI for the whole invocation, so whichever exit-ending test runs
first would strand the other. `util.runs_alone` enforces this itself,
shared with `test_exit_on_empty.py`, rather than trusting the
Makefile/ci.yml wiring to keep them apart.

darwin+iced only: `app.menu_activate` is macOS-iced-only (plan 028
§ 3.12). GTK and the Swift Mac app both skip.
"""

from __future__ import annotations

import json
import os
import sys

import pytest
import ui
from client import scaled_timeout
from util import is_fresh, runs_alone

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# `title_fallback(BundleProfileKind::Iced)` — see test_menu_bar.py's
# same constant + comment.
APP = "Roost-Iced"


def test_menu_quit_ends_the_app_cleanly(roost, project, target, request):
    """Fire `["<app>", "Quit <app>"]` via `app.menu_activate` and assert
    the three things the graceful exit path owes: the activating
    client's reply is not itself proof of anything (the `MenuEvent`
    lands on a later update turn, per § 3.12's caveat) — so this waits
    on the process actually exiting — the process ends itself with
    status 0 (proof `App::drop` ran, since only a normal run-loop
    return gets there), and `state.json` is intact and reflects the
    live layout (the `project` fixture's throwaway project), not a torn
    or stale file.
    """
    if sys.platform != "darwin" or target != "iced":
        pytest.skip(
            "app.menu_activate is macOS-iced-only (plan 028 § 3.12)"
        )
    if not TEST_MODE:
        pytest.skip("app.menu_activate requires ROOST_TEST_MODE=1 in the UI's launch env")
    if not runs_alone(request):
        pytest.skip(
            "ends the UI it drives, so it must be its own pytest "
            "invocation (`make e2e-iced-menu-quit`) — collected beside "
            "other modules it would strand every test after it"
        )
    if not is_fresh():
        pytest.skip(
            "ends the UI — requires a fresh, harness-owned instance (--roost-fresh)"
        )
    process = ui.owned_process(target)
    state_dir = ui.session_state_dir()
    assert process is not None and state_dir is not None, (
        "fresh mode must have launched the UI itself; without the child "
        "handle there is no way to observe the exit"
    )

    # The workspace is deliberately left non-empty (the `project`
    # fixture's throwaway project plus whatever bootstrap seeded) —
    # unlike test_exit_on_empty.py, Quit does not require an empty
    # workspace to fire.
    before = roost.list()
    assert any(int(p["id"]) == project for p in before), (
        "the throwaway project must exist before Quit fires, so state.json "
        "afterward has something to have flushed"
    )

    roost.app_menu_activate([APP, f"Quit {APP}"])

    exit_code = process.wait(timeout=scaled_timeout(20.0))
    assert exit_code == 0, (
        f"the UI must end its run loop normally (exit {exit_code}); a "
        "non-zero status means the process was killed or panicked instead "
        "of dropping App and flushing state — Quit must not be routed "
        "through NSApplication.terminate:, which would skip that entirely"
    )
    assert not ui.is_alive(target), "the IPC socket must not answer after the exit"

    state = json.loads((state_dir / "state.json").read_text())
    state_project_ids = {int(p["id"]) for p in state["projects"]}
    assert project in state_project_ids, (
        f"state.json must reflect the live layout at the moment Quit fired, "
        f"got projects {sorted(state_project_ids)!r}"
    )
