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

import pytest

# Detect login state per shell: bash via `shopt -q login_shell`, zsh via
# `[[ -o login ]]`. Anything else (dash, etc.) reports `skip` — Roost
# ships integration for bash + zsh. Both arms parse cleanly in either
# shell (the dead branch is parsed but not executed).
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
    """An explicit argv (launcher-style) is NOT forced into login mode."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(
        tab,
        'shopt -q login_shell && printf "EXARGV:%s\\n" yes '
        '|| printf "EXARGV:%s\\n" no',
    )
    roost.wait_text(tab, "EXARGV:no", timeout=8)
