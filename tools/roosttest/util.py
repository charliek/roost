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
import time

import pytest

from client import RoostError, scaled_timeout


def is_fresh() -> bool:
    """Whether the harness owns a fresh, hermetic UI this session
    (`--roost-fresh` / `ROOST_TEST_FRESH=1`). In fresh mode the harness
    guarantees the seed config + working OSC 7 cwd tracking, so a failed
    setup precondition is a real regression — see `precondition`. The
    `fresh` conftest fixture exports `ROOST_TEST_FRESH=1` when the flag is
    used, so this works whether fresh came from the flag or the env."""
    return os.environ.get("ROOST_TEST_FRESH") == "1"


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


def wait_tab_attached(roost, tab_id: int, timeout: float = 5.0) -> None:
    """Wait until the UI's TerminalView for `tab_id` is live.

    `tab.open` returns as soon as the workspace creates the tab; the
    UI's TerminalView attaches asynchronously on the main loop. Poll
    `tab.dump` (same shape, same attachment dependency) until it
    stops returning `not-found`. Raises `TimeoutError` on overrun.
    """
    deadline = time.monotonic() + scaled_timeout(timeout)
    while True:
        try:
            roost.dump_text(tab_id)
            return
        except RoostError as e:
            if e.code != "not-found":
                raise
        if time.monotonic() >= deadline:
            raise TimeoutError(f"tab {tab_id} never attached a TerminalView")
        time.sleep(0.05)


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
