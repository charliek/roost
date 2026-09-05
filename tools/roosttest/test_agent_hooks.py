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

**Not here yet:** the `agent ensure` half — installing into a jailed
`$HOME`, and the config-off path — lands with plan 046's C7 under the
marked section at the bottom of this file. The harness already runs with
`agent-hooks = off` (`fixtures/launcher.conf`), so no lane touches a real
dotfile in the meantime.
"""

from __future__ import annotations

import json
import os
import socket as socketlib
import subprocess
import tempfile

import ui
from client import scaled_timeout
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
# Lands here, not elsewhere: the install engine (C6) and the config keys
# (C7) do not exist yet, and the harness runs with `agent-hooks = off`
# (`fixtures/launcher.conf`) so nothing above can touch a real dotfile in
# the meantime.
# ---------------------------------------------------------------------------
