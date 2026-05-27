"""Shell-integration E2E (runs against both --roost-target mac and gtk).

P1 — login shell: Roost spawns the default shell as a LOGIN shell (via
`-l`) so profile files (.bash_profile/.zprofile) load. On macOS that
silences the bash deprecation banner and puts login-only PATH entries
(e.g. `claude`) in scope; it matches Terminal.app / Ghostty.

Assertion technique: the asserted substring must appear ONLY in command
*output*, never in the echoed command line. We print the result through
`printf "MARKER:%s\\n" "$VAR"` — the echo shows the literal `%s`/`$VAR`,
so `MARKER:<value>` materializes only when the command actually runs
(same trick `test_newtab_cwd.py` uses with `$(pwd)`).
"""

from __future__ import annotations

import sys

import pytest

# Detect login state per shell: bash via `shopt -q login_shell`, zsh via
# `[[ -o login ]]`. Anything else POSIX (dash, etc.) reports `skip` —
# Roost ships integration for bash + zsh. Both arms parse cleanly in
# either shell (the dead branch is parsed but not executed). Assumes a
# POSIX-family default shell; fish (non-POSIX) isn't probed here — it
# emits OSC 7 natively and isn't in the shipped-integration set.
_LOGIN_PROBE = (
    'L=no; '
    'if [ -n "$BASH_VERSION" ]; then shopt -q login_shell && L=yes; '
    'elif [ -n "$ZSH_VERSION" ]; then [[ -o login ]] && L=yes; '
    'else L=skip; fi; '
    'printf "ROOST_LOGIN_RESULT:%s\\n" "$L"'
)


def test_default_shell_is_login(roost, project):
    """A plain new tab's shell is a login shell."""
    tab = roost.open_tab(project, cwd="/tmp")
    roost.run(tab, _LOGIN_PROBE)
    roost._wait(
        lambda: any(
            f"ROOST_LOGIN_RESULT:{v}" in roost.dump_text(tab)
            for v in ("yes", "no", "skip")
        ),
        timeout=8,
        what="login probe result",
    )
    text = roost.dump_text(tab)
    if "ROOST_LOGIN_RESULT:skip" in text:
        pytest.skip("default shell is neither bash nor zsh")
    assert "ROOST_LOGIN_RESULT:yes" in text, f"expected a login shell, got:\n{text}"
    assert "ROOST_LOGIN_RESULT:no" not in text


def test_explicit_argv_not_login(roost, project):
    """An explicit argv (launcher-style) is NOT forced into login mode.

    Assert the spawned shell is actually the explicit bash AND non-login,
    so this can't false-pass if the explicit argv were ignored and a
    default zsh/dash opened instead (where `shopt` would *also* report
    not-login). `${BASH_VERSION:+yes}` proves it's bash; `shopt -q
    login_shell` proves it's non-login. Neither marker value appears in
    the echoed command (the echo shows the literal `%s`/`$(...)`).
    """
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(
        tab,
        'printf "EXARGV:bash=%s login=%s\\n" "${BASH_VERSION:+yes}" '
        '"$(shopt -q login_shell && echo yes || echo no)"',
    )
    roost.wait_text(tab, "EXARGV:bash=yes login=no", timeout=8)


def test_native_cwd_inherits_cd(roost, project, palette, target):
    """A new tab inherits the active tab's *current* dir via the native
    cwd read, even when the shell emits no OSC 7 (bare bash). This is the
    P3 fallback that fixes Cmd-T for shells without Roost integration.

    Uses /usr (not a symlink on macOS or Linux) so the native read's
    physical path matches the logical path. Skipped on the macOS GTK dev
    build, which has no /proc; e2e-mac (proc_pidinfo) and Linux e2e-gtk
    (/proc) cover the real read in CI.
    """
    if target == "gtk" and sys.platform == "darwin":
        pytest.skip("GTK native cwd read is Linux-only (/proc); macOS GTK is dev-only")

    # Bare shell: no rc, no profile, no integration -> no OSC 7 emitted.
    active = roost.open_tab(project, cwd="/tmp",
                            argv=["/bin/bash", "--norc", "--noprofile"])
    roost.focus(active)  # make the project active so new_tab lands here
    roost.run(active, 'cd /usr && echo "ATDIR:$(pwd)"')
    roost.wait_text(active, "ATDIR:/usr", timeout=8)  # cd done (output-only marker)

    before = {int(t["id"]) for t in roost.tabs()}
    state = palette.palette_open(kind="commands")
    assert "new_tab" in roost.palette_item_ids(state), roost.palette_item_ids(state)
    palette.palette_activate("new_tab")
    roost._wait(lambda: {int(t["id"]) for t in roost.tabs()} - before,
                5.0, "new_tab spawned a tab")
    new_id = next(iter({int(t["id"]) for t in roost.tabs()} - before))

    # The new tab spawned in the active shell's cwd (/usr), proven by its
    # own pwd — independent of the new tab's OSC 7 timing.
    roost.run(new_id, "echo NEWTAB_PWD=$(pwd)")
    roost.wait_text(new_id, "NEWTAB_PWD=/usr", timeout=8)


def test_launcher_inherits_native_cwd(roost, project, palette, target):
    """The command launcher inherits the active tab's native cwd too —
    parity with Cmd-T — for shells without OSC 7. Uses the seeded
    `Print Pwd` command; skips when the seed config isn't active."""
    if target == "gtk" and sys.platform == "darwin":
        pytest.skip("GTK native cwd read is Linux-only (/proc); macOS GTK is dev-only")

    probe = palette.palette_open(kind="launcher")
    have_seed = "Print Pwd" in {it["title"] for it in probe["items"]}
    palette.palette_dismiss()
    if not have_seed:
        pytest.skip("seed config not active (UI not launched by the harness)")

    active = roost.open_tab(project, cwd="/tmp",
                            argv=["/bin/bash", "--norc", "--noprofile"])
    roost.focus(active)
    roost.run(active, 'cd /usr && echo "ATDIR:$(pwd)"')
    roost.wait_text(active, "ATDIR:/usr", timeout=8)

    before = {int(t["id"]) for t in roost.tabs()}
    state = palette.palette_open(kind="launcher")
    items = {it["title"]: it["id"] for it in state["items"]}
    palette.palette_activate(items["Print Pwd"])
    roost._wait(lambda: {int(t["id"]) for t in roost.tabs()} - before,
                5.0, "launcher spawned a tab")
    new_id = next(iter({int(t["id"]) for t in roost.tabs()} - before))
    roost.wait_text(new_id, "LAUNCH_PWD=/usr", timeout=8)
