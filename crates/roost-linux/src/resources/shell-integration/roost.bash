# Roost shell integration (bash).
#
# Makes the header subtitle, the tab label, and new-tab cwd inheritance
# follow `cd` via OSC 7, and sets a sane default prompt. Safe everywhere:
# gated on $ROOST_TAB_ID (no-op outside Roost), idempotent (safe to load
# twice), and interactive-only.
#
# Loaded two ways, both handled here:
#   * Auto-bootstrap (modern bash, >= 4.4): Roost starts bash with
#     `--posix` and points ENV at this file, the only per-interactive-shell
#     hook bash offers. POSIX mode skips bash's normal startup files, so the
#     inject block below recreates that sequence (sourcing the user's
#     profile/rc), drops POSIX mode, then falls through to the integration.
#   * Manual `source` (the documented opt-in, and Apple's bash 3.2 which
#     can't do ENV+POSIX): ROOST_BASH_INJECT is unset, so the inject block
#     is a no-op and only the integration body runs.
#
# Roost also resolves a new tab's cwd natively, so cwd inheritance works
# without this file; loading it adds the live subtitle/title + a default
# prompt, and reports cwd over SSH (where the native read can't reach).
#
# Feature flags via $ROOST_SHELL_FEATURES (comma list; `no-<feature>`
# disables): cwd, title, marks, prompt, ssh-env.
#
# `ssh-env` adds `-o "SendEnv COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION"`
# to every `ssh` invocation so modern TUIs (opencode, neovim with
# truecolor themes) render correctly on remote hosts. Equivalent to
# Ghostty's `shell-integration-features.ssh-env`. Requires the remote
# sshd to `AcceptEnv` those variables (Debian/Ubuntu defaults only
# accept LANG LC_*; users may need to extend `AcceptEnv` server-side
# for the env to take effect).
#
# The inject block is adapted from Ghostty's ghostty.bash (GPLv3 header
# below); the integration body is Roost's.
#
# KEEP IN SYNC with mac/Sources/Roost/Resources/shell-integration/roost.bash
#
# Parts of the inject block are based on Ghostty's bash integration, which
# is based on Kitty's. Kitty is distributed under GPLv3, so that block is
# also distributed under GPLv3:
#
#   This program is free software: you can redistribute it and/or modify
#   it under the terms of the GNU General Public License as published by
#   the Free Software Foundation, either version 3 of the License, or
#   (at your option) any later version.
#
#   This program is distributed in the hope that it will be useful,
#   but WITHOUT ANY WARRANTY; without even the implied warranty of
#   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
#   GNU General Public License for more details.
#
#   You should have received a copy of the GNU General Public License
#   along with this program.  If not, see <http://www.gnu.org/licenses/>.

case $- in
  *i*) ;;
  *) return 0 2>/dev/null || exit 0 ;;
esac

# Auto-bootstrap: recreate bash's startup sequence when Roost injected us
# via --posix + ENV (see the header). No-op when manually sourced
# (ROOST_BASH_INJECT unset). `builtin`-prefixed so a pre-existing alias
# can't hijack these steps before the user's config has even loaded.
if [ -n "${ROOST_BASH_INJECT:-}" ]; then
  # Stash our flags, then unset the injection vars up front so a re-sourced
  # rc (or a nested shell) can't recurse back through this block.
  builtin declare __roost_bash_flags="$ROOST_BASH_INJECT"
  builtin unset ENV ROOST_BASH_INJECT

  # Restore an ENV we displaced so the user's own ENV still applies.
  if [ -n "${ROOST_BASH_ENV:-}" ]; then
    builtin export ENV="$ROOST_BASH_ENV"
    builtin unset ROOST_BASH_ENV
  fi

  # Leave POSIX mode (and reset inherit_errexit, which the posix reset
  # doesn't cover) so the shell behaves like a normal interactive bash.
  builtin set +o posix
  builtin shopt -u inherit_errexit 2>/dev/null

  # Un-export HISTFILE if we set it; POSIX mode would otherwise have
  # defaulted history to ~/.sh_history instead of ~/.bash_history.
  if [ -n "${ROOST_BASH_UNEXPORT_HISTFILE:-}" ]; then
    builtin export -n HISTFILE
    builtin unset ROOST_BASH_UNEXPORT_HISTFILE
  fi

  # Manually source the startup files. See INVOCATION in bash(1) and
  # run_startup_files() in shell.c in the bash source: a login shell reads
  # the profile chain; a non-login interactive shell reads the system +
  # user bashrc.
  if builtin shopt -q login_shell; then
    if [[ $__roost_bash_flags != *"--noprofile"* ]]; then
      [ -r /etc/profile ] && builtin source "/etc/profile"
      for __roost_rcfile in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
        [ -r "$__roost_rcfile" ] && {
          builtin source "$__roost_rcfile"
          break
        }
      done
    fi
  else
    if [[ $__roost_bash_flags != *"--norc"* ]]; then
      # The system bashrc path is set at bash build time via -DSYS_BASHRC
      # and varies by distro: Arch/Debian/Ubuntu use /etc/bash.bashrc,
      # Void uses /etc/bash/bashrc, Fedora/NixOS use /etc/bashrc.
      for __roost_rcfile in /etc/bash.bashrc /etc/bash/bashrc /etc/bashrc; do
        [ -r "$__roost_rcfile" ] && {
          builtin source "$__roost_rcfile"
          break
        }
      done
      if [ -z "${ROOST_BASH_RCFILE:-}" ]; then ROOST_BASH_RCFILE="$HOME/.bashrc"; fi
      [ -r "$ROOST_BASH_RCFILE" ] && builtin source "$ROOST_BASH_RCFILE"
    fi
  fi

  builtin unset __roost_rcfile
  builtin unset __roost_bash_flags
  builtin unset ROOST_BASH_RCFILE
fi

[ -n "${ROOST_TAB_ID:-}" ] || return 0
[ -n "${_ROOST_BASH_LOADED:-}" ] && return 0
_ROOST_BASH_LOADED=1

_roost_feature() {
  case ",${ROOST_SHELL_FEATURES:-cwd,title,marks,prompt,ssh-env}," in
    *",no-$1,"*) return 1 ;;
    *) return 0 ;;
  esac
}

# `ssh-env` feature: forward terminal-capability env vars across the
# SSH boundary. The macOS default `ssh_config` only sends LANG + LC_*,
# so COLORTERM is silently dropped — opencode and other modern TUIs
# then fall back to 256-color and look broken. `builtin command ssh`
# bypasses any user-defined `ssh` alias/function, mirroring Ghostty's
# `ssh-env` (ghostty.bash::ssh). Whether the remote accepts these
# vars depends on its `AcceptEnv` setting; SendEnv with a rejecting
# server is a silent no-op (no worse than current behavior).
if _roost_feature ssh-env; then
  ssh() {
    builtin command ssh \
      -o "SendEnv COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION" \
      "$@"
  }
fi

__roost_osc7() {
  _roost_feature cwd || return 0
  printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-}" "$PWD"
}

__roost_title() {
  _roost_feature title || return 0
  printf '\033]0;%s\033\\' "${PWD/#$HOME/~}"
}

# OSC 133 command marks: C on command start (PS0), D when it ends (the
# next prompt's PROMPT_COMMAND). Roost maps C -> running, D -> cleared.
__roost_marks() {
  _roost_feature marks || return 0
  printf '\033]133;D\033\\'
}
# C via PS0 needs bash >= 4.4; older bash (e.g. macOS /bin/bash 3.2)
# silently ignores PS0, so only the D (command-end) mark fires there.
if _roost_feature marks && { [ "${BASH_VERSINFO[0]:-0}" -gt 4 ] ||
  { [ "${BASH_VERSINFO[0]:-0}" -eq 4 ] && [ "${BASH_VERSINFO[1]:-0}" -ge 4 ]; }; }; then
  PS0='\e]133;C\e\\'"${PS0:-}"
fi

# Prepend so the user's existing PROMPT_COMMAND still runs.
PROMPT_COMMAND="__roost_marks;__roost_osc7;__roost_title;${PROMPT_COMMAND:-}"

# Default prompt (cwd in blue + a plain $) only when the user hasn't set
# one — bash's stock interactive default is '\s-\v\$ ', else empty.
if _roost_feature prompt && { [ -z "${PS1:-}" ] || [ "$PS1" = '\s-\v\$ ' ]; }; then
  PS1='\[\033[34m\]\w\[\033[0m\] \$ '
  export ROOST_PS1_APPLIED=1
fi
