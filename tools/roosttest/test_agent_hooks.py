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
agent ensure` and a UI's own startup wiring, driven against a **jailed
`$HOME`** (plan 046 §3.9). Those lanes write agent config files, so read
the fence comment above them before adding to them — nothing in this
suite may reach a real dotfile.
"""

from __future__ import annotations

import contextlib
import json
import os
import platform
import shutil
import socket as socketlib
import subprocess
import tempfile
from pathlib import Path

import pytest
import ui
from client import Roost, RoostError, scaled_timeout
from test_agent_lifecycle import agent_tab
from util import HOOK_DEADLINE, REPO_ROOT, roostctl_path, run_hook

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
#      unless `ROOST_AGENT_HOOKS_FORCE=1` is also set. This file is the
#      one place in the tree that sets the override, and
#      `test_the_test_mode_fence_refuses_without_the_override` is what
#      proves the fence is still there for everyone else.
#   3. Every process started below runs with `HOME`, `XDG_CONFIG_HOME`
#      and all five agent-directory variables pointed inside a tempdir,
#      **asserted immediately before the spawn** by `Jail.assert_jailed`
#      — on the merged environment, not on the overrides, so an inherited
#      value that survived the merge is caught rather than assumed away.
# ---------------------------------------------------------------------------

# Agent name → the environment variable that relocates its config dir.
# MIRRORS `roost_agent_install::home::config_dir_env`; the five are what
# make the jail complete.
AGENT_CONFIG_DIR_ENV = {
    "claude": "CLAUDE_CONFIG_DIR",
    "codex": "CODEX_HOME",
    "grok": "GROK_HOME",
    "cursor": "CURSOR_CONFIG_DIR",
    "opencode": "OPENCODE_CONFIG_DIR",
}

# Report order of `roost_agent_install::ALL_AGENTS`.
INSTALLABLE_AGENTS = ("claude", "codex", "grok", "cursor", "opencode")

# The seven variables §3.9 pins. `XDG_CONFIG_HOME` is belt and braces:
# Roost's own state record is `$HOME/.config/roost/agent-hooks.json`
# whatever XDG says, so `HOME` already covers it — but a future move to
# the XDG dir must not silently unjail this suite.
JAIL_ENV_KEYS = ("HOME", "XDG_CONFIG_HOME", *AGENT_CONFIG_DIR_ENV.values())


class Jail:
    """A throwaway home with its own `config.conf`, its own agent config
    directories, and the environment that points every relevant tool at
    them."""

    def __init__(
        self,
        root,
        *,
        agent_hooks: str = "auto",
        skip: str | None = None,
        present=INSTALLABLE_AGENTS,
    ):
        self.root = root.resolve()
        self.home = self.root / "home"
        self.config = self.home / ".config/roost/config.conf"
        self.record = self.home / ".config/roost/agent-hooks.json"
        self.state_dir = self.root / "state"
        self.runtime_dir = self.root / "run"
        self.agent_dirs = {name: self.root / "agents" / name for name in AGENT_CONFIG_DIR_ENV}
        # Distinct log file per launch, so a relaunch's boot output does
        # not overwrite the evidence of the launch before it.
        self.launches = 0

        for name in present:
            self.agent_dirs[name].mkdir(parents=True, exist_ok=True)
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self._make_runtime_dir()
        self.write_config(agent_hooks=agent_hooks, skip=skip)

        self.env = {
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            **{
                AGENT_CONFIG_DIR_ENV[name]: str(path)
                for name, path in self.agent_dirs.items()
            },
        }

    def _make_runtime_dir(self) -> None:
        """A private `XDG_RUNTIME_DIR` for a jailed UI, so its socket and
        single-instance locks cannot collide with the session UI's.

        On the Wayland lane that also moves the compositor out of reach —
        `WAYLAND_DISPLAY` is a socket *name*, resolved against
        `XDG_RUNTIME_DIR` — so the real one is linked back in. Without
        this the jailed UI would fail to open a window on the weston
        lane, and only there."""
        self.runtime_dir.mkdir(parents=True, exist_ok=True)
        self.runtime_dir.chmod(0o700)
        display = os.environ.get("WAYLAND_DISPLAY", "")
        if not display or os.path.isabs(display):
            return
        real = Path(os.environ.get("XDG_RUNTIME_DIR", "")) / display
        link = self.runtime_dir / display
        if real.exists() and not link.exists():
            link.symlink_to(real)

    def write_config(self, *, agent_hooks: str, skip: str | None = None) -> None:
        self.config.parent.mkdir(parents=True, exist_ok=True)
        body = f"agent-hooks = {agent_hooks}\n"
        if skip is not None:
            body += f"agent-hooks-skip = {skip}\n"
        self.config.write_text(body)

    def assert_jailed(self, env: dict) -> None:
        """Every jail variable is set, absolute, and inside this root.

        Run on the *merged* environment a spawn is about to get, right
        before the spawn. An assertion on `self.env` would prove only
        that the dict was built correctly."""
        for key in JAIL_ENV_KEYS:
            value = env.get(key)
            assert value, f"{key} is not set: the jail is not in force"
            path = Path(value)
            assert path.is_absolute(), f"{key}={value} is not absolute"
            assert path.resolve().is_relative_to(self.root), (
                f"{key}={value} escapes the jail at {self.root}"
            )

    def read_record(self) -> dict:
        return json.loads(self.record.read_text())

    def owned_files(self, agent: str) -> list:
        """The files the state record says Roost wrote for `agent` — read
        back rather than hardcoded here, so the five per-agent layouts
        live in exactly one place (the install crate)."""
        return [Path(p) for p in self.read_record()[agent]["files"]]


def run_agent(jail: Jail, *args: str, force: bool = True):
    """`roostctl agent …` inside `jail`. Never `check=True`: several
    cases assert on a non-zero exit, and a failure's stdout is the most
    useful thing in the report."""
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
    env["ROOST_CONFIG"] = str(jail.config)
    jail.assert_jailed(env)
    return subprocess.run(
        [roostctl_path(), "agent", *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=scaled_timeout(60),
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


def test_agent_hooks_off_wires_nothing_and_unwires_on_request(tmp_path):
    """`agent-hooks = off`, read from a real `config.conf` by the real
    binary.

    Two halves, and they differ deliberately. With nothing wired, `off`
    writes **nothing at all** — not even an empty state record, which is
    what makes the key safe to leave in the harness's own config. Asked
    explicitly (`agent install`), Roost still wires: explicit wins over
    the config. A later `ensure` then reads the same `off` and removes
    what it put there — which is the difference between a config switch
    (opt out of future wiring) and the verb (take it out now)."""
    jail = Jail(tmp_path, agent_hooks="off")

    quiet = ensure_json(jail)
    assert quiet["wired"] == [] and quiet["removed"] == [], quiet
    assert not jail.record.exists(), "`off` with nothing to remove still wrote the record"
    assert not (jail.agent_dirs["claude"] / "settings.json").exists()

    forced = run_agent(jail, "install", "codex")
    assert forced.returncode == 0, forced.stdout + forced.stderr
    assert "codex" in jail.read_record(), "explicit `agent install` did not win over off"
    assert (jail.agent_dirs["codex"] / "hooks.json").exists()

    swept = ensure_json(jail)
    assert swept["removed"] == ["codex"], swept
    assert jail.read_record() == {}, "off left the record naming an agent it unwired"
    hooks = jail.agent_dirs["codex"] / "hooks.json"
    assert not hooks.exists() or "ROOST_AGENT_HOOK" not in hooks.read_text()


def test_agent_hooks_skip_is_honoured_and_a_typo_is_reported(tmp_path):
    """`agent-hooks-skip` keeps a named agent unwired; a name no agent
    answers to is reported on stderr and otherwise ignored.

    Ignoring it is the deliberate half: refusing to run would turn one
    typo into "nothing is wired and nothing says why", and a newer
    Roost's agent name must not break an older one's config."""
    jail = Jail(tmp_path, skip="codex, gemini")

    outcome = ensure_json(jail)
    assert "codex" not in outcome["wired"], outcome
    assert sorted(outcome["wired"]) == sorted(
        a for a in INSTALLABLE_AGENTS if a != "codex"
    ), outcome
    skipped = {row["agent"]: row["reason"] for row in outcome["skipped"]}
    assert "codex" in skipped, outcome
    assert not (jail.agent_dirs["codex"] / "hooks.json").exists()
    assert "codex" not in jail.read_record()

    done = run_agent(jail, "ensure")
    assert "gemini" in done.stderr, done.stderr


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


def jailed_ui_env(jail: Jail) -> dict:
    """The environment a jailed UI launch gets: the agent jail, XDG dirs
    inside it (so the socket, the log and the caches land there too), and
    the two variables that let the install engine run under
    `ROOST_TEST_MODE`."""
    env = {**os.environ}
    # Same list `ui.launch` strips, and for the same reason: per-tab
    # values Roost injects itself, plus the selectors set explicitly
    # below. Stripped first so the explicit values cannot be undone.
    for leaked in ui._UI_ENV_SANITIZE:
        env.pop(leaked, None)
    env.update(jail.env)
    env.update(
        {
            "XDG_RUNTIME_DIR": str(jail.runtime_dir),
            "XDG_DATA_HOME": str(jail.home / ".local/share"),
            "XDG_STATE_HOME": str(jail.home / ".local/state"),
            "XDG_CACHE_HOME": str(jail.home / ".cache"),
            "ROOST_BUNDLE_PROFILE": ui.TARGET_SPECS["iced"].profile,
            "ROOST_CONFIG": str(jail.config),
            "ROOST_STATE_DIR": str(jail.state_dir),
            "ROOST_TEST_MODE": "1",
            # The one place in the tree that lifts the install engine's
            # test-mode refusal. Everything it can reach is in the jail.
            "ROOST_AGENT_HOOKS_FORCE": "1",
            "RUST_LOG": os.environ.get("RUST_LOG", "warn") + ",roost_iced=info",
        }
    )
    return env


def jailed_socket(jail: Jail) -> Path:
    """Where a UI launched with `jailed_ui_env` binds. MIRRORS
    `ui.socket_path`, rooted in the jail rather than in `$HOME` /
    `$XDG_RUNTIME_DIR`."""
    spec = ui.TARGET_SPECS["iced"]
    if platform.system() == "Darwin":
        return jail.home / f"Library/Caches/{spec.mac_label}/roost.sock"
    return jail.runtime_dir / spec.linux_namespace / "roost.sock"


@contextlib.contextmanager
def jailed_ui(jail: Jail):
    """Launch a jailed iced UI, yield `(process, log path)`, and stop it.

    Teardown waits for the process to *exit* before the caller's
    assertions run against the jail. That is what makes "nothing was
    written" an assertion rather than a race: a dead process has no more
    writes left in it."""
    binary, explicit = ui.rust_binary_path("iced")
    if not binary.is_file():
        if explicit:
            pytest.skip(f"explicit iced binary does not exist: {binary}")
        subprocess.run(["cargo", "build", "-p", "roost-iced"], cwd=REPO_ROOT, check=True)

    env = jailed_ui_env(jail)
    jail.assert_jailed(env)
    log = jail.root / f"ui-{jail.launches}.log"
    jail.launches += 1
    with open(log, "wb") as handle:
        proc = subprocess.Popen(
            [str(binary)],
            cwd=REPO_ROOT,
            env=env,
            stdout=handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    try:
        yield proc, log
    finally:
        if proc.poll() is None:
            proc.terminate()
        try:
            proc.wait(timeout=scaled_timeout(20))
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=scaled_timeout(10))


def wait_for_jailed_window(jail: Jail, proc, log: Path) -> None:
    """Block until the jailed UI has a window on screen.

    `app.screenshot` is answered only once `window_id` is set, and that
    happens inside the same `window_opened` handler that decides whether
    to start the agent-hooks ensure — *before* it returns. So a
    screenshot that comes back proves the decision has been made, which
    is what lets the `off` case assert on an empty jail instead of racing
    a write that was never going to happen."""
    sock = jailed_socket(jail)

    def windowed() -> bool:
        if proc.poll() is not None:
            raise AssertionError(
                f"jailed UI exited {proc.returncode} before opening a window:\n"
                f"{log.read_text(errors='replace')}"
            )
        if not sock.exists():
            return False
        try:
            with Roost(str(sock), timeout=scaled_timeout(10)) as roost:
                roost.screenshot()
            return True
        except (OSError, RoostError):
            return False

    Roost._wait(windowed, 60.0, f"the jailed UI to open a window ({sock})")


def wait_for_log_line(log: Path, needle: str, what: str) -> str:
    """Block until a line of the jailed UI's log contains `needle`, and
    return that line.

    The log is how this module reads the status banner. No IPC op
    carries it — `app.*` exposes the menu, the dialogs and the sidebar's
    last-rendered rows, and `tab.dump` is the terminal grid; the
    transient line is drawn straight from `App::status` in `view` and is
    exposed nowhere else — so a test that wants to know what the banner
    says has the UI's own log and nothing better."""
    found: list[str] = []

    def seen() -> bool:
        for line in log.read_text(errors="replace").splitlines():
            if needle in line:
                found.append(line)
                return True
        return False

    Roost._wait(seen, 30.0, what)
    return found[0]


@pytest.fixture
def iced_only(target):
    if target != "iced":
        pytest.skip("the startup ensure is asserted against the iced UI's own launch")


@pytest.fixture
def short_root():
    """A jail root short enough to hold a Unix socket path.

    `sun_path` is 104 bytes on macOS, and pytest's `tmp_path` spends
    ~70 of them before this test adds
    `home/Library/Caches/Roost-iced/roost.sock` — the jailed UI then
    refuses to bind. `/tmp` is the only root with room, and it is short
    on Linux too."""
    root = Path(tempfile.mkdtemp(prefix="roost-jail-", dir="/tmp"))
    try:
        yield root
    finally:
        shutil.rmtree(root, ignore_errors=True)


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
        # Roost has just edited five of the user's config files. Both
        # ways back out have to be in the sentence that says so.
        assert "roostctl agent uninstall --all" in toast, toast
        assert "agent-hooks = off" in toast, toast
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
