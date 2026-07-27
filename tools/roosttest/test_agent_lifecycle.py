"""Agent lifecycle E2E — synthetic Claude hook payloads driven through
the REAL `roostctl claude-hook` binary, asserted via `tab.list`.

This is the one test that exercises the whole plan-002 path end to end:

    hook JSON on stdin
      -> roostctl claude-hook <Event>
      -> roost-agent's pure adapter (event -> reports)
      -> tab.agent_report over the IPC socket
      -> Workspace (ownership scoping + patch semantics)
      -> the derived `state` / `hook_active` / axis fields on `tab.list`

Everything below the CLI is unit-tested per layer (`roost-agent`,
`roost-ipc::agent`, `daemon::state`, the shared
`tests/agent-state-fixtures/`); only this file proves the layers are
actually wired to each other.

Conventions:

* The agent tab is always a **background** tab (a second tab is opened
  right after it, stealing active). Notification policy B (plan §3.5)
  drops a notification for the active tab of an active window, so an
  active agent tab would make every attention assertion depend on
  whether the runner's window happens to hold focus.
* The agent tab is a bare `bash --norc --noprofile`: no shell
  integration, therefore no OSC 133 marks, therefore a `shell_state`
  that only the test moves.
* Hook events are applied synchronously by the server before `roostctl`
  exits, but reads go over a second connection, so every assertion still
  polls through the harness's condition waits.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

import ui
from util import wait_tab_attached

REPO_ROOT = Path(__file__).resolve().parents[2]

TEST_MODE = os.environ.get("ROOST_TEST_MODE") == "1"

# `tab.state` is a closed four-value enum and must stay one (plan §3.2):
# the Swift decoders have no fallback case, so a fifth value throws on
# the Mac client. `AgentLifecycle::Failed` is observable only on the
# `agent_lifecycle` axis.
LEGACY_STATES = {"none", "running", "needs_input", "idle"}


# ---------------------------------------------------------------------------
# Driving the real CLI
# ---------------------------------------------------------------------------


def _roostctl() -> str:
    """Absolute path to a `roostctl` binary, building one if needed.

    Checked in the order a developer's tree makes them available: an
    explicit override, the cargo debug build (what CI's GTK job builds),
    the binary embedded in the Mac bundle (what CI's Mac job builds),
    then PATH. The cargo fallback mirrors `ui.launch`, which builds the
    GTK UI the same way when it's missing."""
    candidates = [
        os.environ.get("ROOST_ROOSTCTL", ""),
        str(REPO_ROOT / "target/debug/roostctl"),
        str(REPO_ROOT / "mac/build/Roost.app/Contents/Resources/bin/roostctl"),
        shutil.which("roostctl") or "",
    ]
    for path in candidates:
        if path and os.access(path, os.X_OK):
            return path
    subprocess.run(["cargo", "build", "-p", "roost-cli"], cwd=REPO_ROOT, check=True)
    return str(REPO_ROOT / "target/debug/roostctl")


def claude_hook(target: str, tab_id: int, event: str, payload: dict) -> None:
    """Run `roostctl claude-hook EVENT` exactly as Claude Code would:
    the payload on stdin, the tab + socket in the environment.

    `claude-hook` is fire-and-forget by contract (it always exits 0 so a
    Roost problem can never break the turn it fired from), so the only
    proof it worked is the state assertions the caller makes after."""
    body = {"cwd": "/tmp", "transcript_path": "/tmp/transcript.jsonl", **payload}
    env = {
        **os.environ,
        "ROOST_TAB_ID": str(tab_id),
        "ROOST_SOCKET": str(ui.socket_path(target)),
    }
    proc = subprocess.run(
        [_roostctl(), "claude-hook", event],
        input=json.dumps(body).encode(),
        capture_output=True,
        env=env,
        timeout=30,
    )
    assert proc.returncode == 0, (
        f"roostctl claude-hook {event} exited {proc.returncode}: "
        f"{proc.stderr.decode(errors='replace')}"
    )
    # Claude parses the hook's stdout as JSON, so every path — including
    # "no UI is listening" — must answer with an empty object.
    assert proc.stdout.strip() == b"{}", proc.stdout


def agent_tab(roost, project) -> int:
    """A bare-shell agent tab that is NOT the active tab (see module
    docstring), attached and ready to be fed PTY bytes."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.open_tab(project, cwd="/tmp")  # steals active, so the agent tab is background
    wait_tab_attached(roost, tab)
    return tab


# ---------------------------------------------------------------------------
# The full turn
# ---------------------------------------------------------------------------


def test_full_turn_through_the_real_hook_binary(roost, project, target):
    """SessionStart -> UserPromptSubmit -> Notification -> Stop ->
    SessionEnd, each through `roostctl claude-hook`, asserting both the
    new `agent_lifecycle` axis and the legacy `tab.state` projection."""
    tab = agent_tab(roost, project)
    session = "sess-full-turn"

    # SessionStart claims the tab but starts idle: a session that just
    # opened is not working yet (plan §3.8).
    claude_hook(target, tab, "SessionStart", {
        "session_id": session, "source": "startup", "model": "claude-opus-5",
    })
    roost._wait(lambda: roost.hook_active(tab), 5.0, "SessionStart claimed the tab")
    owner = roost.ownership(tab)
    assert owner is not None and owner["source"] == "claude", owner
    assert owner["session_id"] == session, owner
    # `metadata` is the additive channel (plan §3.6) — SessionStart's
    # model lands there rather than in a new named wire field.
    assert owner.get("metadata", {}).get("model") == "claude-opus-5", owner
    assert roost.agent_lifecycle(tab) == "inactive"
    assert roost.tab(tab)["state"] == "none"

    claude_hook(target, tab, "UserPromptSubmit", {
        "session_id": session, "prompt": "refactor the parser",
    })
    roost.wait_lifecycle(tab, "working")
    assert roost.tab(tab)["state"] == "running"

    claude_hook(target, tab, "Notification", {
        "session_id": session,
        "notification_type": "permission_prompt",
        "message": "Claude needs your permission to use Bash",
    })
    roost.wait_lifecycle(tab, "waiting")
    assert roost.tab(tab)["state"] == "needs_input"
    roost.wait_notification(tab, True)

    claude_hook(target, tab, "Stop", {"session_id": session, "stop_hook_active": False})
    roost.wait_lifecycle(tab, "finished")
    assert roost.tab(tab)["state"] == "idle"

    # SessionEnd releases: ownership is gone, the tab is no longer
    # hook_active, and derivation falls back to the (untouched) shell axis.
    claude_hook(target, tab, "SessionEnd", {"session_id": session, "reason": "clear"})
    roost._wait(lambda: not roost.hook_active(tab), 5.0, "SessionEnd released the tab")
    assert roost.ownership(tab) is None
    assert roost.agent_lifecycle(tab) == "inactive"
    assert roost.shell_state(tab) == "unknown"
    assert roost.tab(tab)["state"] == "none"


def test_stop_with_background_tasks_stays_working(roost, project, target):
    """`Stop` carrying in-flight `background_tasks` means "paused waiting
    on background work", not "turn over" — lifecycle stays `working`
    (plan §3.8 / AC 4). The array is by definition in-flight-only, so a
    non-empty array is the whole signal."""
    tab = agent_tab(roost, project)
    session = "sess-bg-tasks"
    claude_hook(target, tab, "SessionStart", {"session_id": session, "source": "startup"})
    claude_hook(target, tab, "UserPromptSubmit", {"session_id": session, "prompt": "go"})
    roost.wait_lifecycle(tab, "working")

    claude_hook(target, tab, "Stop", {
        "session_id": session,
        "stop_hook_active": False,
        "background_tasks": [
            {"id": "bg-1", "type": "shell", "status": "running", "description": "cargo test"},
        ],
        # A scheduled future wake is NOT in-flight work, so it must not
        # keep the turn open on its own.
        "session_crons": [{"id": "cron-1", "schedule": "@daily", "recurring": True,
                           "prompt": "check"}],
    })
    roost.wait_notification(tab, True)  # the Stop notification still fires
    assert roost.agent_lifecycle(tab) == "working", roost.tab(tab)
    assert roost.tab(tab)["state"] == "running"
    assert "background_tasks:1" in (roost.ownership(tab) or {}).get("detail", "")

    # …and a subsequent Stop with nothing in flight does finish the turn.
    claude_hook(target, tab, "Stop", {"session_id": session, "stop_hook_active": False})
    roost.wait_lifecycle(tab, "finished")
    assert roost.tab(tab)["state"] == "idle"


def test_stop_failure_is_failed_but_legacy_state_stays_closed(roost, project, target):
    """`StopFailure` (reading `error`, not `error_type`) produces the
    `failed` lifecycle — a value the legacy `tab.state` enum cannot
    express. This is the end-to-end half of AC 11: `state` must project
    to one of the four legacy values and NEVER to `"failed"`, or the
    Mac client's closed Swift enum throws on decode."""
    tab = agent_tab(roost, project)
    session = "sess-stop-failure"
    claude_hook(target, tab, "SessionStart", {"session_id": session, "source": "startup"})
    claude_hook(target, tab, "UserPromptSubmit", {"session_id": session, "prompt": "go"})
    roost.wait_lifecycle(tab, "working")

    claude_hook(target, tab, "StopFailure", {
        "session_id": session, "error": "rate_limit", "error_details": "429 from the API",
    })
    roost.wait_lifecycle(tab, "failed")
    state = roost.tab(tab)["state"]
    assert state in LEGACY_STATES, state
    assert state != "failed", "tab.state must stay a closed four-value enum"
    assert state == "needs_input", state
    assert (roost.ownership(tab) or {}).get("detail") == "rate_limit", roost.ownership(tab)
    roost.wait_notification(tab, True)

    # `failed` and `finished` are distinguishable on the axis even though
    # both are reachable from the same event pair (AC 4).
    claude_hook(target, tab, "Stop", {"session_id": session, "stop_hook_active": False})
    roost.wait_lifecycle(tab, "finished")


def test_unknown_notification_type_notifies_without_moving_lifecycle(roost, project, target):
    """`notification_type` is a free string, so unknown values are
    expected. They fire the notification but leave lifecycle alone: the
    state dot is sticky, and a false `waiting` (wrongly reading
    "blocked") is worse than a missed one (plan §3.8 / AC 3)."""
    tab = agent_tab(roost, project)
    session = "sess-unknown-notif"
    claude_hook(target, tab, "SessionStart", {"session_id": session, "source": "startup"})
    claude_hook(target, tab, "UserPromptSubmit", {"session_id": session, "prompt": "go"})
    roost.wait_lifecycle(tab, "working")
    assert not roost.has_notification(tab), "UserPromptSubmit clears attention"

    claude_hook(target, tab, "Notification", {
        "session_id": session,
        "notification_type": "some_future_type_roost_has_never_seen",
        "message": "something happened",
    })
    roost.wait_notification(tab, True)
    assert roost.agent_lifecycle(tab) == "working", roost.tab(tab)
    assert roost.tab(tab)["state"] == "running"


def test_event_carrying_agent_id_leaves_lifecycle_unchanged(roost, project, target):
    """An event that fired inside a subagent describes the subagent's
    turn, not the tab owner's — its notification still reaches the user
    but it must not move the owner's lifecycle (plan §3.8 / AC 5)."""
    tab = agent_tab(roost, project)
    session = "sess-agent-id"
    claude_hook(target, tab, "SessionStart", {"session_id": session, "source": "startup"})
    claude_hook(target, tab, "UserPromptSubmit", {"session_id": session, "prompt": "go"})
    roost.wait_lifecycle(tab, "working")

    # Without `agent_id` this exact payload would set `waiting`.
    claude_hook(target, tab, "Notification", {
        "session_id": session,
        "agent_id": "subagent-7",
        "notification_type": "permission_prompt",
        "message": "the subagent wants Bash",
    })
    roost.wait_notification(tab, True)
    assert roost.agent_lifecycle(tab) == "working", roost.tab(tab)


def test_report_from_a_foreign_session_is_dropped(roost, project, target):
    """Ownership identity is the pair `(source, session_id)`; a report
    that doesn't match the current owner is dropped (plan §3.3 / AC 6).

    The no-sleep proof is an ordering barrier: a *matching* event sent
    after the foreign one is applied after it, so once the barrier's
    effect is visible, the foreign event has either landed or been
    dropped — and lifecycle says which."""
    tab = agent_tab(roost, project)
    session = "sess-owner"
    claude_hook(target, tab, "SessionStart", {"session_id": session, "source": "startup"})
    claude_hook(target, tab, "UserPromptSubmit", {"session_id": session, "prompt": "go"})
    roost.wait_lifecycle(tab, "working")

    claude_hook(target, tab, "Notification", {
        "session_id": "sess-someone-else",
        "notification_type": "permission_prompt",
        "message": "not from the owner",
    })
    # Barrier: an owner event that fires a notification but (unknown
    # type) leaves lifecycle alone.
    claude_hook(target, tab, "Notification", {
        "session_id": session,
        "notification_type": "auth_success",
        "message": "signed in",
    })
    roost.wait_notification(tab, True)
    assert roost.agent_lifecycle(tab) == "working", (
        "the foreign session's permission_prompt must not have set `waiting`"
    )

    # Same rule, read straight off the op's own reply.
    result = roost.agent_report(tab, "claude", "preserve", session_id="sess-someone-else",
                                lifecycle="waiting")
    assert result["accepted"] is False, result
    assert roost.agent_lifecycle(tab) == "working"


def test_legacy_prompt_submit_spelling_still_works(roost, project, target):
    """`claude install` used to write the kebab-case `prompt-submit`
    spelling into `claude-settings.json`; every already-installed
    settings file still calls it. It must keep reaching the same arm as
    the canonical `UserPromptSubmit`."""
    tab = agent_tab(roost, project)
    session = "sess-legacy-spelling"
    claude_hook(target, tab, "session-start", {"session_id": session, "source": "startup"})
    roost._wait(lambda: roost.hook_active(tab), 5.0, "legacy session-start claimed the tab")

    claude_hook(target, tab, "prompt-submit", {"session_id": session, "prompt": "go"})
    roost.wait_lifecycle(tab, "working")
    assert roost.tab(tab)["state"] == "running"


def test_set_hook_active_alias_still_claims_and_releases(roost, project):
    """The deprecated `tab.set_hook_active` alias (plan §3.6): `true`
    claims as source `legacy`, `false` releases. Installed hook scripts
    and older `roostctl` builds still call it, so it stays wired."""
    tab = agent_tab(roost, project)
    roost.set_hook_active(tab, True)
    roost._wait(lambda: roost.hook_active(tab), 5.0, "set_hook_active(True) claimed")
    assert (roost.ownership(tab) or {}).get("source") == "legacy", roost.ownership(tab)
    # Ownership alone doesn't move the tab's state — the axes are
    # independent, and no lifecycle was reported.
    assert roost.agent_lifecycle(tab) == "inactive"

    roost.set_hook_active(tab, False)
    roost._wait(lambda: not roost.hook_active(tab), 5.0, "set_hook_active(False) released")
    assert roost.ownership(tab) is None


# ---------------------------------------------------------------------------
# The D / A failsafe (plan §3.4, AC 8)
# ---------------------------------------------------------------------------


@pytest.mark.skipif(
    not TEST_MODE,
    reason="feeding OSC 133 / OSC 9 needs ROOST_TEST_MODE=1 in the UI's launch env",
)
def test_prompt_mark_deactivates_a_dead_agent_and_reopens_raw_osc(roost, project):
    """The failsafe that stops a killed agent muting a tab forever.

    An agent claims the tab and goes `working`, which suppresses raw OSC
    9/99/777 (the agent reports its own attention, so a wrapper shell
    echoing OSC 9 would double-notify). If that agent is then killed,
    nothing releases ownership — so the shell reaching a prompt does it:
    an OSC 133 `D` sets lifecycle `inactive` while KEEPING ownership as a
    label, which makes derivation fall through to the shell axis and
    re-opens raw OSC in the same move."""
    tab = agent_tab(roost, project)
    roost.agent_report(tab, "claude", "claim", session_id="sess-failsafe",
                       lifecycle="working")
    roost.wait_lifecycle(tab, "working")
    assert roost.tab(tab)["state"] == "running"

    # Raw OSC is suppressed while the agent drives the tab.
    roost.tab_feed_pty_bytes(tab, b"\x1b]9;muted while claude works\x07")
    # Ordering barrier: OSC 133 D is fed after the OSC 9 and both take
    # the same drain, so once D's effect is visible the OSC 9 has already
    # been decided on.
    roost.tab_feed_pty_bytes(tab, b"\x1b]133;D;0\x1b\\")
    roost.wait_lifecycle(tab, "inactive")
    assert not roost.has_notification(tab), "raw OSC must be suppressed under a live agent"

    # Ownership survives as a label; only the lifecycle dropped.
    owner = roost.ownership(tab)
    assert owner is not None and owner["source"] == "claude", owner
    assert roost.hook_active(tab) is True
    # Derivation now falls through to the shell axis.
    assert roost.shell_state(tab) == "at_prompt"
    assert roost.tab(tab)["state"] == "none"

    # …and raw OSC works again.
    roost.tab_feed_pty_bytes(tab, b"\x1b]9;build complete\x07")
    roost.wait_notification(tab, True)
