# Roost shell integration (zsh).
#
# Sourced inside a Roost tab to make the header subtitle, the tab label,
# and new-tab cwd inheritance follow `cd` via OSC 7, and to set a sane
# default prompt. Safe everywhere: gated on $ROOST_TAB_ID (no-op outside
# Roost), idempotent (safe to source twice), and interactive-only.
#
# Roost also resolves a new tab's cwd natively, so cwd inheritance works
# without this file; sourcing it adds the live subtitle/title + a default
# prompt, and reports cwd over SSH (where the native read can't reach).
#
# Feature flags via $ROOST_SHELL_FEATURES (comma list; `no-<feature>`
# disables): cwd, title, prompt.
#
# KEEP IN SYNC with crates/roost-linux/src/resources/shell-integration/roost.zsh

[[ -o interactive ]] || return 0
[[ -n "${ROOST_TAB_ID:-}" ]] || return 0
[[ -n "${_ROOST_ZSH_LOADED:-}" ]] && return 0
typeset -g _ROOST_ZSH_LOADED=1

_roost_feature() {
  case ",${ROOST_SHELL_FEATURES:-cwd,title,prompt}," in
    (*",no-$1,"*) return 1 ;;
    (*) return 0 ;;
  esac
}

__roost_osc7() {
  _roost_feature cwd || return 0
  printf '\033]7;file://%s%s\033\\' "${HOST}" "$PWD"
}

__roost_title() {
  _roost_feature title || return 0
  printf '\033]0;%s\033\\' "${PWD/#$HOME/~}"
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd __roost_osc7
add-zsh-hook precmd __roost_title

# Default prompt (cwd in blue + a plain $) only when the user hasn't set
# one — zsh's stock default PROMPT is '%m%# ', else empty.
if _roost_feature prompt && [[ -z "${PS1:-}" || "$PS1" == '%m%# ' ]]; then
  PROMPT='%F{blue}%~%f $ '
  export ROOST_PS1_APPLIED=1
fi
