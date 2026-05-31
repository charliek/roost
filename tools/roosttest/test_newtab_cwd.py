"""New-tab cwd inheritance E2E (Cmd-T / Ctrl-T and the Cmd/Alt+Shift+T
launcher).

A new tab should spawn in the *active tab's live (OSC 7) cwd*, not the
project's stored cwd. Both surfaces dispatch through the same core path
the keybind drives:
  - `palette.activate("new_tab")` is exactly what Cmd-T / Ctrl-T fire.
  - the launcher frame is exactly what Cmd/Alt+Shift+T opens.

Determinism: rather than depend on the shell's OSC 7 integration
(PROMPT_COMMAND), the helper `cd`s and then emits the OSC 7 sequence
itself — the same `ESC ] 7 ; file://… ST` the shell integration sends —
so the live cwd is set on any shell. The project's stored cwd is /tmp
(the `project` fixture), so a live cwd of /usr proves inheritance reads
the *live* cwd, not the project cwd.
"""

from __future__ import annotations

from client import Timeout
from util import cwd_reaches, precondition

# Live cwd distinct from the project fixture's /tmp, and real on both
# Linux and macOS (note: /tmp is a symlink on macOS, /usr is not — we
# only ever assert on /usr, so the symlink quirk can't bite us).
LIVE_CWD = "/usr"


def _active_tab_in_live_cwd(roost, project):
    """Open a tab in `project`, make the project active, move its live cwd
    to LIVE_CWD, and return the tab id. `cd` makes the shell's real cwd
    match; the explicit OSC 7 emit sets roost's tracked cwd even on shells
    without prompt integration. Skips only if OSC 7 reception itself is
    broken (defensive — shouldn't happen)."""
    tab = roost.open_tab(project, cwd="/tmp")
    roost.focus(tab)  # make `project` active so a new tab lands here
    # `cd` + emit OSC 7 for LIVE_CWD (empty authority → just the path,
    # the form docs/guides/cwd-tracking.md documents as the smoke test).
    roost.run(tab, rf"cd {LIVE_CWD} && printf '\033]7;file://{LIVE_CWD}\033\\' && echo AT=done")
    roost.wait_text(tab, "AT=done", timeout=8)
    # The explicit OSC 7 emit above means tracking should always work; a
    # failure here is a regression in fresh mode (hard fail), a capability
    # gap on an ad-hoc dev UI otherwise (skip).
    precondition(
        cwd_reaches(roost, tab, LIVE_CWD),
        "OSC 7 cwd not tracked (terminal cwd reception unavailable)",
    )
    return tab


def _new_tab_id(roost, before, what):
    roost._wait(
        lambda: {int(t["id"]) for t in roost.tabs()} - before,
        5.0,
        what,
    )
    return next(iter({int(t["id"]) for t in roost.tabs()} - before))


def _wait_pwd_output(roost, tab_id, needle, timeout=12.0):
    """Wait for a spawned tab's `pwd` marker, dumping the tab on timeout so
    a flake is diagnosable instead of an opaque wait failure. The base
    timeout is generous (scaled by ROOST_TEST_TIMEOUT_SCALE) because the
    spawned shell must start + run its command under CI load before output
    appears."""
    try:
        roost.wait_text(tab_id, needle, timeout=timeout)
    except Timeout:
        dump = roost._safe_dump_text(tab_id)
        raise AssertionError(
            f"tab {tab_id} never showed {needle!r} (shell slow to spawn/run?). "
            f"Viewport:\n{dump}"
        )


def test_new_tab_inherits_active_cwd(roost, project, palette):
    """Cmd-T / Ctrl-T (palette `new_tab`) opens the new tab in the active
    tab's live cwd (/usr), not the project cwd (/tmp)."""
    _active_tab_in_live_cwd(roost, project)

    before = {int(t["id"]) for t in roost.tabs()}
    state = palette.palette_open(kind="commands")
    assert "new_tab" in roost.palette_item_ids(state), roost.palette_item_ids(state)
    state = palette.palette_activate("new_tab")
    assert state["open"] is False  # activating new_tab confirms + closes

    new_id = _new_tab_id(roost, before, "new_tab spawned a tab")
    # Ask the new shell where it is — proves the *spawn* cwd directly,
    # independent of the new tab's own OSC 7 timing.
    roost.run(new_id, "echo NEWTAB_PWD=$(pwd)")
    _wait_pwd_output(roost, new_id, f"NEWTAB_PWD={LIVE_CWD}")


def test_launcher_runs_in_active_cwd(roost, project, palette):
    """The command launcher (Cmd/Alt+Shift+T) spawns its tab in the active
    tab's live cwd too. Uses the seeded `Print Pwd` command; skips when the
    seed config isn't active (a developer's already-running UI)."""
    _active_tab_in_live_cwd(roost, project)

    state = palette.palette_open(kind="launcher")
    items = {it["title"]: it["id"] for it in state["items"]}
    precondition("Print Pwd" in items, "seed config not active (UI not launched by the harness)")

    before = {int(t["id"]) for t in roost.tabs()}
    state = palette.palette_activate(items["Print Pwd"])
    assert state["open"] is False
    new_id = _new_tab_id(roost, before, "launcher spawned a tab")
    _wait_pwd_output(roost, new_id, f"LAUNCH_PWD={LIVE_CWD}")
