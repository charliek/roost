"""Exit-on-empty E2E — deleting the last project ends the Iced app.

Plan 026 D8/C7. Mac parity: the Swift app closes its window when
`.projectDeleted` leaves no projects and the process terminates behind it
(App.swift `applicationShouldTerminateAfterLastWindowClosed`); Iced now
does the same from `reconcile()`'s snapshot, so every route reaches it —
UI close, palette, the confirm dialog, the last tab's PTY exit (the engine
cascades tab → project, workspace.rs), and raw `project.delete` over IPC.

**Its own pytest invocation, on purpose** (Makefile `e2e-iced-exit`, its
own CI step). This test kills the UI it drives, so it cannot live in the
shared-session iced lane: the session-scoped harness fixture launches ONE
instance for the whole invocation, and a mid-suite exit would leave every
later module without a UI. It is deliberately absent from Makefile
`ICED_E2E_TESTS` and from the three enumerated ci.yml iced lists — and it
enforces that itself (`util.runs_alone`) rather than trusting those lists.

Mac has always terminated on the last project's window close, so it
skips here too. (The now-removed GTK UI was unchanged by this slice,
per the plan-016 stance — it kept its empty-workspace state instead.)
"""

from __future__ import annotations

import json

import pytest
import ui
from client import scaled_timeout
from util import is_fresh, runs_alone


def _project_ids(roost) -> list[int]:
    return [int(p["id"]) for p in roost.list()]


def test_deleting_the_last_project_exits_cleanly_and_flushes_state(roost, target, request):
    """Delete every project over IPC and assert the three things the exit
    path owes: the deleting client gets its success reply (the socket does
    NOT tear down under an in-flight request), the process ends itself with
    status 0, and `state.json` is intact and holds the emptied workspace.

    Status 0 is the observable half of the flush-on-drop requirement: the
    run loop returned, which is what drops `App` and runs `Drop::drop`'s
    fsync-ing `workspace.flush()` — a killed or panicking process could not
    produce it. (The flush itself logs `workspace state flushed on
    shutdown` at INFO; `ui.launch`'s `RUST_LOG` floor — added for plan 039
    C7's signal-teardown assertion — means the exit code above is a
    redundant but not the only proof any more. Kept as the assertion here
    regardless: an exit code is a coarser signal that costs no log parsing.)
    """
    if target != "iced":
        pytest.skip(
            "exit-on-empty is an iced behavior this slice: mac already "
            "terminates on the last project's window close (the now-removed "
            "GTK UI kept its empty-workspace state instead — recorded "
            "divergence)"
        )
    if not runs_alone(request):
        pytest.skip(
            "ends the UI it drives, so it must be its own pytest "
            "invocation (`make e2e-iced-exit`) — collected beside other "
            "modules it would strand every test after it"
        )
    if not is_fresh():
        pytest.skip(
            "deletes every project in the workspace and ends the UI — "
            "requires a fresh, harness-owned instance (--roost-fresh)"
        )
    process = ui.owned_process(target)
    state_dir = ui.session_state_dir()
    assert process is not None and state_dir is not None, (
        "fresh mode must have launched the UI itself; without the child "
        "handle there is no way to observe the exit"
    )

    # A second project makes the "still alive with one project left" step
    # meaningful whatever the bootstrap seeded (normally exactly one).
    survivor = roost.create_project(name="pytest-exit-on-empty", cwd="/tmp")
    roost._wait(
        lambda: survivor in _project_ids(roost),
        4.0,
        "the extra project appears before anything is deleted",
    )

    for project_id in _project_ids(roost):
        if project_id == survivor:
            continue
        roost.delete_project(project_id)
    roost._wait(
        lambda: _project_ids(roost) == [survivor],
        5.0,
        "every project but the survivor is gone",
    )
    assert process.poll() is None, (
        "a non-empty workspace must not exit — exit is gated on the "
        "snapshot being empty, not on a project being deleted"
    )

    # The reply for THIS call is the assertion: `Roost.call` raises on an
    # error envelope and on a socket that closes mid-response, so a normal
    # return is proof the success frame was written before teardown.
    roost.delete_project(survivor)

    exit_code = process.wait(timeout=scaled_timeout(20.0))
    assert exit_code == 0, (
        f"the UI must end its run loop normally (exit {exit_code}); a "
        "non-zero status means the process was killed or panicked instead "
        "of dropping App and flushing state"
    )
    assert not ui.is_alive(target), "the IPC socket must not answer after the exit"

    state = json.loads((state_dir / "state.json").read_text())
    assert state["projects"] == [], (
        f"state.json must record the emptied workspace, got {state['projects']!r}"
    )
