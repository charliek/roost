"""Shared helpers for the roosttest pytest harness.

Helpers used by more than one test file land here so the
`scaled_timeout` discipline and the poll-drain shape stay in one
place. Test files import directly:

    from util import wait_tab_attached, drain, drain_until_match

History: `_wait_tab_attached` + a poll-drain-until-regex helper
existed in both `test_mouse_tracking.py` and `test_osc_pipeline.py`
with identical bodies (only the helper's name differed:
`_drain_until_match` vs `_drain_capture_until`). CodeRabbit flagged
the duplication on PR #183 (mac mouse-tracking) and PR #184 (gtk).
Consolidated into this module; the canonical names drop the
leading underscore because cross-file helpers aren't "private to
a file" any more.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import threading
import time
import uuid
from pathlib import Path

import pytest

from client import RoostError, Timeout, scaled_timeout

REPO_ROOT = Path(__file__).resolve().parents[2]

# A shell with NO startup files, therefore no Roost shell integration,
# therefore no OSC 133 marks the test didn't feed itself. Any tab whose
# agent lifecycle a test seeds MUST be opened with this argv: a real
# integrated shell can emit an A/B/D mark at any point after the claim
# (a WINCH-driven prompt redraw at view attach, say), and the plan-002
# dead-agent failsafe resets the seeded lifecycle to `inactive` when it
# lands. `test_agent_lifecycle.agent_tab` has always used this argv for
# exactly that reason; `test_agent_palette._seed` requires it.
BARE_SHELL_ARGV = ["/bin/bash", "--norc", "--noprofile"]


def roostctl_path() -> str:
    """Absolute path to a `roostctl` binary, building one if needed.

    Checked in the order a developer's tree makes them available: an
    explicit override, the cargo debug build (what CI's iced job builds),
    the binary embedded in the Mac bundle (what CI's Mac job builds),
    then PATH. The cargo fallback mirrors `ui.launch`, which builds the
    iced UI the same way when it's missing."""
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


# The wall-clock budget one hook invocation may take, before
# `ROOST_TEST_TIMEOUT_SCALE`.
#
# MIRRORS `roost_agent::hook::TOTAL_BUDGET` (2 s from the first socket
# call through the last report), plus 3 s for the one thing that budget
# deliberately excludes: this process's own spawn — a cold `execve` +
# dynamic link, and on macOS a Gatekeeper first-exec check.
#
# It is an assertion, not a convenience. Claude's and codex's
# `PermissionRequest` are decision hooks whose dialog is blocked on this
# process, so "it eventually answered" is not the contract — "it
# answered inside the budget" is. A generic 30 s ceiling would let a
# 20 s hook pass.
HOOK_DEADLINE = 5.0


def run_hook(
    verb: list[str],
    tab_id: int,
    socket: str | Path | None,
    stdin: bytes,
) -> float:
    """Run one of `roostctl`'s hook verbs (`agent-hook AGENT`,
    `claude-hook EVENT`) exactly as an installed hook would: the payload
    on stdin, the tab + socket in the environment, nothing else.
    `socket=None` runs with `ROOST_SOCKET` **unset**, which is how an
    `env -i` wrapper leaves it.

    Both verbs are fire-and-forget by contract — they always exit 0 with
    `{}` on stdout, inside [`HOOK_DEADLINE`], because Claude's and
    codex's `PermissionRequest` are decision hooks whose dialog blocks on
    this process and parses that stdout as JSON. That contract is
    asserted here, once, so the two lanes that drive hooks
    (`test_agent_lifecycle`, `test_agent_hooks`) cannot hold it to
    different standards. Everything else — including "no UI is listening"
    and "no adapter for that agent" — is silent, so the only proof a hook
    *did* something is the state assertions the caller makes after.

    Returns the elapsed seconds, so a caller driving a socket that never
    answers can pin the budget more tightly than the deadline below."""
    env = {**os.environ, "ROOST_TAB_ID": str(tab_id)}
    if socket is None:
        env.pop("ROOST_SOCKET", None)
    else:
        env["ROOST_SOCKET"] = str(socket)
    named = " ".join(verb)
    deadline = scaled_timeout(HOOK_DEADLINE)
    started = time.monotonic()
    try:
        proc = subprocess.run(
            [roostctl_path(), *verb],
            input=stdin,
            capture_output=True,
            env=env,
            timeout=deadline,
        )
    except subprocess.TimeoutExpired as expired:
        raise AssertionError(
            f"roostctl {named} did not answer within its {deadline:.1f}s budget "
            f"(HOOK_DEADLINE, scaled) — a decision hook's dialog waits this long"
        ) from expired
    elapsed = time.monotonic() - started
    assert proc.returncode == 0, (
        f"roostctl {named} exited {proc.returncode}: "
        f"{proc.stderr.decode(errors='replace')}"
    )
    assert proc.stdout.strip() == b"{}", proc.stdout
    return elapsed


def is_fresh() -> bool:
    """Whether the harness owns a fresh, hermetic UI this session
    (`--roost-fresh` / `ROOST_TEST_FRESH=1`). In fresh mode the harness
    guarantees the seed config + working OSC 7 cwd tracking, so a failed
    setup precondition is a real regression — see `precondition`. The
    `fresh` conftest fixture exports `ROOST_TEST_FRESH=1` when the flag is
    used, so this works whether fresh came from the flag or the env."""
    return os.environ.get("ROOST_TEST_FRESH") == "1"


def runs_alone(request) -> bool:
    """Whether this module is the whole pytest invocation.

    Self-enforcing check for app-ending tests (deleting the last project,
    firing the menu's Quit item, …): the session-scoped harness fixture
    launches exactly one UI for the whole invocation, so a test that ends
    that UI must be certain it's the only module collected — collected
    beside others (a whole-directory run, or a stale test-list edit),
    ending the UI here would fail every test that comes after. Skipping
    is loud — `pytest_terminal_summary` prints every skip with its
    reason. Originally duplicated verbatim between `test_exit_on_empty.py`
    and `test_menu_quit.py`; consolidated here per this module's own
    history note above."""
    return {item.path for item in request.session.items} == {request.node.path}


def skip_on_ci(reason: str, alt_coverage: str | None = None) -> None:
    """Skip a test on CI (`CI=true`) with a justification. Reserve this for
    tests that genuinely can't run remotely (e.g. a quit→relaunch lifecycle
    under bare xvfb), NOT for setup failures — those are `precondition`.
    Always cite where the regression class is otherwise covered via
    `alt_coverage`, so a remote skip never silently drops coverage."""
    if os.environ.get("CI") == "true":
        msg = reason if alt_coverage is None else f"{reason} [alt-coverage: {alt_coverage}]"
        pytest.skip(msg)


def precondition(ok: bool, reason: str) -> None:
    """Gate a test on a *setup* precondition. In fresh mode a failed
    precondition is a hard failure (the harness guarantees the
    environment, so this is a regression, not a capability gap);
    otherwise it's a skip (an ad-hoc dev UI may genuinely lack the
    capability — e.g. no seed config, a shell without OSC 7)."""
    if ok:
        return
    if is_fresh():
        pytest.fail(f"precondition failed in fresh (harness-owned) mode: {reason}")
    pytest.skip(reason)


def cwd_reaches(roost, tab_id: int, want: str, timeout: float = 3.0) -> bool:
    """True once the tab's tracked cwd equals `want`. Scaled poll —
    replaces the per-file `_cwd_becomes` raw loops that ignored
    `ROOST_TEST_TIMEOUT_SCALE` (so a hard assertion off this doesn't flake
    under CI's scale=3)."""
    deadline = time.monotonic() + scaled_timeout(timeout)
    while time.monotonic() < deadline:
        if (roost.tab(tab_id) or {}).get("cwd") == want:
            return True
        time.sleep(0.05)
    return False


# Attach is the lowest IPC-observable readiness rung above "id exists in
# tab.list"; `test_agent_lifecycle`'s single-shot `tab()["state"]` reads
# ride on it and are sound only while `state` and `agent_lifecycle`
# derive from ONE server-side write. If those ever split, those reads
# need condition waits of their own.
def wait_tab_attached(roost, tab_id: int, timeout: float = 5.0) -> None:
    """Wait until the UI's TerminalView for `tab_id` is live.

    `tab.open` returns as soon as the workspace creates the tab; the
    UI's TerminalView attaches asynchronously on the main loop. Poll
    `tab.dump` (same shape, same attachment dependency) until it
    stops returning `not-found`. Raises `TimeoutError` on overrun.

    The 5.0s default is pinned: attach is a main-loop round-trip, so a
    longer budget would mask the very regressions this anchor exists to
    catch. On overrun the message carries the last IPC error plus a
    `tab.list` snapshot (is the id even present?) — both gathered
    best-effort, so a failing diagnostic can't mask the timeout.
    """
    deadline = time.monotonic() + scaled_timeout(timeout)
    last_error: RoostError | None = None
    while True:
        try:
            roost.dump_text(tab_id)
            return
        except RoostError as e:
            if e.code != "not-found":
                raise
            last_error = e
        if time.monotonic() >= deadline:
            raise TimeoutError(
                f"tab {tab_id} never attached a TerminalView; "
                f"last IPC error={last_error}; {_tab_list_snapshot(roost, tab_id)}"
            )
        time.sleep(0.05)


def wait_tab_quiet(
    roost,
    tab_id: int,
    *,
    stable_polls: int = 3,
    interval: float = 0.1,
    timeout: float = 10.0,
) -> None:
    """Wait until a freshly opened tab's shell has painted AND gone quiet.

    The rung above `wait_tab_attached` that any test seeding viewport
    content with `tab.feed_pty_bytes` needs. Attach only means the
    TerminalView is live; the shell's own startup bytes (prompt, OSC 7 /
    OSC 0 / OSC 133 marks) are still in flight behind it. `feed_pty_bytes`
    applies its bytes the moment the UI services the op and does NOT
    serialize with PTY output already queued, so a seed sent at attach can
    land BEFORE the prompt — the prompt then appends to the row we just
    seeded (observed: triple-click on a seeded `hello world` selecting
    through col 33 because the prompt trailed it).

    Quiet is defined as: the viewport is non-empty (something painted) and
    `tab.dump`'s text is byte-identical across `stable_polls` consecutive
    polls. That is a condition wait, not a sleep — a shell that is still
    writing resets the counter and we keep polling to the deadline. Both
    the deadline and the poll interval go through `scaled_timeout`, so the
    quiet window widens with `ROOST_TEST_TIMEOUT_SCALE` on slow runners.

    Raises `client.Timeout` (with a viewport tail) if the tab never
    settles — a shell that never stops writing makes any seed racy, so
    that is a real failure, not something to paper over.
    """
    wait_tab_attached(roost, tab_id)
    eff_interval = scaled_timeout(interval)
    deadline = time.monotonic() + scaled_timeout(timeout)
    previous: str | None = None
    stable = 0
    while True:
        text = roost._safe_dump_text(tab_id)
        if text.strip() and text == previous:
            stable += 1
            if stable >= stable_polls:
                return
        else:
            stable = 0
        previous = text
        if time.monotonic() >= deadline:
            raise Timeout(
                f"tab {tab_id} never went quiet ({stable_polls} identical "
                f"tab.dump polls) within {timeout}s (scaled). Viewport tail:\n"
                f"{previous}"
            )
        time.sleep(eff_interval)


def _tab_list_snapshot(roost, tab_id: int, budget: float = 2.0) -> str:
    """Best-effort `tab.list` summary for timeout diagnostics. Runs the
    IPC call on a daemon thread with a hard join budget: the client's
    socket recv has no timeout, so a wedged UI — the very condition a
    timeout usually means — would otherwise hang this diagnostic forever
    and the real `TimeoutError` would never surface."""
    result: list[str] = []

    def grab() -> None:
        try:
            ids = sorted(int(t["id"]) for t in roost.tabs())
            result.append(f"tab.list ids={ids}, present={tab_id in ids}")
        except Exception as diag:
            result.append(f"tab.list unavailable ({diag!r})")

    t = threading.Thread(target=grab, daemon=True)
    t.start()
    t.join(scaled_timeout(budget))
    return result[0] if result else "tab.list unavailable (snapshot timed out)"


def spawned_tab_id(roost, before: set[int], what: str, timeout: float = 5.0) -> int:
    """Wait for a spawn to add a tab and return its id.

    `before` is the tab-id set captured before the spawn was triggered.
    """
    roost._wait(lambda: {int(t["id"]) for t in roost.tabs()} - before, timeout, what)
    return next(iter({int(t["id"]) for t in roost.tabs()} - before))


def wait_spawned_output(roost, tab_id: int, needle: str, timeout: float = 12.0) -> None:
    """Wait for a freshly spawned tab to print `needle`.

    The base timeout is deliberately generous (and scaled by
    `ROOST_TEST_TIMEOUT_SCALE`): the tab has to start a shell *and* run
    its command before anything reaches the viewport, and a cold first
    spawn under Xvfb on a loaded CI runner is the slowest case. An
    under-provisioned timeout here reads as a launcher bug when it is
    really just shell startup — that ambiguity is why the dump below
    exists.

    Anchored on `wait_tab_attached` first (its own scaled budget): the
    marker clock must not start before the TerminalView is live, or the
    pre-attach window is silently consumed as failed `tab.dump` polls
    and the marker budget measures attach latency instead of "shell runs
    the command and output round-trips."
    """
    wait_tab_attached(roost, tab_id)
    try:
        roost.wait_text(tab_id, needle, timeout=timeout)
    except Timeout as exc:
        dump = roost._safe_dump_text(tab_id)
        raise AssertionError(
            f"tab {tab_id} never showed {needle!r} (shell slow to spawn/run?). Viewport:\n{dump}"
        ) from exc


def wait_shell_ready(
    roost,
    tab_id: int,
    *,
    sentinel_attempts: int = 10,
    per_attempt_timeout: float = 2.0,
    total_timeout: float = 20.0,
) -> None:
    """Wait until the tab's shell can run a command and produce output.

    Robust against shells that emit startup output (compinit, MOTD,
    /etc/zshrc banners, `--posix` recreation, login chains) BEFORE
    the line editor is interactable: the harness's default
    'viewport non-empty' check (`roost.run`) races such output,
    dropping the first keystroke into a half-initialized zle.

    Each attempt sends `printf 'ROOST_READY_%s\\n' '<freshUuid>'`.
    The `%s` + positional-arg pattern is load-bearing: the shell
    echoes the typed command verbatim to the prompt line, so a
    literal sentinel inside single quotes would match `wait_text`
    via the echo before the shell ever runs the command. With `%s`
    + a separate VALUE arg, the echo shows the literal `%s` while
    only the printf OUTPUT contains the resolved value — present
    only when the command actually executes. Mirrors the in-tree
    convention documented in test_shell_integration.py:13-18.

    A fresh sentinel suffix is generated per attempt so a partial
    echo or a delayed first-attempt completion can't false-positive
    a later attempt.

    By the time this helper returns, the shell HAS executed printf
    and emitted output — that's what `wait_text` matched — so the
    race `roost.run`'s viewport-non-empty check defends against
    (writes-while-zle-uninitialized) is already past. The lingering
    sentinel echo is harmless to subsequent `roost.run` calls.

    Bounded by `sentinel_attempts` outer iterations; each per-attempt
    `wait_text` call is itself scaled by ROOST_TEST_TIMEOUT_SCALE
    inside `_wait`, so the outer total is a SOFT cap (the last
    iteration may overrun the outer deadline by up to one scaled
    `per_attempt_timeout`). On retry exhaustion, raises `client.Timeout`
    with a viewport dump. A transport failure (`roost.send` /
    `wait_text` raising a non-timeout `RoostError` like `not-found`
    when the tab dies) propagates the underlying `RoostError` rather
    than being rewrapped — the caller gets the real cause.

    `suffix` is `uuid4().hex` (`[0-9a-f]`) — shell-safe inside single
    quotes. Callers should not parameterize the value without
    re-checking that quoting.
    """
    deadline = time.monotonic() + scaled_timeout(total_timeout)
    last_sentinel = ""
    for _ in range(sentinel_attempts):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        # Fresh sentinel per attempt so a partial echo from a prior
        # iteration can't false-positive this one.
        suffix = uuid.uuid4().hex
        last_sentinel = f"ROOST_READY_{suffix}"
        # Output-only marker: echo shows the literal `%s`; only the
        # printf STDOUT contains the suffix.
        roost.send(tab_id, f"printf 'ROOST_READY_%s\\n' '{suffix}'\n")
        # 0.3s floor leaves room for `_wait`'s 100ms polling to see at
        # least 2-3 cycles; anything shorter would race the poll
        # interval. _wait re-applies scaled_timeout internally, so
        # per_attempt_timeout is passed un-scaled here.
        attempt_budget = min(per_attempt_timeout, max(0.3, remaining))
        try:
            roost.wait_text(tab_id, last_sentinel, timeout=attempt_budget)
            return
        except Timeout:
            continue
    try:
        tail = roost._safe_dump_text(tab_id)
    except Exception:
        tail = "<dump unavailable>"
    raise Timeout(
        f"shell never echoed printf output (last sentinel={last_sentinel!r}) "
        f"within {sentinel_attempts} attempts / {total_timeout}s (scaled). "
        f"Viewport tail:\n{tail}"
    )


def run_printf_probe(
    roost,
    tab_id: int,
    fields,
    *,
    attempts: int = 3,
    per_attempt_timeout: float = 4.0,
) -> str:
    """Run a `printf` probe of `fields` and return the viewport text once its
    OUTPUT (never the echo) has landed.

    `fields` is a list of `(label, shell_expr)` pairs, printed as `label=%s`
    with `shell_expr` as the matching positional arg — so the echoed command
    line shows the literal `%s`/`$expr` and `label=<value>` materializes ONLY
    when the command actually runs (the in-tree convention,
    test_shell_integration.py:13-18; same trick `wait_shell_ready` uses).

    Readiness is gated on a FRESH output-only sentinel (`roost_done=%s` + a
    uuid) appended to the probe, so `wait_text` can never false-match the echoed
    command line. That false-match is exactly what made `test_env_injected` the
    dominant e2e-mac flake: a literal sentinel in the format string matched the
    echo mid-render, so the dump was captured before the output existed and the
    re-send loop never fired. Building the sentinel here makes every probe
    output-only by construction.

    Each field is printed on its own short `label=%s` line (never one long
    line): the UI sizes the grid to the window, so a long line wraps at narrow
    widths and a contiguous-substring match flakes. Re-sends up to `attempts`
    times when a send's keystrokes are dropped under CI load. `shell_expr` must
    be safe inside double quotes (`$VAR`, `${VAR:+set}`); the uuid suffix is
    `[0-9a-f]`, shell-safe inside single quotes. Raises AssertionError with a
    viewport dump on exhaustion.
    """
    fmt = "".join(f"{label}=%s\\n" for label, _ in fields) + "roost_done=%s\\n"
    exprs = " ".join(f'"{expr}"' for _, expr in fields)
    last_text = ""
    for _ in range(attempts):
        suffix = uuid.uuid4().hex
        roost.send(tab_id, f"printf \"{fmt}\" {exprs} '{suffix}'\n")
        try:
            # Scaled inside `_wait` via ROOST_TEST_TIMEOUT_SCALE.
            roost.wait_text(tab_id, f"roost_done={suffix}", timeout=per_attempt_timeout)
            return roost._safe_dump_text(tab_id)
        except Timeout:
            last_text = roost._safe_dump_text(tab_id)
    raise AssertionError(
        f"printf probe produced no output after {attempts} sends; "
        f"tab {tab_id} viewport:\n{last_text}"
    )


def drain(roost, tab_id: int) -> bytes:
    """One-shot drain. Returns whatever bytes the UI has queued
    onto the input channel since the last drain — including empty
    when no event fired."""
    return roost.tab_capture_pty_input(tab_id, drain=True)


def drain_until_match(
    roost, tab_id: int, pattern: bytes, timeout: float = 5.0
) -> bytes:
    """Poll-drain until `pattern` (a regex over bytes) is seen, or
    the deadline expires. Returns the accumulated bytes for
    assertion-context use; raises `AssertionError` on timeout so
    the test fails loudly with the captured tail.

    `timeout` defaults to 5.0 (the more permissive value the OSC
    pipeline tests used) — color-query replies can arrive
    arbitrarily late through the drain. Call sites making
    fast-failing assertions on synthetic-event encoding (e.g.
    `test_mouse_tracking.py`) pass `timeout=2.0` explicitly.
    """
    deadline = time.monotonic() + scaled_timeout(timeout)
    captured = b""
    while time.monotonic() < deadline:
        captured += drain(roost, tab_id)
        if re.search(pattern, captured):
            return captured
        time.sleep(0.05)
    # One last drain+check after the deadline so a reply that lands
    # during the final 50 ms sleep window isn't lost. Otherwise the
    # check-then-drain-then-sleep loop ordering can flake out tests
    # whose data arrived in time but missed the last loop iteration.
    captured += drain(roost, tab_id)
    if re.search(pattern, captured):
        return captured
    raise AssertionError(
        f"never saw pattern {pattern!r} on tab {tab_id} (captured={captured!r})"
    )
