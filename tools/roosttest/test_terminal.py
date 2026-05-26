"""Terminal-behavior E2E — the program-driven cwd pipeline.

Scope note: OSC *parsing* is unit-tested in `roost-osc` (osc7/osc2/…).
Several program-driven terminal behaviors are NOT cleanly E2E-testable
through this IPC harness and live elsewhere:
  - OSC 2 window-title: roost derives the tab title from cwd, and the
    shell re-emits its own title each prompt, so a transient OSC 2 is
    overwritten — verify visually via `tools/uitest` screenshots.
  - Live resize/reflow: the UI sizes the terminal grid to the window, so
    `tab.resize` doesn't pin a deterministic size — `tools/uitest` (resize
    the window, check reflow) is the right tool.
This file covers the one program→core→UI pipeline that IS deterministic
where the shell cooperates: OSC 7 cwd tracking via a real `cd`.
"""

from __future__ import annotations

import time

import pytest


def _cwd_becomes(roost, tab, want, timeout=2.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if (roost.tab(tab) or {}).get("cwd") == want:
            return True
        time.sleep(0.1)
    return False


def test_cwd_tracking_follows_cd(roost, project):
    """`cd` updates the tab's tracked cwd (the shell emits OSC 7; roost
    parses it → cwd → the derived title). Skips on shells without OSC 7
    integration (e.g. a bare bash with no PROMPT_COMMAND), rather than
    pinning a shell-dependent assertion."""
    tab = roost.open_tab(project, cwd="/tmp")
    roost.run(tab, "printf 'P=%s\\n' 1")
    roost.wait_text(tab, "P=1", timeout=8)

    roost.run(tab, "cd /usr && printf 'AT=%s\\n' done")
    roost.wait_text(tab, "AT=done", timeout=8)

    if not _cwd_becomes(roost, tab, "/usr"):
        pytest.skip("shell does not emit OSC 7 (no cwd integration)")
    assert roost.tab(tab)["cwd"] == "/usr"
    assert "/usr" in roost.tab(tab)["title"]  # title derives from cwd
