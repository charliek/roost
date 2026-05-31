"""Shell-integration E2E (runs against both --roost-target mac and gtk).

P1 — shell type follows Ghostty's platform split: Roost spawns the
default shell as a LOGIN shell (via `-l`) on **macOS** (so
.bash_profile/.zprofile load — silencing the bash deprecation banner
and putting login-only PATH entries like `claude` in scope, matching
Terminal.app), but as a **non-login interactive** shell on **Linux** (so
~/.bashrc loads — where prompts/aliases live; a Linux login shell would
read the profile chain and let a stray .bash_profile shadow .bashrc).
The expected state keys off the UI host OS (`sys.platform`), since the
Rust default-shell logic gates `-l` on `cfg!(target_os = "macos")`: the
Mac app and the macOS GTK dev build are login, Linux GTK is non-login.

Assertion technique: the asserted substring must appear ONLY in command
*output*, never in the echoed command line. We print the result through
`printf "MARKER:%s\\n" "$VAR"` — the echo shows the literal `%s`/`$VAR`,
so `MARKER:<value>` materializes only when the command actually runs
(same trick `test_newtab_cwd.py` uses with `$(pwd)`).
"""

from __future__ import annotations

import sys

import pytest

from client import Timeout
from util import cwd_reaches, precondition, wait_shell_ready, wait_tab_attached

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


def test_default_shell_login_matches_platform(roost, project):
    """A plain new tab's shell is a login shell on macOS, non-login on
    Linux — Ghostty's platform split (see module docstring). The host OS
    decides because the Rust logic gates `-l` on
    `cfg!(target_os = "macos")`; the Mac app only ever runs on darwin,
    and the macOS GTK dev build is login too, so `sys.platform` is the
    correct determinant for either target."""
    expect_login = sys.platform == "darwin"
    want = "yes" if expect_login else "no"
    other = "no" if expect_login else "yes"
    tab = roost.open_tab(project, cwd="/tmp")
    # The default shell on either platform is integrated (bash on
    # Linux, zsh on macOS) — it emits pre-prompt content
    # (compinit warnings on the GH ubuntu runner zsh / `--posix`
    # recreation in bash / etc.) that races `roost.run`'s
    # viewport-non-empty check, eating the first keystroke. Wait
    # for attach + the printf sentinel before sending the probe.
    wait_tab_attached(roost, tab)
    wait_shell_ready(roost, tab)
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
    assert f"ROOST_LOGIN_RESULT:{want}" in text, (
        f"expected a {'login' if expect_login else 'non-login'} shell on "
        f"{sys.platform}, got:\n{text}"
    )
    assert f"ROOST_LOGIN_RESULT:{other}" not in text


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
    precondition(have_seed, "seed config not active (UI not launched by the harness)")

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


def test_env_injected(roost, project):
    """Roost injects its shell-integration env contract into every tab,
    and forces TERM=xterm-256color (advertising the terminal it provides,
    not the one that launched it).

    Hermetic: a bare `--norc --noprofile` bash so the developer's own
    `~/.bashrc` (which may `export ROOST_SHELL_FEATURES=no-title` per the
    documented git-aware-title recipe) can't override the injected default
    we assert. Roost injects the env regardless of which shell runs."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    # Wait for the TerminalView to attach before driving the shell: under
    # full-suite load `run()` can otherwise fire before the PTY is live and
    # the keystrokes are lost (no echo, no output → a spurious timeout).
    wait_tab_attached(roost, tab)
    # Emit each field on its OWN short line, not one long line: the UI sizes
    # the grid to the window, so a single ~83-char marker wraps at narrow
    # widths and a contiguous-substring match flakes on window size. Short
    # per-field lines never wrap. Each value appears only in the OUTPUT (the
    # echoed command shows the literal `%s`/`$VAR`), so a match is genuine.
    roost.run(
        tab,
        'printf "ENV_tp=%s\\nENV_si=%s\\nENV_feat=%s\\nENV_term=%s\\n'
        'ENV_rd=%s\\nENV_done\\n" '
        '"$TERM_PROGRAM" "$ROOST_SHELL_INTEGRATION" "$ROOST_SHELL_FEATURES" '
        '"$TERM" "${ROOST_RESOURCES_DIR:+set}"',
    )
    expected = [
        "ENV_tp=Roost",
        "ENV_si=1",
        "ENV_feat=cwd,title,marks,prompt,ssh-env",
        "ENV_term=xterm-256color",
        "ENV_rd=set",
    ]
    try:
        roost.wait_text(tab, "ENV_done", timeout=12)  # all fields emitted
    except Timeout:
        raise AssertionError(
            f"shell-integration env probe produced no output; tab {tab} "
            f"viewport:\n{roost._safe_dump_text(tab)}"
        )
    text = roost._safe_dump_text(tab)
    missing = [m for m in expected if m not in text]
    assert not missing, (
        f"shell-integration env contract missing {missing}; tab {tab} "
        f"viewport:\n{text}"
    )


def test_resources_dir_has_scripts(roost, project):
    """The shipped integration scripts are present at ROOST_RESOURCES_DIR
    (Mac: in the .app bundle; GTK: written to the XDG cache at spawn)."""
    tab = roost.open_tab(project, cwd="/tmp")
    roost.run(
        tab,
        'if test -r "$ROOST_RESOURCES_DIR/shell-integration/roost.bash" '
        '&& test -r "$ROOST_RESOURCES_DIR/shell-integration/roost.zsh"; '
        'then r=ok; else r=missing; fi; printf "SCRIPTS:%s\\n" "$r"',
    )
    roost.wait_text(tab, "SCRIPTS:ok", timeout=8)


def test_sourced_script_tracks_cwd(roost, project):
    """Sourcing the shipped bash integration makes cwd follow `cd` via
    OSC 7 — validates the shipped artifact actually works (not just that
    it ships). Bare bash so only the sourced script enables OSC 7."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(
        tab,
        'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash" '
        '&& cd /usr && echo "SRC:$(pwd)"',
    )
    roost.wait_text(tab, "SRC:/usr", timeout=8)
    # The next prompt fires PROMPT_COMMAND -> OSC 7 -> tracked cwd.
    assert cwd_reaches(roost, tab, "/usr"), \
        f"sourced script did not track cwd; got {(roost.tab(tab) or {}).get('cwd')!r}"


def test_documented_rooster_override(roost, project):
    """The documented git-aware override (`__roost_fancy_title`) works end
    to end: in a non-repo dir it emits OSC 0 with 🐓 + the ~-path, the tab
    title reflects it, and the emoji round-trips through the OSC scanner.

    Also pins the `$HOME → ~` abbreviation across bash versions: the recipe
    uses a `case` + prefix-strip, NOT `${PWD/#$HOME/~}`, because that isn't
    portable — macOS bash 3.2 treats the replacement `~` as literal while
    bash >= 5.2 tilde-expands it (a no-op showing the full home path), and
    escaping (`\\~`) just flips which version breaks. The cd-to-$HOME step
    below runs on both runners (Apple bash 3.2 on Mac, bash 5.x on Linux),
    so it catches a regression to either non-portable form."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    # The exact function from docs/guides/cwd-tracking.md, as a one-liner.
    fancy = (
        r'''__roost_fancy_title() { [ -n "$ROOST_TAB_ID" ] || return; '''
        r'''local icon="🐓" branch title; '''
        r'''case "$PWD" in "$HOME") title="~";; "$HOME"/*) title="~${PWD#"$HOME"}";; '''
        r'''*) title="$PWD";; esac; '''
        r'''if branch=$(git symbolic-ref --short HEAD 2>/dev/null); then '''
        r'''[ -n "$(git status --porcelain 2>/dev/null)" ] && icon="🐣"; '''
        r'''title+=" (${branch})"; fi; '''
        r"""printf '\033]0;%s\033\\' "${icon} ${title}"; }; """
        r'''PROMPT_COMMAND="__roost_fancy_title;${PROMPT_COMMAND}"; cd /usr'''
    )
    roost.run(tab, fancy)
    # /usr is not a git repo -> icon stays 🐓, no branch suffix.
    roost._wait(
        lambda: "🐓" in (roost.tab(tab) or {}).get("title", "")
        and "/usr" in (roost.tab(tab) or {}).get("title", ""),
        timeout=8,
        what="fancy title tracks cwd in a non-repo",
    )
    # Regression guard for the escaped tilde: in $HOME the title must
    # abbreviate to "🐓 ~", not the full home path. (With an unescaped `~`
    # the substitution no-ops and this would show "🐓 /home/<user>".)
    roost.run(tab, 'cd "$HOME"')
    roost._wait(
        lambda: "🐓 ~" in (roost.tab(tab) or {}).get("title", ""),
        timeout=8,
        what="fancy title abbreviates $HOME to ~",
    )


def test_osc133_drives_state(roost, project):
    """OSC 133 C (command start) -> running; D (command end) -> cleared.
    Emitted directly so it needs no shell integration; bare bash so only
    our explicit marks drive state."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(tab, r"""printf '\033]133;C\033\\'; echo C133""")
    roost.wait_text(tab, "C133", timeout=8)
    roost.wait_state(tab, "running", timeout=5)
    roost.run(tab, r"""printf '\033]133;D\033\\'; echo D133""")
    roost.wait_text(tab, "D133", timeout=8)
    roost.wait_state(tab, "none", timeout=5)


def test_osc133_suppressed_when_hook_active(roost, project):
    """While a Claude hook owns the tab (hookActive), shell OSC 133 is
    suppressed — the hook's state wins; releasing it re-enables 133."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.set_hook_active(tab, True)
    roost.set_state(tab, "idle")  # as the Claude hook would
    roost.wait_state(tab, "idle", timeout=5)
    # Shell emits C; with the hook active the dot must NOT flip to running.
    roost.run(tab, r"""printf '\033]133;C\033\\'; echo HC1""")
    roost.wait_text(tab, "HC1", timeout=8)
    roost.run(tab, "echo HC2")  # second round-trip drains any queued OSC dispatch
    roost.wait_text(tab, "HC2", timeout=8)
    assert (roost.tab(tab) or {}).get("state") == "idle", (roost.tab(tab) or {}).get("state")
    # Release the hook: shell OSC 133 drives state again.
    roost.set_hook_active(tab, False)
    roost.run(tab, r"""printf '\033]133;C\033\\'; echo HC3""")
    roost.wait_text(tab, "HC3", timeout=8)
    roost.wait_state(tab, "running", timeout=5)


def test_bash_marks_emit_wired(roost, project):
    """The shipped bash integration wires the OSC 133 C mark into PS0 on
    bash >= 4.4. Older bash (e.g. macOS /bin/bash 3.2) ignores PS0, so the
    C mark is intentionally skipped there (only D fires) — assert that's
    what we get rather than silently shipping a dead PS0."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(
        tab,
        'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash"; '
        'if [ "${BASH_VERSINFO[0]}" -gt 4 ] || '
        '{ [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -ge 4 ]; }; then '
        'case "$PS0" in *133*) r=wired;; *) r=missing;; esac; else r=oldbash; fi; '
        'printf "PS0MARK:%s\\n" "$r"',
    )
    roost._wait(
        lambda: any(f"PS0MARK:{v}" in roost.dump_text(tab)
                    for v in ("wired", "oldbash", "missing")),
        timeout=8, what="PS0 mark probe",
    )
    text = roost.dump_text(tab)
    if "PS0MARK:oldbash" in text:
        pytest.skip("bash < 4.4 (no PS0); C mark intentionally skipped")
    assert "PS0MARK:wired" in text, text
    assert "PS0MARK:missing" not in text


def test_zsh_auto_bootstrap_tracks_cwd(roost, project):
    """A zsh tab auto-loads the integration via the ZDOTDIR shim — NO
    manual `source` — and cwd follows `cd` (OSC 7). Skips if zsh isn't
    installed. Uses /usr (not a symlink) so the path compares cleanly."""
    import os
    import shutil

    zsh = "/bin/zsh" if os.path.exists("/bin/zsh") else (shutil.which("zsh") or "")
    # In fresh mode (CI), the GTK + Mac runners are provisioned with zsh — a
    # missing binary is a CI-provisioning regression, NOT a benign capability
    # gap. Outside fresh mode (dev hosts), skip cleanly when zsh isn't
    # installed.
    precondition(bool(zsh), "zsh not available")
    tab = roost.open_tab(project, cwd="/tmp", argv=[zsh, "-l"])
    wait_tab_attached(roost, tab)
    # GH runner zsh prints `compinit: insecure directories…` before the first
    # prompt, which makes the harness's default "viewport non-empty" readiness
    # signal a false positive — the first keystroke is dropped into a
    # still-initializing zle. wait_shell_ready loops on a `printf '%s\n' VAL`
    # sentinel so the shell is provably interactable before we send `cd`.
    wait_shell_ready(roost, tab)
    # The ZDOTDIR shim loads roost.zsh on the FIRST precmd (deferred so a
    # user's .zshrc can't drop us); roost.zsh's OSC 7 hook then fires from
    # the NEXT prompt on. So give it several prompt cycles before polling —
    # the load + hook-registration + emit costs a couple of prompts, and a
    # slow CI runner widens that window. (Verified working on a clean
    # ubuntu:24.04 in Docker — the deferred-load timing is the only variable.)
    roost.run(tab, "cd /usr")
    for _ in range(3):
        roost.run(tab, "true")
    if not cwd_reaches(roost, tab, "/usr", timeout=10):
        raise AssertionError(
            "zsh auto-bootstrap cwd not tracked; got "
            f"{(roost.tab(tab) or {}).get('cwd')!r}. Viewport:\n"
            f"{roost._safe_dump_text(tab)}"
        )


def _modern_bash() -> str:
    """Path to a bash >= 4.4, or "" if only Apple's /bin/bash 3.2 exists.
    Probes candidates on the harness host (same machine the UI spawns on)."""
    import os
    import shutil
    import subprocess

    def ok(path: str) -> bool:
        if not path or not os.path.exists(path):
            return False
        try:
            out = subprocess.run(
                [path, "-c", "echo ${BASH_VERSINFO[0]} ${BASH_VERSINFO[1]}"],
                capture_output=True, text=True, timeout=5,
            ).stdout.split()
            major, minor = int(out[0]), int(out[1])
            return major > 4 or (major == 4 and minor >= 4)
        except Exception:
            return False

    return next(
        (c for c in ("/opt/homebrew/bin/bash", "/usr/local/bin/bash",
                     shutil.which("bash") or "", "/usr/bin/bash") if ok(c)),
        "",
    )


def test_bash_auto_bootstrap_tracks_cwd(roost, project):
    """A bash tab auto-loads the integration via `--posix` + ENV — NO manual
    `source` — so cwd follows `cd` (OSC 7), and the inject header has left
    POSIX mode (proving it recreated bash's startup rather than leaving the
    shell in the raw `--posix` we spawned it with).

    Needs a modern bash (>= 4.4) opened explicitly, mirroring the zsh test;
    Apple's /bin/bash 3.2 can't do the ENV+POSIX path and is skipped (it
    keeps the documented manual source). That the user's real ~/.bashrc
    still loads on top is covered by live validation — CI dotfiles aren't
    predictable, but POSIX mode being off here proves the recreation ran."""
    bash = _modern_bash()
    # In fresh mode (CI), the Mac runner is provisioned with brew bash; Linux
    # ships modern bash by default. A missing modern bash is a CI-provisioning
    # regression on either, NOT a benign capability gap. Outside fresh mode
    # (dev hosts), skip cleanly when only Apple's 3.2 is around.
    precondition(
        bool(bash),
        "no modern bash (>= 4.4); Apple /bin/bash 3.2 is manual-source only",
    )
    tab = roost.open_tab(project, cwd="/tmp", argv=[bash, "-l"])
    # Bash's --posix + ENV inject + recreate-startup chain emits content
    # before the first prompt is interactable; same race the zsh test above
    # documents. wait_shell_ready proves the shell can run a command before
    # the first `roost.run` lands.
    wait_tab_attached(roost, tab)
    wait_shell_ready(roost, tab)
    # No manual source: --posix + ENV auto-load roost.bash; its OSC 7 hook
    # fires on the prompt. A couple of round-trips lets it settle.
    roost.run(tab, "cd /usr")
    roost.run(tab, "true")
    assert cwd_reaches(roost, tab, "/usr", timeout=8), \
        f"bash auto-bootstrap cwd not tracked; got {(roost.tab(tab) or {}).get('cwd')!r}"
    # The recreation block ran `set +o posix`; if the shell were still in
    # the raw --posix state we spawned it with, this would read `on`.
    roost.run(
        tab,
        'case ":$SHELLOPTS:" in *:posix:*) p=on;; *) p=off;; esac; '
        'printf "BOOTPOSIX:%s\\n" "$p"',
    )
    roost.wait_text(tab, "BOOTPOSIX:off", timeout=8)


def test_title_follows_cd_via_script(roost, project):
    """The shipped integration's default title feature sets the tab title
    to the cwd (tilde-abbreviated) via OSC 0 on cd."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(tab,
              'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash" && cd /usr')
    roost._wait(lambda: (roost.tab(tab) or {}).get("title") == "/usr",
                timeout=8, what="tab title follows cd to /usr")


def test_prompt_set_when_stock(roost, project):
    r"""roost.bash sets its default prompt when PS1 is the shell's stock
    default (bare bash's interactive PS1 is the stock '\s-\v\$ ')."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(tab,
              'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash"; '
              'printf "PS1APP:%s\\n" "${ROOST_PS1_APPLIED:-no}"')
    roost.wait_text(tab, "PS1APP:1", timeout=8)


def test_prompt_not_clobbered_when_custom(roost, project):
    """roost.bash leaves a user-set prompt alone (only-if-stock): no
    ROOST_PS1_APPLIED, and the user's PS1 survives."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(tab,
              'PS1="MYPROMPT> "; '
              'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash"; '
              'printf "PS1CHK:applied=%s kept=%s\\n" "${ROOST_PS1_APPLIED:-no}" '
              '"$([ "$PS1" = "MYPROMPT> " ] && echo yes || echo no)"')
    roost.wait_text(tab, "PS1CHK:applied=no kept=yes", timeout=8)


# --- ssh-env feature ------------------------------------------------------
#
# The `ssh-env` feature defines an `ssh()` wrapper that adds
# `-o "SendEnv COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION"` to every
# `ssh` invocation. Goal: opencode + other modern TUIs render correctly
# on remote hosts that today's macOS ssh_config silently strips
# COLORTERM from. Equivalent to Ghostty's shell-integration-features.ssh-env.
#
# These tests cover the function-definition path (default on; opt-out
# via no-ssh-env). The actual SendEnv argv injection is a literal,
# verified by `declare -f ssh` inspection without invoking the real ssh
# binary (the bash check below) and `which ssh` for zsh.


def test_ssh_env_wrapper_defined_by_default_bash(roost, project):
    """`ssh-env` is in the default feature list, so sourcing roost.bash
    in a fresh interactive shell makes `ssh` a function (not a binary
    path). Bare bash so only the sourced script enables the wrapper."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(tab,
              'export ROOST_TAB_ID=1; '
              'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash"; '
              'printf "SSHFN:%s\\n" "$(type -t ssh)"')
    roost.wait_text(tab, "SSHFN:function", timeout=8)


def test_ssh_env_wrapper_omitted_when_opted_out_bash(roost, project):
    """`ROOST_SHELL_FEATURES=...,no-ssh-env` opts out: `ssh` resolves
    back to the underlying binary (`type -t ssh` = `file`), not a
    function. Confirms the standard `no-<feature>` opt-out works for
    the new flag."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(tab,
              'export ROOST_TAB_ID=1; '
              'export ROOST_SHELL_FEATURES=cwd,title,marks,prompt,no-ssh-env; '
              'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash"; '
              'printf "SSHFN:%s\\n" "$(type -t ssh)"')
    # Expected: not a function. `type -t ssh` prints "file" when ssh
    # resolves to a binary on PATH (the normal case on macOS + Linux
    # CI), or empty when ssh isn't installed at all (still proves the
    # wrapper didn't load).
    roost.wait_text(tab, "SSHFN:", timeout=8)
    # The exact follow-up text must not be "function".
    import re
    dump = roost.dump_text(tab)
    line = next((ln for ln in dump.splitlines() if ln.startswith("SSHFN:")), "")
    assert "function" not in line, \
        f"ssh wrapper still defined after no-ssh-env opt-out: {line!r}"


def test_ssh_env_wrapper_sendenv_args_bash(roost, project):
    """The wrapper's body forwards COLORTERM + TERM_PROGRAM +
    TERM_PROGRAM_VERSION via SendEnv. Verified by `declare -f ssh`
    inspection — no remote network round-trip needed since the args
    are a static literal.

    `declare -f ssh` produces ~100 chars; in a default 80-col viewport
    it wraps across rows. We can't bracket-search reliably (a unique
    sentinel typed into the command would also appear in the command
    echo) so we join the whole dump with no separator — wrap-recovery
    by reassembly. The SendEnv literal is unique enough to the function
    body that a false hit elsewhere is implausible."""
    tab = roost.open_tab(project, cwd="/tmp",
                         argv=["/bin/bash", "--norc", "--noprofile"])
    roost.run(tab,
              'export ROOST_TAB_ID=1; '
              'source "$ROOST_RESOURCES_DIR/shell-integration/roost.bash"; '
              'declare -f ssh; echo SSHDONE')
    roost.wait_text(tab, "SSHDONE", timeout=8)
    joined = "".join(roost.dump_text(tab).splitlines())
    assert "SendEnv COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION" in joined, \
        f"ssh wrapper missing SendEnv literal in dump: {joined!r}"
