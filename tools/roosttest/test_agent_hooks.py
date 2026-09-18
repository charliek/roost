"""Agent hook E2E — the five adapters replayed through the REAL
`roostctl agent-hook <agent>` binary, asserted via `tab.list`.

`test_agent_lifecycle.py` proves the Claude path end to end with
hand-written payloads and the legacy `claude-hook EVENT` verb. This
module proves the *generic* verb (plan 046 §3.2) and the four adapters
that only reach a running UI through it, and it does so with the
**captured** payloads — `crates/roost-agent/tests/fixtures/<agent>.jsonl`
is the scrubbed 2026-09-04 probe of a real session per agent, the same
file `roost-agent`'s `fixture_replay_test.rs` pins the pure mapping
against. Same bytes, two layers:

    <agent>.jsonl payload on stdin
      -> roostctl agent-hook <agent>       (event read from the payload)
      -> roost-agent's pure adapter        (unit-tested on these bytes)
      -> tab.agent_report over the IPC socket
      -> Workspace (ownership scoping, lifecycle_if guards)
      -> agent_lifecycle + ownership.source on tab.list

The row tables below deliberately mirror the ones in
`fixture_replay_test.rs`, so a mapping change has to be made twice — once
in the pure replay and once against a live UI — and a drift in either is
loud.

Conventions are `test_agent_lifecycle.py`'s — its `agent_tab` is
imported outright (read its docstring): the agent tab is a **background**
tab so notification policy does not depend on window focus, it runs a
bare `bash --norc --noprofile` so no OSC 133 mark the test did not send
can move the lifecycle, and every read is a condition wait rather than a
sleep.

The second half of the file is the other direction entirely: `roostctl
agent ensure`/`set`/`install`/`uninstall` and a UI's own startup wiring,
driven against a **jailed `$HOME`** (plan 046 §3.9, extended by plan 064
§5 C5 for the list-shaped `agent-hooks` key and the `set` verb). Those
lanes write agent config files, so read the fence comment above them
before adding to them — nothing in this suite may reach a real dotfile.
"""

from __future__ import annotations

import fcntl
import json
import os
import socket as socketlib
import subprocess
import threading
import tempfile
from pathlib import Path

import pytest
import ui
from agent_jail import (
    INSTALLABLE_AGENTS,
    Jail,
    jailed_socket,
    jailed_ui,
    wait_for_jailed_window,
    wait_for_log_line,
)
from client import Roost, scaled_timeout
from test_agent_lifecycle import agent_tab
from util import HOOK_DEADLINE, REPO_ROOT, config_value, roostctl_path, run_hook

FIXTURES = REPO_ROOT / "crates/roost-agent/tests/fixtures"

# Session ids as they appear in the captures. Same constants as
# `fixture_replay_test.rs`; ownership identity is the `(source,
# session_id)` pair, so asserting them here is what proves the reports
# arrived scoped rather than merely arrived.
CLAUDE_SESSION_TWO = "eed354f6-c5c7-4e10-ad32-fe6a8d343225"
GROK_SESSION_THREE = "01a06e3e-2d6b-7f13-bc74-f86b6c947e08"
CODEX_SESSION = "01a06e4d-b178-7f53-bbc3-f9e551c3b56b"
CURSOR_SESSION = "206da977-c2d4-4b1f-a280-29c6e27ea973"
OPENCODE_SESSION = "ses_f91cef768ffeTI8TEd0E4v53Ov"

# The bus events `assets/opencode/roost-agent-state.js` forwards.
# MIRRORS `roost_agent::opencode::OPENCODE_HOOK_EVENTS`; the count
# assertion in `opencode_forwarded` is what catches a drift.
OPENCODE_FORWARDED = (
    "session.created",
    "chat.message",
    "session.status",
    "permission.asked",
    "permission.replied",
    "question.asked",
    "question.replied",
    "session.idle",
    "session.error",
    "dispose",
)


# ---------------------------------------------------------------------------
# Driving the real CLI
# ---------------------------------------------------------------------------

# `agent_hook`'s default socket, which is the running UI's — distinct
# from `None`, which means "run with ROOST_SOCKET unset".
_POINTED_AT_THE_UI = object()


def agent_hook(
    target: str,
    tab_id: int,
    agent: str,
    payload: dict,
    socket: object = _POINTED_AT_THE_UI,
    args: list[str] | None = None,
) -> float:
    """Run `roostctl agent-hook <agent>` with NO event on the command
    line — the verb reads `hook_event_name` out of the payload, which is
    what lets one installed command string serve every event an agent
    has.

    `socket=None` runs with `ROOST_SOCKET` unset, `args` prepends global
    flags; both are for the target-resolution lane below, and everything
    else passes the running UI's socket.

    `util.run_hook` holds the rest (payload on stdin, tab + socket in the
    environment, and the always-`{}`-always-0-inside-the-budget contract
    a decision hook's dialog depends on), so the only proof this worked
    is the state assertions the caller makes after."""
    if socket is _POINTED_AT_THE_UI:
        socket = ui.socket_path(target)
    return run_hook(
        [*(args or []), "agent-hook", agent],
        tab_id,
        socket,
        json.dumps(payload).encode(),
    )


def fixture(agent: str) -> list[tuple[str, dict]]:
    """`<agent>.jsonl` as `(event, payload)` pairs, in capture order."""
    path = FIXTURES / f"{agent}.jsonl"
    records = []
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        records.append((record["event"], record["payload"]))
    return records


def expect(
    roost,
    tab: int,
    agent: str,
    where: str,
    lifecycle: str,
    owner: str | None,
    detail: str | None = None,
) -> None:
    """Wait until the tab holds `lifecycle`, is owned by `(agent, owner)`
    — or by nobody when `owner` is None — and the owner record's `detail`
    reads `detail`.

    One predicate over all three axes rather than three waits: a report
    that landed with the right lifecycle under the *wrong* ownership is
    the failure this suite exists to catch (grok and cursor both execute
    Claude-format hooks), and sequential waits could each pass on a
    different poll.

    `detail` is what makes a row *discriminating*. Most events in a turn
    move the tab to `working`, so a wait on lifecycle alone is satisfied
    by the state its predecessor already established — drop the event
    entirely and the assertion still passes. Every adapter writes a
    per-event `detail`, and `detail` merges even when a `lifecycle_if`
    guard vetoes the patch, so it is the one field that says *this*
    report arrived. Same idea as `test_agent_lifecycle.wait_detail`."""

    def settled() -> bool:
        state = roost.tab(tab) or {}
        if state.get("agent_lifecycle") != lifecycle:
            return False
        ownership = state.get("ownership")
        if owner is None:
            return ownership is None
        return (
            ownership is not None
            and ownership.get("source") == agent
            and ownership.get("session_id") == owner
            and (detail is None or ownership.get("detail") == detail)
        )

    roost._wait(
        settled,
        5.0,
        f"{where} -> {lifecycle} owned by {agent}/{owner} detail={detail}",
    )


def replay(roost, target, tab: int, agent: str, rows, payloads=None) -> None:
    """Drive `rows` — `(index, event, lifecycle, owner, detail)` against
    `<agent>.jsonl` — through the real verb, asserting every axis after
    every single event.

    `index` is a column rather than an implied position so a row names
    which captured line it is, exactly like the Rust replay's rows.
    `detail` is the column that makes the row discriminating — see
    [`expect`]. A row whose event maps to *no* report repeats its
    predecessor's detail, which is the honest answer: "changes nothing"
    is exactly what those rows are driven to prove, and nothing
    observable can distinguish them from not having been sent."""
    records = payloads if payloads is not None else fixture(agent)
    for index, want_event, want_lifecycle, want_owner, want_detail in rows:
        event, payload = records[index]
        assert event == want_event, f"{agent}.jsonl[{index}] is {event!r}, not {want_event!r}"
        agent_hook(target, tab, agent, payload)
        expect(
            roost,
            tab,
            agent,
            f"{agent}.jsonl[{index}] {event}",
            want_lifecycle,
            want_owner,
            want_detail,
        )


# ---------------------------------------------------------------------------
# The five replay lanes (plan 046 W1)
# ---------------------------------------------------------------------------


def test_claude_replays_through_the_generic_verb(roost, project, target):
    """Claude through `agent-hook claude` rather than `claude-hook EVENT`:
    same adapter, event taken from the payload instead of argv.

    The capture's second session is the one that hit a permission dialog,
    so this lane carries the whole arc including `waiting` — and the
    `PostToolUse` that follows an approval is the regression test for the
    orange-after-approval defect (there is no second `PreToolUse`)."""
    tab = agent_tab(roost, project)
    session = CLAUDE_SESSION_TWO
    replay(roost, target, tab, "claude", [
        (9, "SessionStart", "inactive", session, "startup"),
        (10, "UserPromptSubmit", "working", session, "user_prompt_submit"),
        (11, "PreToolUse", "working", session, "pre_tool_use"),
        (12, "PermissionRequest", "waiting", session, "permission_request"),
        (13, "PostToolUse", "working", session, "post_tool_use"),
        (14, "Stop", "finished", session, "stop"),
        # Roost registers no `SubagentStop`; the adapter maps none either,
        # so this is the "an unmapped event changes nothing" case driven
        # through the real binary — the one row whose detail is its
        # predecessor's, because changing nothing is the whole point.
        (15, "SubagentStop", "finished", session, "stop"),
        (16, "UserPromptSubmit", "working", session, "user_prompt_submit"),
        (17, "SessionEnd", "inactive", None, None),
    ])
    assert roost.hook_active(tab) is False


def test_grok_replays_through_the_generic_verb(roost, project, target):
    """grok's third captured session — the one that hit plan mode, which
    is grok's only blocked signal (a `notification` carrying
    `notificationType: permission_prompt`). Its payloads name their event
    in camelCase, which the verb has to read as readily as Claude's."""
    tab = agent_tab(roost, project)
    session = GROK_SESSION_THREE
    replay(roost, target, tab, "grok", [
        (19, "SessionStart", "inactive", session, "new"),
        (20, "UserPromptSubmit", "working", session, "user_prompt_submit"),
        (21, "PreToolUse", "working", session, "pre_tool_use"),
        (22, "PostToolUse", "working", session, "post_tool_use"),
        (23, "PreToolUse", "working", session, "pre_tool_use"),
        (24, "Notification", "waiting", session, "permission_prompt"),
        (25, "PostToolUse", "working", session, "post_tool_use"),
        (26, "PreToolUse", "working", session, "pre_tool_use"),
        (27, "StopCancelled", "finished", session, "stop_cancelled"),
        (28, "UserPromptSubmit", "working", session, "user_prompt_submit"),
        (29, "SessionEnd", "inactive", None, None),
        # Every grok session ends `SessionEnd` then a trailing
        # `Stop{reason: shutdown}`. The adapter cannot know ownership was
        # just released, so the *server* drops it — which is only
        # observable against a live UI, i.e. here.
        (30, "Stop", "inactive", None, None),
    ])


def test_codex_replays_through_the_generic_verb(roost, project, target):
    """codex's capture, whole. Charlie's codex runs with approvals off, so
    this session never reaches `waiting`; what it does carry is
    `Interrupt` — the Esc signal that ends a turn without bannering."""
    tab = agent_tab(roost, project)
    session = CODEX_SESSION
    replay(roost, target, tab, "codex", [
        (0, "SessionStart", "inactive", session, "startup"),
        (1, "UserPromptSubmit", "working", session, "user_prompt_submit"),
        (2, "PostToolUse", "working", session, "post_tool_use"),
        (3, "Stop", "finished", session, "stop"),
        (4, "UserPromptSubmit", "working", session, "user_prompt_submit"),
        (5, "PostToolUse", "working", session, "post_tool_use"),
        (6, "Interrupt", "finished", session, "interrupt"),
        (7, "SessionEnd", "inactive", None, None),
    ])


def test_cursor_replays_through_the_generic_verb(roost, project, target):
    """cursor's capture, whole — camelCase event names, three `stop`s for
    two turns, and never a `waiting` (cursor has no permission hook at
    all, plan §4).

    The five zero-report lines are driven rather than skipped: they are
    events Roost deliberately does not register, and the proof they cost
    nothing is that the real binary can be handed them. They are also the
    rows that repeat their predecessor's `detail`, for exactly that
    reason."""
    tab = agent_tab(roost, project)
    session = CURSOR_SESSION
    replay(roost, target, tab, "cursor", [
        (0, "sessionStart", "inactive", session, "session_start"),
        (1, "beforeSubmitPrompt", "working", session, "before_submit_prompt"),
        (2, "afterAgentThought", "working", session, "before_submit_prompt"),
        (3, "preToolUse", "working", session, "pre_tool_use"),
        (4, "afterAgentThought", "working", session, "pre_tool_use"),
        (5, "beforeShellExecution", "working", session, "pre_tool_use"),
        (6, "afterShellExecution", "working", session, "pre_tool_use"),
        (7, "postToolUse", "working", session, "post_tool_use"),
        (8, "afterAgentThought", "working", session, "post_tool_use"),
        (9, "afterAgentThought", "working", session, "post_tool_use"),
        (10, "afterAgentResponse", "working", session, "after_agent_response"),
        # cursor's `stop` carries its raw status in `detail`, which is
        # what tells the three of them apart.
        (11, "stop", "finished", session, "completed"),
        (12, "beforeSubmitPrompt", "working", session, "before_submit_prompt"),
        # Esc: `aborted` ends the turn, and the `error` right behind it is
        # the same interrupt reported twice — vetoed by `lifecycle_if`,
        # which is why two turns produce two banners and not three. The
        # veto drops the lifecycle patch, not the report, so `error`
        # still merges its detail — the proof it arrived at all.
        (13, "stop", "finished", session, "aborted"),
        (14, "stop", "finished", session, "error"),
        (15, "sessionEnd", "inactive", None, None),
    ])


def opencode_forwarded() -> list[tuple[str, dict]]:
    """The capture as the plugin hands it to the verb.

    opencode has no command hooks: `assets/opencode/roost-agent-state.js`
    subscribes to the plugin event bus and forwards a whitelist of it as
    `{...event.properties, hook_event_name: event.type, session_id: <root
    session>}`. The JS→Rust seam itself is covered by
    `crates/roost-agent/tests/opencode_plugin_test.rs` (a stub hook
    records argv + stdin); what is untested until here is that same
    forwarded shape reaching a live UI, so this rebuilds it from the raw
    bus log rather than re-testing the plugin.

    A synthetic `dispose` is appended: the probe never observed opencode
    calling its teardown hook (`opencode.rs`'s module doc), so the capture
    alone cannot show ownership being released — and a lane that leaves a
    tab owned would not have shown the whole arc."""
    records = [
        (event, {**payload, "hook_event_name": event, "session_id": OPENCODE_SESSION})
        for event, payload in fixture("opencode")
        if event in OPENCODE_FORWARDED
    ]
    assert len(records) == 19, f"opencode.jsonl or the whitelist changed: {len(records)}"
    records.append(
        ("dispose", {"hook_event_name": "dispose", "session_id": OPENCODE_SESSION})
    )
    return records


def test_opencode_replays_through_the_generic_verb(roost, project, target):
    """opencode's forwarded bus, whole. `permission.asked` is its blocked
    signal; `session.status idle` is a level rather than an edge and must
    map to nothing; `session.idle` is what ends a turn, and its two
    repeats after the Esc are vetoed by the guard."""
    tab = agent_tab(roost, project)
    session = OPENCODE_SESSION
    replay(roost, target, tab, "opencode", [
        (0, "session.created", "inactive", session, "session_created"),
        (1, "chat.message", "working", session, "chat_message"),
        (2, "session.status", "working", session, "session_status"),
        (3, "session.status", "working", session, "session_status"),
        (4, "permission.asked", "waiting", session, "permission_asked"),
        (5, "permission.replied", "working", session, "permission_replied"),
        (6, "session.status", "working", session, "session_status"),
        (7, "session.status", "working", session, "session_status"),
        (8, "session.status", "working", session, "session_status"),
        # `idle`: the level that maps to nothing, so the detail is the
        # one the `busy` above left.
        (9, "session.status", "working", session, "session_status"),
        (10, "session.idle", "finished", session, "session_idle"),
        (11, "chat.message", "working", session, "chat_message"),
        (12, "session.status", "working", session, "session_status"),
        (13, "session.status", "working", session, "session_status"),
        # Esc. `MessageAbortedError` arrives on the same channel as a real
        # failure and is the one value that must not paint the tab red.
        (14, "session.error", "finished", session, "message_aborted"),
        (15, "session.status", "finished", session, "message_aborted"),
        # Guarded on working/waiting, so the lifecycle patch is vetoed
        # and only the detail lands — which is what proves the two
        # trailing idles reached the state machine at all.
        (16, "session.idle", "finished", session, "session_idle"),
        (17, "session.status", "finished", session, "session_idle"),
        (18, "session.idle", "finished", session, "session_idle"),
        (19, "dispose", "inactive", None, None),
    ], payloads=opencode_forwarded())


# ---------------------------------------------------------------------------
# The verb's own contract (plan 046 §3.2)
# ---------------------------------------------------------------------------


def test_an_unknown_agent_answers_cleanly_and_changes_nothing(roost, project, target):
    """A config left behind by a newer Roost names an agent this binary
    has no adapter for. It must drain stdin, answer `{}`, exit 0 — and
    above all not disturb the session that owns the tab.

    The inert call carries the capture's **SessionEnd**, not one of its
    working events, and that choice is the whole test. A barrier is only
    a barrier if the call in front of it cannot reach the same state: a
    `UserPromptSubmit` under `amp` that quietly behaved would land on
    `working`, which is exactly where the barrier then puts the tab, and
    nothing would fail. A `SessionEnd` that behaved would *release
    ownership* — after which the barrier is dropped by the server for
    naming an owner the tab no longer has, and this test times out."""
    tab = agent_tab(roost, project)
    session = CLAUDE_SESSION_TWO
    records = fixture("claude")
    agent_hook(target, tab, "claude", records[9][1])
    expect(roost, tab, "claude", "SessionStart", "inactive", session, "startup")

    agent_hook(target, tab, "amp", records[17][1])
    agent_hook(target, tab, "claude", records[10][1])
    expect(
        roost, tab, "claude", "UserPromptSubmit", "working", session, "user_prompt_submit"
    )


def test_a_payload_naming_no_event_changes_nothing(roost, project, target):
    """The event comes from the payload, so a body without one is the
    generic verb's version of an unrecognized event: inert, and still
    `{}` on stdout.

    Same barrier construction as above, and for the same reason — the
    body stripped of its event name is the capture's `SessionEnd`, so a
    verb that guessed an event from anywhere else would release ownership
    and strand the barrier behind it."""
    tab = agent_tab(roost, project)
    session = CLAUDE_SESSION_TWO
    records = fixture("claude")
    agent_hook(target, tab, "claude", records[9][1])
    expect(roost, tab, "claude", "SessionStart", "inactive", session, "startup")

    nameless = {k: v for k, v in records[17][1].items() if k != "hook_event_name"}
    assert nameless.get("session_id") == session, "the stripped body must still be owned"
    agent_hook(target, tab, "claude", nameless)
    agent_hook(target, tab, "claude", records[10][1])  # barrier, as above
    expect(
        roost, tab, "claude", "UserPromptSubmit", "working", session, "user_prompt_submit"
    )


def test_the_verb_reports_only_into_the_socket_it_was_pointed_at(roost, project, target):
    """`ROOST_TAB_ID` is only meaningful to the Roost that spawned the
    tab, so `agent-hook` must dial `ROOST_SOCKET` (or an explicit
    `--socket`) and never fall back to a bundle profile's default path.

    The failure this pins is not theoretical: a wrapper that strips the
    environment down to `ROOST_TAB_ID` (`env -i`, sudo, a sanitized
    launcher) would otherwise send a `SessionStart` to whichever Roost
    owns the default path — a *different* window — where an unconditional
    claim evicts tab 7's real owner for an identity no release can match.

    This UI is reachable at that default path (`ui.socket_path`), so the
    unsocketed call below would land if the fallback were still there;
    the barrier construction is the one the two tests above explain."""
    tab = agent_tab(roost, project)
    session = CLAUDE_SESSION_TWO
    records = fixture("claude")
    agent_hook(target, tab, "claude", records[9][1])
    expect(roost, tab, "claude", "SessionStart", "inactive", session, "startup")

    # `--target` names the profile whose default socket this UI is on,
    # so the general resolver would answer with a live path here.
    agent_hook(
        target, tab, "claude", records[17][1], socket=None, args=["--target", target]
    )
    agent_hook(target, tab, "claude", records[10][1])
    expect(
        roost, tab, "claude", "UserPromptSubmit", "working", session, "user_prompt_submit"
    )

    # …and the explicit flag still works, which is the override the
    # fallback's removal must not take with it.
    agent_hook(
        target,
        tab,
        "claude",
        records[11][1],
        socket=None,
        args=["--socket", str(ui.socket_path(target))],
    )
    expect(roost, tab, "claude", "PreToolUse via --socket", "working", session, "pre_tool_use")


def test_a_socket_that_never_answers_cannot_hold_a_decision_hook():
    """Both verbs are bounded, and by the same numbers.

    A UI that accepted the connection and then wedged (a stuck main
    thread, a paused process) leaves an unbounded hook waiting forever —
    and `PermissionRequest` is in `CLAUDE_HOOK_EVENTS`, so `claude
    install` writes a *decision* hook onto `claude-hook` too. The dialog
    the user is looking at is blocked on this process the whole time.

    Driven against a socket that accepts and never replies, which is the
    one shape a connect timeout alone does not cover — no UI needed, and
    deliberately none used."""
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "silent.sock")
        listener = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
        listener.bind(path)
        # Backlogged, never accepted: `connect(2)` succeeds and the
        # request that follows is answered by nobody.
        listener.listen(4)
        try:
            budget = scaled_timeout(HOOK_DEADLINE)
            for verb in (
                ["agent-hook", "claude"],
                ["claude-hook", "PermissionRequest"],
            ):
                elapsed = run_hook(
                    verb,
                    7,
                    path,
                    json.dumps(
                        {"hook_event_name": "PermissionRequest", "session_id": "s-1"}
                    ).encode(),
                )
                assert elapsed < budget, (
                    f"roostctl {' '.join(verb)} held a decision dialog for "
                    f"{elapsed:.1f}s against a socket that never answers"
                )
        finally:
            listener.close()


def test_the_verb_answers_even_when_its_own_output_is_gone():
    """`{}` on stdout and exit 0 hold even when stdout — or, under
    `ROOST_DEBUG`, stderr — has been closed out from under the process.

    Rust ignores SIGPIPE, so a write to a vanished reader comes back as
    an error rather than a signal, and `println!`/`eprintln!` turn that
    error into a panic: exit 101 with no JSON at all, which is the one
    shape a decision hook may read as a block. A hook fires inside
    whatever process tree the agent has; a reader that has already gone
    is a normal Tuesday, not a bug in the caller.

    A pipe with *no reader at all* rather than a closed descriptor:
    Rust's runtime re-opens fds 0/1/2 onto `/dev/null` before `main`, so
    handing the process a closed fd 1 tests nothing. A pipe whose read
    end the parent drops is the real shape anyway — the agent went
    away."""
    payload = json.dumps({"hook_event_name": "SessionStart", "session_id": "s-1"}).encode()
    env = {**os.environ, "ROOST_TAB_ID": "7"}
    env.pop("ROOST_SOCKET", None)

    def run_with_readerless(stream: str, extra_env: dict[str, str]) -> subprocess.Popen:
        """Spawn `agent-hook amp` with `stream` wired to a pipe nobody
        reads. `amp` has no adapter, so no socket work happens and the
        only thing left to do is answer."""
        read_fd, write_fd = os.pipe()
        streams = {"stdout": subprocess.PIPE, "stderr": subprocess.PIPE}
        streams[stream] = write_fd
        proc = subprocess.Popen(
            [roostctl_path(), "agent-hook", "amp"],
            stdin=subprocess.PIPE,
            env={**env, **extra_env},
            **streams,
        )
        # Both ends dropped here, so the child's very first write to that
        # stream comes back EPIPE. Rust ignores SIGPIPE, which is what
        # turns it into an error `println!` would panic on.
        os.close(write_fd)
        os.close(read_fd)
        return proc

    with_stdout_gone = run_with_readerless("stdout", {})
    _, stderr = with_stdout_gone.communicate(payload, timeout=scaled_timeout(HOOK_DEADLINE))
    assert with_stdout_gone.returncode == 0, (
        f"exited {with_stdout_gone.returncode} writing `{{}}` into a pipe nobody reads: "
        f"{stderr.decode(errors='replace')}"
    )

    # And with `ROOST_DEBUG` set, so the verb has something to say on
    # stderr — which it must not say *before* the `{}` it owes stdout.
    with_stderr_gone = run_with_readerless("stderr", {"ROOST_DEBUG": "1"})
    stdout, _ = with_stderr_gone.communicate(payload, timeout=scaled_timeout(HOOK_DEADLINE))
    assert with_stderr_gone.returncode == 0, (
        f"exited {with_stderr_gone.returncode} logging into a pipe nobody reads"
    )
    assert stdout.strip() == b"{}", stdout


def test_a_payload_over_the_cap_is_drained_rather_than_abandoned():
    """The 1 MiB cap bounds what is *parsed*, never what is *read*.

    `take(CAP).read_to_end(..)` declares EOF at exactly the cap and
    leaves the rest in the pipe, so the agent — writing into it right now
    — gets an EPIPE the moment this process exits. Same for an early
    return that never reads at all: the legacy verb used to check
    `ROOST_TAB_ID` first and close the pipe without consuming a byte.

    Both are asserted from the writer's side, which is the only side that
    can tell the difference."""
    # Valid JSON and comfortably over the cap; what is asserted is the
    # writer's `write`, not what the adapter made of the payload.
    oversized = (
        b'{"hook_event_name":"Stop","session_id":"s-1","pad":"'
        + b"x" * (2 * 1024 * 1024)
        + b'"}'
    )
    small = json.dumps({"hook_event_name": "Stop", "session_id": "s-1"}).encode()
    # Bigger than a pipe buffer, so an unread pipe blocks the writer
    # rather than swallowing the payload whole.
    unread = small + b" " * (512 * 1024)

    cases = [
        # (verb, ROOST_TAB_ID, payload) — the over-cap read, then the
        # early return that used to precede any read at all.
        (["agent-hook", "claude"], "7", oversized),
        (["claude-hook", "Stop"], "7", oversized),
        (["claude-hook", "Stop"], None, unread),
        (["agent-hook", "claude"], None, unread),
    ]
    for verb, tab_id, payload in cases:
        # No UI: what is under test is the read, and a dial that fails at
        # once keeps the assertion about the pipe and nothing else.
        env = {**os.environ, "ROOST_SOCKET": "/nonexistent/roost-drain-test.sock"}
        env.pop("ROOST_TAB_ID", None)
        if tab_id is not None:
            env["ROOST_TAB_ID"] = tab_id
        proc = subprocess.Popen(
            [roostctl_path(), *verb],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        broken = None
        try:
            proc.stdin.write(payload)
            proc.stdin.flush()
        except BrokenPipeError as error:
            broken = error
        finally:
            try:
                proc.stdin.close()
            except BrokenPipeError:
                broken = broken or BrokenPipeError()
            # `communicate` flushes `proc.stdin` again if it is still set,
            # and flushing an already-closed file raises `ValueError` on
            # Linux/CPython 3.12. Detaching it is the documented way to
            # say "stdin is already dealt with".
            proc.stdin = None
        stdout, _ = proc.communicate(timeout=scaled_timeout(HOOK_DEADLINE))
        named = f"roostctl {' '.join(verb)} (ROOST_TAB_ID={tab_id})"
        assert broken is None, f"{named} stopped reading and broke the writer's pipe"
        assert proc.returncode == 0, f"{named} exited {proc.returncode}"
        assert stdout.strip() == b"{}", f"{named}: {stdout!r}"


def test_a_spawned_tab_carries_the_hook_entrypoint(roost, project):
    """Plan 046 W2: every tab Roost spawns is told where the one hook
    entrypoint lives, so an installed agent config can name
    `"$ROOST_AGENT_HOOK"` instead of a path that is wrong on every other
    machine.

    Read out of the tab's own environment rather than off the UI's state,
    because the value only matters if it reached the child's `execve`."""
    tab = agent_tab(roost, project)
    # The whole check runs in the shell rather than being parsed back out
    # here: the value is an absolute path long enough to wrap an 80-column
    # viewport, so anything that reads it off the screen reads half of it.
    # `printf "HOOK%s"` is what keeps the verdict distinguishable from the
    # shell's echo of the command that produced it.
    roost.run(
        tab,
        'case "$ROOST_AGENT_HOOK" in '
        '/*roostctl) [ -x "$ROOST_AGENT_HOOK" ] '
        '&& printf "HOOK%s\\n" OK || printf "HOOK%s\\n" BAD-not-executable ;; '
        '*) printf "HOOK%s [%s]\\n" BAD "$ROOST_AGENT_HOOK" ;; '
        "esac",
    )
    roost._wait(
        lambda: "HOOKOK" in roost.dump_text(tab) or "HOOKBAD" in roost.dump_text(tab),
        10.0,
        "the probe reported on ROOST_AGENT_HOOK",
    )
    text = roost.dump_text(tab)
    assert "HOOKOK" in text, f"ROOST_AGENT_HOOK failed the probe:\n{text}"


# ---------------------------------------------------------------------------
# `roostctl agent ensure` against a jailed $HOME — plan 046 C7.
#
# THE JAIL IS THE POINT OF THIS SECTION. Everything above drives hooks at
# a running UI and writes nothing; everything below writes into agent
# config files, and a bug in any of it would otherwise land in the
# developer's own `~/.claude/settings.json`. Three fences, all required:
#
#   1. The harness's `fixtures/launcher.conf` says `agent-hooks = off`,
#      so no UI the suite launches wires anything (plan 046 §3.9).
#   2. `roost-agent-install` refuses to run under `ROOST_TEST_MODE=1`
#      unless `ROOST_AGENT_HOOKS_FORCE=1` is also set. Two files in the
#      tree set that override — this one and `test_host_client.py`'s
#      remote-wiring case — and
#      `test_the_test_mode_fence_refuses_without_the_override` is what
#      proves the fence is still there for everyone else.
#   3. Every process started below runs with `HOME`, `XDG_CONFIG_HOME`
#      and all five agent-directory variables pointed inside a tempdir,
#      **asserted immediately before the spawn** by `Jail.assert_jailed`
#      — on the merged environment, not on the overrides, so an inherited
#      value that survived the merge is caught rather than assumed away.
#
# `Jail` itself lives in `agent_jail.py`, because `test_host_client.py`
# jails the `roost-session` it spawns with the same helper.
# ---------------------------------------------------------------------------


def agent_env(jail: Jail, *, force: bool, socket: str | None) -> dict:
    """The environment a jailed `roostctl agent …` runs in. Assert it
    with `Jail.assert_jailed` immediately before every spawn."""
    env = {**os.environ, **jail.env}
    env["ROOST_TEST_MODE"] = "1"
    if force:
        env["ROOST_AGENT_HOOKS_FORCE"] = "1"
    else:
        env.pop("ROOST_AGENT_HOOKS_FORCE", None)
    # `roostctl` inherits the harness's environment; a tab id or socket
    # left in it would point these verbs at the running UI.
    for leaked in ("ROOST_TAB_ID", "ROOST_SOCKET"):
        env.pop(leaked, None)
    if socket is not None:
        env["ROOST_SOCKET"] = socket
    env["ROOST_CONFIG"] = str(jail.config)
    return env


def run_agent(jail: Jail, *args: str, force: bool = True, socket: str | None = None):
    """`roostctl agent …` inside `jail`. Never `check=True`: several
    cases assert on a non-zero exit, and a failure's stdout is the most
    useful thing in the report.

    `socket` is for the one verb that dials — bare `agent set`, the
    UI-routed form — and must name a jailed UI's socket: pointing it at
    the harness's own UI would ask a process whose `$HOME` is the
    developer's to write agent files."""
    env = agent_env(jail, force=force, socket=socket)
    jail.assert_jailed(env)
    return subprocess.run(
        [roostctl_path(), "agent", *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=scaled_timeout(60),
    )


def run_agent_async(jail: Jail, *args: str, socket: str | None = None):
    """[`run_agent`] left running, for the one case that has to send a
    second request while the first is still in flight."""
    env = agent_env(jail, force=True, socket=socket)
    jail.assert_jailed(env)
    return subprocess.Popen(
        [roostctl_path(), "agent", *args],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def ensure_json(jail: Jail) -> dict:
    done = run_agent(jail, "ensure", "--json")
    assert done.returncode == 0, f"agent ensure failed: {done.stdout}{done.stderr}"
    return json.loads(done.stdout)


def test_agent_ensure_wires_a_jailed_home(tmp_path):
    """The install engine driven end to end through the real binary: five
    present agents wired, the state record written, a second run planning
    nothing, and `uninstall --all` taking it back out.

    The per-agent file *shapes* are the install crate's own inline tests
    (plan 046 §3.9). What this adds is that `roostctl agent` — argument
    parsing, config read, `Home::from_env`, the lock, the record — works
    as one program against a real filesystem."""
    jail = Jail(tmp_path)
    assert not jail.record.exists()

    first = ensure_json(jail)
    assert sorted(first["wired"]) == sorted(INSTALLABLE_AGENTS), first
    assert first["errors"] == [], first

    record = jail.read_record()
    assert sorted(record) == sorted(INSTALLABLE_AGENTS), record
    for agent in INSTALLABLE_AGENTS:
        entry = record[agent]
        # False until a UI has shown the toast. Nothing here shows one.
        assert entry["noticed"] is False, entry
        assert entry["by"] == "local", entry
        assert entry["files"], entry
        for path in jail.owned_files(agent):
            assert path.is_relative_to(jail.root), f"{agent} wrote outside the jail: {path}"
            assert path.exists(), f"{agent}: {path} was recorded but not written"

    # The command Roost installs names the env-indirected entrypoint and
    # the agent it speaks for — never an absolute Roost path (W2).
    claude_settings = (jail.agent_dirs["claude"] / "settings.json").read_text()
    assert "ROOST_AGENT_HOOK" in claude_settings
    assert "agent-hook claude" in claude_settings
    assert str(REPO_ROOT) not in claude_settings

    # Idempotent: everything is current, nothing is wired again, and the
    # record is not rewritten (mtime, because "wrote nothing" is the
    # claim — the plan's own assertion is zero planned edits).
    stamp = jail.record.stat().st_mtime_ns
    second = ensure_json(jail)
    assert second["wired"] == [], second
    assert sorted(second["current"]) == sorted(INSTALLABLE_AGENTS), second
    assert jail.record.stat().st_mtime_ns == stamp

    rows = {row["agent"]: row for row in json.loads(run_agent(jail, "status", "--json").stdout)}
    for agent in INSTALLABLE_AGENTS:
        assert rows[agent]["present"] is True, rows[agent]
        assert rows[agent]["wired"] is not None, rows[agent]
        assert rows[agent]["up_to_date"] is True, rows[agent]
        assert rows[agent]["noticed"] is False, rows[agent]
        # `allowed` is the resolved `agent-hooks` key's own claim, kept
        # apart from `wired`/`up_to_date` (the disk's claim) — every
        # agent here is both, since the jail's default key names all
        # five (plan 064 §5 C5).
        assert rows[agent]["allowed"] is True, rows[agent]
    # codex is the one adapter split across two files (`hooks.json` +
    # `config.toml`'s `[features] hooks`); `files` is what an uninstall
    # touches and what the consent sheet names, so both paths have to be
    # in it.
    assert len(rows["codex"]["files"]) == 2, rows["codex"]

    # Read the file list off the record before it is dropped: it is the
    # only thing that knows which of the five layouts wrote what.
    wrote = {agent: jail.owned_files(agent) for agent in INSTALLABLE_AGENTS}
    removed = run_agent(jail, "uninstall", "--all")
    assert removed.returncode == 0, removed.stdout + removed.stderr
    assert jail.read_record() == {}, "uninstall left entries behind"
    for agent, paths in wrote.items():
        for path in paths:
            if path.exists():
                assert "ROOST_AGENT_HOOK" not in path.read_text(), (
                    f"{agent}: uninstall left Roost's entry in {path}"
                )


def test_agent_hooks_off_wires_nothing_and_leaves_the_record_absent(tmp_path):
    """`agent-hooks = off`, read from a real `config.conf` by the real
    binary: with nothing already wired, `off` writes **nothing at all**
    — not even an empty state record, which is what makes the key safe
    to leave in the harness's own config."""
    jail = Jail(tmp_path, agent_hooks="off")

    quiet = ensure_json(jail)
    assert quiet["wired"] == [] and quiet["removed"] == [], quiet
    assert not jail.record.exists(), "`off` with nothing to remove still wrote the record"
    assert not (jail.agent_dirs["claude"] / "settings.json").exists()


def test_agent_set_local_wires_and_unwires_exactly_the_named_agents(tmp_path):
    """`roostctl agent set <list|off> --local` (plan 064 §5 C5): a comma
    list writes the key and wires exactly what it names — nothing else
    on this machine — and `off` unwires it again.

    The list form is asserted against two named agents with three more
    present but unnamed, so "exactly" is the thing under test rather
    than "wires everything present"."""
    jail = Jail(tmp_path, agent_hooks=None)
    assert jail.read_key() is None

    wired = run_agent(jail, "set", "claude,codex", "--local", "--json")
    assert wired.returncode == 0, wired.stdout + wired.stderr
    outcome = json.loads(wired.stdout)
    assert sorted(outcome["wired"]) == ["claude", "codex"], outcome
    assert jail.read_key() == "claude, codex"
    assert (jail.agent_dirs["claude"] / "settings.json").exists()
    assert (jail.agent_dirs["codex"] / "hooks.json").exists()
    # grok is present in the jail (`INSTALLABLE_AGENTS` default) but was
    # never named, so `set` must have left it alone.
    assert not any(jail.agent_dirs["grok"].iterdir()), "set wired an agent it was not given"

    off = run_agent(jail, "set", "off", "--local", "--json")
    assert off.returncode == 0, off.stdout + off.stderr
    outcome = json.loads(off.stdout)
    assert sorted(outcome["removed"]) == ["claude", "codex"], outcome
    assert jail.read_key() == "off"
    assert jail.read_record() == {}, "off left the record naming an agent it unwired"
    for agent, filename in (("claude", "settings.json"), ("codex", "hooks.json")):
        path = jail.agent_dirs[agent] / filename
        assert not path.exists() or "ROOST_AGENT_HOOK" not in path.read_text()


def test_agent_set_refuses_an_empty_or_unknown_list_and_writes_nothing(tmp_path):
    """An empty spec and an unrecognised name are the two shapes `set`
    must refuse before it writes anything — a partial answer to a
    consent question is not an answer (plan 064 §5 C5)."""
    jail = Jail(tmp_path, agent_hooks=None)

    for spec in ("", "banana"):
        refused = run_agent(jail, "set", spec, "--local")
        assert refused.returncode == 2, f"set {spec!r} --local: {refused.stdout}{refused.stderr}"
        assert not refused.stdout.strip(), f"set {spec!r} --local wrote to stdout: {refused.stdout}"

    assert jail.read_key() is None, "a refused `set` changed the key"
    assert not jail.record.exists(), "a refused `set` wrote the state record"


def test_agent_install_and_uninstall_move_the_key(tmp_path):
    """`agent install <name>` unions the key rather than ignoring it, and
    `agent uninstall <name>` narrows it back — `uninstall --all` is the
    one shape that spells the result `off` rather than leaving the key
    unanswered (plan 064 §5 C5's `install`/`uninstall` cases)."""
    jail = Jail(tmp_path, agent_hooks=None)

    installed = run_agent(jail, "install", "codex")
    assert installed.returncode == 0, installed.stdout + installed.stderr
    assert jail.read_key() == "codex", "explicit `agent install` did not add codex to the key"
    assert (jail.agent_dirs["codex"] / "hooks.json").exists()

    uninstalled = run_agent(jail, "uninstall", "codex")
    assert uninstalled.returncode == 0, uninstalled.stdout + uninstalled.stderr
    assert jail.read_key() == "off", "narrowing the key to nothing must spell it `off`"
    assert jail.read_record() == {}
    hooks = jail.agent_dirs["codex"] / "hooks.json"
    assert not hooks.exists() or "ROOST_AGENT_HOOK" not in hooks.read_text()

    run_agent(jail, "install", "claude")
    run_agent(jail, "install", "codex")
    assert jail.read_key() == "claude, codex"
    all_out = run_agent(jail, "uninstall", "--all")
    assert all_out.returncode == 0, all_out.stdout + all_out.stderr
    assert jail.read_key() == "off"
    assert jail.read_record() == {}


def test_a_config_warning_never_lands_on_the_json_channel(tmp_path):
    """`--json` is decoded by the Mac app; a diagnostic must not precede it.

    The retired `agent-hooks = auto` spelling is exactly the value that
    warns now (plan 064 §3.1), and it is what every machine that ran the
    old default still has in its config — so this is the common case, not
    an exotic one. `roostctl` logs to stderr for this reason; on stdout
    the warning would sit in front of the JSON and break the decode."""
    jail = Jail(tmp_path, agent_hooks="auto")

    done = run_agent(jail, "ensure", "--json")
    assert done.returncode == 0, done.stderr
    json.loads(done.stdout)
    assert "agent-hooks" in done.stderr, (
        "the unparseable value was not diagnosed anywhere"
    )
    assert jail.read_key() == "auto", "a warned-about value was rewritten"


def test_agent_ensure_under_an_unanswered_key_is_a_quiet_no_op(tmp_path):
    """No `agent-hooks` key at all — nobody has answered the consent
    dialog — is a no-op: `ensure` writes nothing, exits 0, and its
    `--json` output is valid JSON (plan 064 C1 fixed a build where prose
    leaked onto stdout and broke the decode; this pins it)."""
    jail = Jail(tmp_path, agent_hooks=None)
    assert jail.read_key() is None

    outcome = ensure_json(jail)  # raises if stdout does not parse as JSON
    assert outcome == {
        "wired": [],
        "refreshed": [],
        "current": [],
        "removed": [],
        "skipped": [],
        "warnings": [],
        "errors": [],
    }, outcome
    assert not jail.record.exists(), "an unanswered key still wrote the state record"
    for agent in INSTALLABLE_AGENTS:
        assert not any(jail.agent_dirs[agent].iterdir()), f"{agent}: wired under an unanswered key"


def test_ensure_startup_leaves_a_hand_wired_agent_alone_but_bare_ensure_sweeps_it(tmp_path):
    """The pair plan 064 §5 C5 asks for: `--startup` is what a UI launch
    runs, and a launch must never undo a hook somebody added by hand or
    react to a key that changed while the app was closed — only the
    explicit, no-flag `ensure` (`reconcile` underneath) takes an agent
    back out once the key stops naming it.

    Built by installing two agents and then hand-lowering the key to
    name only one — the same shape a user editing `config.conf` in an
    editor produces, and the one thing the Mac launch spawn (`ensure
    --startup`) must not be able to undo."""
    jail = Jail(tmp_path, agent_hooks=None)
    run_agent(jail, "install", "claude")
    run_agent(jail, "install", "grok")
    assert jail.read_key() == "claude, grok"
    grok_files = jail.owned_files("grok")
    assert grok_files, "grok wired nothing to hand-lower away from"

    jail.write_config(agent_hooks="claude")
    assert jail.read_key() == "claude"

    startup = run_agent(jail, "ensure", "--startup", "--json")
    assert startup.returncode == 0, startup.stdout + startup.stderr
    startup_outcome = json.loads(startup.stdout)
    assert startup_outcome["removed"] == [], startup_outcome
    for path in grok_files:
        assert path.exists() and "ROOST_AGENT_HOOK" in path.read_text(), (
            f"--startup undid a hand-wired agent's entry: {path}"
        )

    bare = ensure_json(jail)
    assert bare["removed"] == ["grok"], bare
    for path in grok_files:
        assert not path.exists() or "ROOST_AGENT_HOOK" not in path.read_text(), (
            f"bare ensure left grok's entry behind: {path}"
        )


def test_the_test_mode_fence_refuses_without_the_override(tmp_path):
    """`ROOST_TEST_MODE=1` alone must stop the install engine dead.

    This is the fence that protects every OTHER lane in this suite — none
    of them sets `ROOST_AGENT_HOOKS_FORCE`, so if this ever stops being
    true, a harness UI could reach a real dotfile. Asserted inside the
    jail, so proving it costs nothing."""
    jail = Jail(tmp_path)
    refused = run_agent(jail, "ensure", force=False)
    assert refused.returncode != 0, refused.stdout
    assert not jail.record.exists(), "the refusal still wrote the state record"
    assert not (jail.agent_dirs["claude"] / "settings.json").exists()


# ---------------------------------------------------------------------------
# The UI's own startup ensure — plan 046 C7, §3.7.
#
# These launch a SECOND, fully jailed Roost rather than driving the
# harness's session UI, because the thing under test is what a UI does at
# *launch* and the session UI launched before the test existed. The jail
# moves the socket too (`$HOME/Library/Caches` on macOS,
# `$XDG_RUNTIME_DIR` on Linux), so the second instance has its own
# socket, its own single-instance locks and its own `state.json`, and
# cannot disturb the one every other module in this file is driving.
# ---------------------------------------------------------------------------


@pytest.fixture
def iced_only(target):
    if target != "iced":
        pytest.skip("the startup ensure is asserted against the iced UI's own launch")


def test_the_ui_wires_agent_hooks_at_startup_and_notices_once(short_root, iced_only):
    """Plan 046 W6/W7 against a real launch: the UI wires every present
    agent in a jailed home, says so once, and flips `noticed`.

    The toast is read out of the UI's log, not inferred from `noticed`.
    `noticed` alone would be a circular assertion — the UI writes it
    itself, right beside the line it is supposed to be evidence for — so
    the text is asserted directly. `agent hooks toast shown` is logged
    at the one place the banner takes the message
    (`App::show_agent_hooks_toast`), which runs at the **end** of the
    engine-feed drain, after every other `set_status` that batch can
    reach. So the line is also the proof the sentence survived its own
    drain instead of being replaced by, say, a PTY error that arrived
    with it.

    What it does not prove is that a frame was painted; nothing
    observable from here does (the status banner reaches no IPC op, and
    the screenshot harness reads pixels, not text). `noticed` then
    carries the once-per-machine half: `ensure` reports an agent as
    unannounced only while the record says so."""
    jail = Jail(short_root)

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        toast = wait_for_log_line(
            log,
            "agent hooks toast shown",
            "the jailed UI to put the agent-hooks toast on the banner",
        )
        # Agent order is `ALL_AGENTS`, which `INSTALLABLE_AGENTS` mirrors.
        assert f"for {', '.join(INSTALLABLE_AGENTS)}" in toast, toast
        # Roost has just edited five of the user's config files. The way
        # back has to be in the sentence that says so (plan 064 §3.4).
        assert "Agent Hooks\u2026" in toast, toast
        Roost._wait(
            lambda: jail.record.exists()
            and all(
                entry.get("noticed") is True for entry in jail.read_record().values()
            ),
            30.0,
            f"the jailed UI to record the toast in {jail.record}",
        )
        # Read after the process is gone (the context manager waits for
        # its exit), so the flip cannot still be in flight.

    record = jail.read_record()
    assert sorted(record) == sorted(INSTALLABLE_AGENTS), record
    for agent in INSTALLABLE_AGENTS:
        assert record[agent]["noticed"] is True, (
            f"{agent} was wired but the toast was never recorded as shown: {record[agent]}"
        )
        assert record[agent]["by"] == "local", record[agent]
        for path in jail.owned_files(agent):
            assert path.is_relative_to(jail.root), f"{agent} wrote outside the jail: {path}"
            assert path.exists(), f"{agent}: {path} was recorded but not written"

    settings = (jail.agent_dirs["claude"] / "settings.json").read_text()
    assert "ROOST_AGENT_HOOK" in settings and "agent-hook claude" in settings

    # One ensure per process, however many window events the compositor
    # sent: `window_opened` also runs on every focus *and* unfocus, so
    # without the latch a launch that gets a Focused event runs the whole
    # five-agent wiring twice. How many window events arrive is the
    # compositor's business, which is why the latch itself is pinned
    # deterministically in `agent_hooks.rs`
    # (`the_startup_ensure_runs_once_per_process`); this is the same
    # claim against a real one.
    body = log.read_text(errors="replace")
    assert body.count("agent hooks startup ensure finished") == 1, body

    # And the second launch of the same machine says nothing at all. This
    # is the other half of "once": the ensure finds every agent current,
    # and `noticed` is what keeps it quiet. Asserted after the process is
    # gone, so an absent line is an absence rather than a race.
    with jailed_ui(jail) as (proc, second_log):
        wait_for_jailed_window(jail, proc, second_log)
        # The ensure is off-thread, so wait for the line it logs whether
        # or not it has anything to announce — the drain that reports it
        # is the same one that would toast.
        Roost._wait(
            lambda: "agent hooks startup ensure finished"
            in second_log.read_text(errors="replace"),
            30.0,
            "the jailed UI's second startup ensure to report",
        )
    body = second_log.read_text(errors="replace")
    assert "agent hooks toast shown" not in body, body
    assert jail.read_record() == record, "a silent relaunch rewrote the record"


def test_the_ui_wires_nothing_when_agent_hooks_is_off(short_root, iced_only):
    """`agent-hooks = off` stops the startup ensure before it opens a
    file (W6).

    This is the key the harness's own `fixtures/launcher.conf` sets, so
    it is what stands between every other lane in this suite and the
    developer's real `~/.claude/settings.json`. `off` at startup does not
    *remove* anything either — a launch is not an instruction — which the
    empty jail also shows: the record is never created."""
    jail = Jail(short_root, agent_hooks="off")

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)

    assert not jail.record.exists(), "`agent-hooks = off` still wrote the state record"
    for agent in INSTALLABLE_AGENTS:
        contents = sorted(p.name for p in jail.agent_dirs[agent].iterdir())
        assert contents == [], f"`off` wrote into {agent}'s config dir: {contents}"


def test_agent_set_without_local_goes_through_the_running_ui(short_root, iced_only):
    """Bare `agent set` is the UI-routed form (plan 064 C6): it puts
    `agent.set_hooks` to the running UI, which writes the key and
    reconciles the files in *that* process's home.

    Driven against a jailed UI rather than the harness's own, because
    this verb really writes: the jailed launch is the one place in the
    tree that lifts the install engine's test-mode refusal, and
    everything it can reach is inside the jail.

    The offline probe first is what proves the routing rather than the
    effect. With nothing listening the verb has to fail — a `set` that
    quietly fell back to `--local` would have written the key, the
    record and claude's own file from exactly this argument.
    """
    jail = Jail(short_root, agent_hooks=None)
    sock = jailed_socket(jail)

    offline = run_agent(jail, "set", "claude,codex", socket=str(sock))
    assert offline.returncode != 0, offline.stdout + offline.stderr
    assert jail.read_key() is None, "with no UI to dial, `set` wrote the local key anyway"
    assert not jail.record.exists(), "with no UI to dial, `set` wired this machine anyway"

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        applied = run_agent(jail, "set", "claude,codex", "--json", socket=str(sock))
        assert applied.returncode == 0, applied.stdout + applied.stderr
        reply = json.loads(applied.stdout)
        assert reply["config_path"] == str(jail.config), reply
        assert sorted(reply["local"]["wired"]) == ["claude", "codex"], reply
        assert reply["local"]["errors"] == [], reply
        # Nothing is saved in this UI's own registry, so there is no
        # host to raise and the list is empty rather than absent.
        assert reply["hosts"] == [], reply
        # The receipt, read off the log for `wait_for_log_line`'s reason.
        toast = wait_for_log_line(
            log,
            "agent hooks toast shown",
            "the jailed UI to put the agent.set_hooks receipt on the banner",
        )
        assert "for claude, codex" in toast, toast
        assert "Agent Hooks…" in toast, toast

    # Read after the UI has exited, so nothing is still in flight.
    assert jail.read_key() == "claude, codex"
    assert "ROOST_AGENT_HOOK" in (jail.agent_dirs["claude"] / "settings.json").read_text()
    # grok is present in the jail but was never named: "exactly this
    # list" is the claim, not "everything installed".
    assert not any(jail.agent_dirs["grok"].iterdir()), "the UI wired an agent nobody named"


def test_agent_set_hooks_skips_a_name_this_build_cannot_wire(short_root, iced_only):
    """Plan 065 §3.1 on the UI socket, the sibling of the host op's rule.

    Driven over IPC rather than through `roostctl agent set`, which
    parses its spec locally and so can never put an unknown name on the
    wire — only a newer client can, which is the case this exists for.

    Two arms. A mixed list applies the half this build knows and reports
    the rest; a list of *only* unknown names answers `ok` having written
    nothing at all — no key, no dotfile — because an error there is a
    client that can never grow past it.
    """
    jail = Jail(short_root, agent_hooks=None)
    sock = jailed_socket(jail)

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with Roost(str(sock), timeout=scaled_timeout(30)) as roost:
            alone = roost.call("agent.set_hooks", {"agents": ["gemini", "amp"]})
            assert alone["local"]["wired"] == [], alone
            assert alone["local"]["skipped"] == [
                {"agent": "gemini", "reason": "unknown"},
                {"agent": "amp", "reason": "unknown"},
            ], alone
            assert alone["hosts"] == [], alone
            assert jail.read_key() is None, "an unknown-only list wrote the key"
            assert not jail.record.exists(), "an unknown-only list wired something"

            mixed = roost.call("agent.set_hooks", {"agents": ["claude", "gemini"]})
            assert mixed["local"]["wired"] == ["claude"], mixed
            assert {"agent": "gemini", "reason": "unknown"} in mixed["local"]["skipped"], mixed

    # Read after the UI has exited, so nothing is still in flight. The
    # key is what was applied: `agent.set_hooks` replaces it, so the name
    # it could not act on is not carried into an answer the user gave.
    assert jail.read_key() == "claude"
    assert "ROOST_AGENT_HOOK" in (jail.agent_dirs["claude"] / "settings.json").read_text()


def holds_open(pid: int, path: Path) -> bool:
    """Whether `pid` has `path` open.

    How a *contending* `ConfigLock` is observed. It opens the lock file
    and then polls `flock(LOCK_EX|LOCK_NB)`, and a non-blocking flock
    joins no queue — so a waiting writer never appears in `/proc/locks`
    and an open descriptor on the lock file is the only evidence the
    kernel offers that one got as far as the lock at all.
    """
    try:
        fds = list(Path(f"/proc/{pid}/fd").iterdir())
    except OSError:
        return False
    for fd in fds:
        try:
            if fd.resolve() == path:
                return True
        except OSError:
            continue
    return False


def test_two_processes_writing_config_conf_share_one_lock(short_root, iced_only):
    """#487: `config.conf` has more than one writer, and they all take
    the same `config.lock`.

    Two real processes, the pair a user actually produces: the UI
    rewriting `show-sidebar-agents` from the palette and `roostctl agent
    set --local` rewriting `agent-hooks`. Both are read-modify-write over
    the whole file, so without one lock between them a render built on a
    pre-image replaces the other's key wholesale — an atomic rename
    prevents a torn file, not a lost update.

    Contention is arranged rather than hoped for: this test holds
    `config.lock` itself (the same `flock` Rust takes), and waits until
    each writer has the lock file **open** before asserting that neither
    has written. "It has not finished yet" is also true of a process the
    scheduler has not run, so on a loaded machine that alone would pass
    over a build that took no lock at all. The loop afterwards is the
    same claim under real overlap.
    """
    if not Path("/proc/self/fd").is_dir():
        pytest.skip("proving a writer reached config.lock needs /proc")
    jail = Jail(short_root, agent_hooks="claude")
    sock = jailed_socket(jail)
    lock_path = jail.config.parent / "config.lock"
    rounds = 6

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        with Roost(str(sock), timeout=scaled_timeout(30)) as roost:

            def toggle() -> None:
                roost.palette_open()
                roost.palette_activate("toggle_sidebar_agents")

            handle = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
            try:
                fcntl.flock(handle, fcntl.LOCK_EX)
                toggle()
                roostctl = run_agent_async(jail, "set", "codex", "--local")
                Roost._wait(
                    lambda: holds_open(proc.pid, lock_path),
                    30.0,
                    "the UI's config writer to reach config.lock",
                )
                Roost._wait(
                    lambda: holds_open(roostctl.pid, lock_path),
                    30.0,
                    "roostctl to reach config.lock",
                )
                # Both are at the lock; now the assertion that neither
                # got past it.
                with pytest.raises(subprocess.TimeoutExpired):
                    roostctl.wait(timeout=scaled_timeout(2))
                assert config_value(jail.config, "show-sidebar-agents") is None, (
                    "the UI wrote config.conf while another process held config.lock:"
                    f"\n{jail.config.read_text()}"
                )
                assert jail.read_key() == "claude", jail.config.read_text()
            finally:
                fcntl.flock(handle, fcntl.LOCK_UN)
                os.close(handle)

            done = roostctl.communicate(timeout=scaled_timeout(60))
            assert roostctl.returncode == 0, done
            Roost._wait(
                lambda: config_value(jail.config, "show-sidebar-agents") is not None,
                15.0,
                "both held writers to land once the lock is free",
            )
            assert jail.read_key() == "codex", jail.config.read_text()

            # And under real overlap, neither key is ever missing.
            refused: list[str] = []

            def loop() -> None:
                for index in range(rounds):
                    wrote = run_agent(
                        jail, "set", "claude,codex" if index % 2 else "claude", "--local"
                    )
                    if wrote.returncode != 0:
                        refused.append(wrote.stdout + wrote.stderr)
                        return

            writer = threading.Thread(target=loop)
            writer.start()
            try:
                while writer.is_alive():
                    toggle()
                    assert jail.read_key() is not None, (
                        f"the UI's write lost `agent-hooks`:\n{jail.config.read_text()}"
                    )
                    assert config_value(jail.config, "show-sidebar-agents") is not None, (
                        f"roostctl's write lost `show-sidebar-agents`:"
                        f"\n{jail.config.read_text()}"
                    )
            finally:
                writer.join(timeout=scaled_timeout(120))
            assert not refused, refused

    # Read after the UI has exited, so nothing is still in flight.
    text = jail.config.read_text()
    assert jail.read_key() is not None, text
    assert config_value(jail.config, "show-sidebar-agents") in ("true", "false"), text


def test_two_overlapping_applies_land_in_request_order(short_root, iced_only):
    """#490: the file holds the answer the user gave **last**, not the
    one that finished last.

    Both applies are in flight at once: the first is held before it
    writes by the test-mode delay seam, and the second is sent while it
    is held. Independently spawned tasks would run the second to
    completion first and leave `claude` — the *older* answer — on disk.

    The receipt is asserted too, and it is the other half of the same
    rule: only the newest ticket reaches the banner, so the superseded
    apply says nothing at all.
    """
    jail = Jail(short_root, agent_hooks=None)
    sock = jailed_socket(jail)

    with jailed_ui(jail, extra_env={"ROOST_TEST_AGENT_HOOKS_DELAY_MS": "3000"}) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        first = run_agent_async(jail, "set", "claude", socket=str(sock))
        # A condition wait, not a sleep: the UI logs the ticket the
        # moment it takes it, which is what makes "the second was sent
        # while the first was in flight" true rather than likely.
        wait_for_log_line(log, "agent.set_hooks queued", "the first apply to be queued")
        second = run_agent(jail, "set", "codex", socket=str(sock))
        assert second.returncode == 0, second.stdout + second.stderr
        done = first.communicate(timeout=scaled_timeout(60))
        assert first.returncode == 0, done

        toast = wait_for_log_line(
            log,
            "agent hooks toast shown",
            "the jailed UI to put the agent.set_hooks receipt on the banner",
        )
        assert "for codex" in toast, toast

    assert jail.read_key() == "codex", jail.config.read_text()
    assert any(jail.agent_dirs["codex"].iterdir()), "the winning apply wired nothing"
    assert not any(jail.agent_dirs["claude"].iterdir()), (
        "the superseded apply left claude wired"
    )


def wait_for_agent_hooks_card(roost: Roost) -> dict:
    """Block until the preferences card is up, and return its dump.

    A condition wait rather than a settle: the card is raised from the
    engine feed, one status walk after the palette row was activated."""
    seen: list[dict] = []

    def carded() -> bool:
        card = roost.call("app.dialog_dump", {})
        if card.get("dialog") != "agent_hooks":
            return False
        seen.append(card)
        return True

    Roost._wait(carded, 30.0, "the agent-hooks card to open")
    assert seen[0]["mode"] == "preferences", seen[0]
    return seen[0]


def test_a_superseded_apply_still_moves_the_running_key(short_root, iced_only):
    """#490's other half: the UI's own `agent-hooks` value never lags the
    file.

    The apply that supersedes another can still *fail* — this one is
    refused before it writes anything, the shape a `config.lock` it never
    gets has — and then the newest answer on disk is the superseded one.
    A UI that discarded it on the grounds that "the newer apply already
    wrote the file" would hold the value it launched with while the file
    holds `claude`.

    The running value has exactly one surface: it is what the preferences
    card falls back to when `config.conf` cannot be **read**. So the file
    is made unreadable for the length of one card, which is the only way
    to tell the UI's copy and the file apart.
    """
    jail = Jail(short_root, agent_hooks="off", present=("claude", "codex"))
    sock = jailed_socket(jail)
    seams = {
        "ROOST_TEST_AGENT_HOOKS_DELAY_MS": "3000",
        "ROOST_TEST_AGENT_HOOKS_REFUSE_TICKET": "2",
    }

    with jailed_ui(jail, extra_env=seams) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        first = run_agent_async(jail, "set", "claude", socket=str(sock))
        wait_for_log_line(log, "agent.set_hooks queued", "the first apply to be queued")
        second = run_agent(jail, "set", "codex", socket=str(sock))
        assert second.returncode != 0, "the seam did not refuse the second apply"
        done = first.communicate(timeout=scaled_timeout(60))
        assert first.returncode == 0, done
        assert jail.read_key() == "claude", jail.config.read_text()

        jail.config.chmod(0o000)
        try:
            with Roost(str(sock), timeout=scaled_timeout(30)) as roost:
                roost.palette_dismiss()
                roost.palette_open()
                roost.palette_activate("agent_hooks")
                roost.palette_dismiss()
                card = wait_for_agent_hooks_card(roost)
        finally:
            jail.config.chmod(0o600)

    rows = {row["agent"]: row for row in card["rows"]}
    assert rows["claude"]["on"] is True, (
        f"the UI still holds the key it launched with, not the one on disk: {card}"
    )
    assert rows["codex"]["on"] is False, card


def test_a_failure_after_the_key_write_still_moves_the_running_key(short_root, iced_only):
    """#491: an apply that writes the key and then fails answers its
    caller with an error naming the key now on disk — and the running UI
    takes that key anyway.

    The failure is a real one rather than a seam: a directory where the
    state record belongs. Nothing reads the record before the key is
    written, so the run fails only once the key has landed.

    The running value is read the way the superseded-apply case above
    reads it: the preferences card falls back to it when `config.conf`
    cannot be read. A UI that dropped the failed apply would still hold
    the `off` it launched with.
    """
    jail = Jail(short_root, agent_hooks="off", present=("claude", "codex"))
    sock = jailed_socket(jail)

    with jailed_ui(jail) as (proc, log):
        wait_for_jailed_window(jail, proc, log)
        jail.record.mkdir()
        try:
            failed = run_agent(jail, "set", "claude", socket=str(sock))
        finally:
            jail.record.rmdir()
        assert failed.returncode != 0, failed.stdout + failed.stderr
        assert "the agent-hooks key `claude` is already on disk" in failed.stderr, (
            failed.stderr
        )
        assert jail.read_key() == "claude", jail.config.read_text()
        # Logged after the key is taken, so the card below cannot open
        # ahead of it.
        wait_for_log_line(
            log,
            "agent.set_hooks failed after writing the key",
            "the jailed UI to take the key a failed apply left on disk",
        )

        jail.config.chmod(0o000)
        try:
            with Roost(str(sock), timeout=scaled_timeout(30)) as roost:
                roost.palette_dismiss()
                roost.palette_open()
                roost.palette_activate("agent_hooks")
                roost.palette_dismiss()
                card = wait_for_agent_hooks_card(roost)
        finally:
            jail.config.chmod(0o600)

    rows = {row["agent"]: row for row in card["rows"]}
    assert rows["claude"]["on"] is True, (
        f"the UI still holds the key it launched with, not the one on disk: {card}"
    )
    assert rows["codex"]["on"] is False, card
