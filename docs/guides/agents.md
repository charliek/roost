# Agent Hooks

Roost drives the tab dot, the sidebar rollup, and the desktop banner for
five coding agents — Claude Code, Codex, OpenCode, grok (and its fork
gx), and cursor-agent — by wiring one hook entry per agent into that
agent's own configuration. Wiring happens automatically the first time
the Roost UI starts (`agent-hooks = auto` in `config.conf`); nothing here
is a per-machine manual step.

This page is a stub — the full guide (per-agent wiring details,
`roostctl agent` reference, and the `agent-hooks` / `agent-hooks-skip`
config keys) lands with plan 046's docs commit. What follows is enough
for `roostctl doctor`'s `Agents` section to link somewhere real.

## Install

`roostctl agent ensure` (what the UIs run at startup), `roostctl agent
install <agent>|--all`, and `roostctl agent uninstall <agent>|--all`
wire and unwire Roost's hook entries. Every installed command points at
the `$ROOST_AGENT_HOOK` environment variable rather than an absolute
path, so the same entry works after a relocated install or on a
different machine. `roostctl agent status` reports what is wired, at
which integration version.

## Ownership

A tab is "owned" by whichever agent's hook last reported activity on
it. Doctor's `owning` checks read this off the running UI's tab list —
there is no durable "ever observed" store, so they can only say who
owns a tab *right now*.

## Codex trust

Codex additionally records a `trusted_hash` per hook handler in
`config.toml`, which is what stops it from asking to review a hook
change it can already account for. Doctor's `agent.codex.trust` check
compares the hash codex would compute for the handler Roost has
installed against what is actually on disk, so a mismatch — a hand
edit, a moved `CODEX_HOME`, a codex-side formula change — is diagnosed
instead of reappearing as an unexplained "Hooks need review" dialog on
the next launch.

## Legacy Claude settings

Before plan 046, `roostctl claude install` wrote a Roost-owned file at
`~/.config/roost/claude-settings.json` and asked you to alias `claude`
to pass `--settings` at it. That file is retired: `claude install` is
now a bare alias of `roostctl agent install claude`, which merges hook
entries directly into Claude's own `~/.claude/settings.json` instead.
If both the old file and the alias are still active, every Claude hook
event is delivered twice — harmless to Roost's state, but wasteful, and
the old file's absolute path is wrong on any other machine. Remove it:

1. Delete `~/.config/roost/claude-settings.json` (or run `roostctl
   agent uninstall claude`, which removes it when it still matches the
   shape `claude install` used to write).
2. Remove the `alias claude=…` line it asked you to add from your shell
   rc (`.bashrc`, `.zshrc`, `.bash_profile`, or fish's `config.fish` /
   `alias --save` output).
