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

from util import cwd_reaches, precondition


def test_cwd_tracking_follows_cd(roost, project):
    """The OSC 7 → cwd → derived-title pipeline. We `cd` for real and then
    emit the OSC 7 sequence ourselves (the same `ESC ] 7 ; file://… ST` the
    shell integration sends), so this is hermetic — it doesn't depend on
    the shell's own PROMPT_COMMAND integration loading. A failure is a real
    regression in the cwd pipeline, so it's a hard failure in fresh mode
    (the shell's *automatic* emit-on-cd is covered in test_shell_integration).
    """
    tab = roost.open_tab(project, cwd="/tmp")
    roost.run(tab, r"cd /usr && printf '\033]7;file:///usr\033\\' && echo AT=done")
    roost.wait_text(tab, "AT=done", timeout=8)

    precondition(cwd_reaches(roost, tab, "/usr"),
                 "OSC 7 cwd not tracked (terminal cwd reception unavailable)")
    assert roost.tab(tab)["cwd"] == "/usr"
    assert "/usr" in roost.tab(tab)["title"]  # title derives from cwd
