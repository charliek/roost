"""Terminal-behavior E2E — the program-driven cwd pipeline.

Scope note: OSC *parsing* is unit-tested in `roost-osc` (osc7/osc2/…).
Several program-driven terminal behaviors are NOT cleanly E2E-testable
through this IPC harness and live elsewhere:
  - OSC 2 window-title: roost derives the tab title from cwd, and the
    shell re-emits its own title each prompt, so a transient OSC 2 is
    overwritten — verify visually via `tools/screenshot` screenshots.
  - Live resize/reflow: the UI sizes the terminal grid to the window, so
    `tab.resize` doesn't pin a deterministic size — `tools/screenshot` (resize
    the window, check reflow) is the right tool.
This file covers the one program→core→UI pipeline that IS deterministic
where the shell cooperates: OSC 7 cwd tracking via a real `cd`.
"""

from __future__ import annotations

import pytest

from client import Roost
from util import cwd_reaches, precondition


def _cd_and_emit_osc7(roost, project):
    """Open a tab, `cd /usr`, and emit OSC 7 explicitly (the same
    `ESC ] 7 ; file://… ST` the shell integration sends). Hermetic — no
    dependence on the shell's own PROMPT_COMMAND integration loading.
    Returns the tab id once the tracked cwd is /usr."""
    tab = roost.open_tab(project, cwd="/tmp")
    roost.run(tab, r"cd /usr && printf '\033]7;file:///usr\033\\' && echo AT=done")
    roost.wait_text(tab, "AT=done", timeout=8)
    precondition(cwd_reaches(roost, tab, "/usr"),
                 "OSC 7 cwd not tracked (terminal cwd reception unavailable)")
    return tab


def test_cwd_tracking_follows_cd(roost, project):
    """The OSC 7 → tracked-cwd pipeline. A failure is a real regression in
    cwd reception, so it's a hard failure in fresh mode (the shell's
    *automatic* emit-on-cd is covered in test_shell_integration)."""
    tab = _cd_and_emit_osc7(roost, project)
    assert roost.tab(tab)["cwd"] == "/usr"


def test_title_follows_cwd(roost, project, target):
    """The tab title should re-derive from the cwd when it changes via
    OSC 7. Match the basename (`usr`): the Mac UI titles a tab with the
    cwd's leaf (`/tmp` → `tmp`) while GTK shows the path — `usr` is in both
    and absent from `tmp`. Poll, since the title updates a beat after cwd.

    XXX: skipped on Mac pending investigation (issue #196). The title
    following cwd is **shell-driven** (the integration emits OSC 0 each
    prompt) — neither Workspace re-derives the title in its cwd setter. The
    GTK default shell has integration (emits OSC 0 → title gets `usr`); Mac
    CI's default shell is Apple bash 3.2 with NO integration → no OSC 0 → the
    title stays at the open-time leaf (`tmp`). So it's not a Mac UI bug. The
    open question (issue #196): should the title follow cwd via the *model*
    (re-derive in set_tab_cwd when !userTitled, both UIs) so it works on any
    shell? If so, this skip can be dropped and the test run cross-platform.
    """
    if target == "mac":
        pytest.skip(
            "title-follows-cwd is shell-OSC-0-driven; Mac CI's default shell "
            "(Apple bash 3.2) has no integration, so no OSC 0. Not a Mac UI "
            "bug — see issue #196. cwd tracking is covered cross-platform by "
            "test_cwd_tracking_follows_cd."
        )
    tab = _cd_and_emit_osc7(roost, project)
    Roost._wait(
        lambda: "usr" in (roost.tab(tab) or {}).get("title", ""),
        timeout=5,
        what="tab title reflects cwd /usr",
    )
