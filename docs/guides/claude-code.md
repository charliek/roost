# Claude Code Hooks

Wire Claude Code's hook system to Roost so each tab gets a sticky agent-state indicator (running / needs-input / idle / failed), a click-through desktop banner when Claude is blocked, done, or errors out, and noise-free output (raw-OSC suppression while Claude owns the tab).

## How it works

Roost ships a `roostctl claude-hook EVENT` subcommand that Claude Code invokes for each lifecycle event. The hook reads Claude's JSON payload from stdin, looks up `$ROOST_TAB_ID` (auto-set in every Roost tab), and translates the event into a `tab.agent_report` — a claim/preserve/release of ownership plus an optional lifecycle change and notification, all under the `claude` source (see [`ipc.md`](../reference/ipc.md#tabagent_report) for the wire shape). The hook is a silent no-op when run outside a Roost tab (no `$ROOST_TAB_ID`) or for an event this adapter doesn't map, so installing it doesn't break Claude when you launch it from a regular terminal.

| Hook event | Roost's mapping | Effect |
|---|---|---|
| `SessionStart` | Claims ownership as `claude`; lifecycle → inactive | No visible dot change yet, but raw OSC 9/99/777 from inside the shell is now suppressed while this session owns the tab |
| `UserPromptSubmit` | Lifecycle → running; clears pending notification | Blue dot |
| `Notification` — `permission_prompt`, `agent_needs_input`, `elicitation_dialog` | Lifecycle → needs-input; fires a warn-severity banner | Orange dot + banner |
| `Notification` — `idle_prompt` (the post-turn idle nag — deliberately non-blocking so a finished session doesn't re-render as blocked), `auth_success`, `elicitation_complete`, `elicitation_response`, `agent_completed`, or any **other/unrecognized** `notification_type` | Lifecycle **unchanged**; fires an info-severity banner | The banner still reaches you, but the dot doesn't move — see below for why |
| `Stop`, no in-flight `background_tasks` | Lifecycle → idle ("finished"); fires an info banner ("Turn complete") | Gray dot |
| `Stop`, **non-empty** `background_tasks` | Lifecycle **stays running** ("working"), not idle; banner names the in-flight count | Distinguishes "the turn is done" from "paused, will resume when background work wakes it" — a scheduled `session_crons` entry alone does *not* count as in-flight |
| `StopFailure` | Lifecycle → failed; fires an error-severity banner naming Claude's reported `error` | Red dot — distinct from needs-input everywhere except on the legacy wire `state` field, which has no fifth value (see [`ipc.md`](../reference/ipc.md)) |
| `SessionEnd` | Releases ownership; lifecycle → inactive; clears pending notification | Dot disappears; tab falls back to shell-derived state |

`notification_type` is a free string Claude Code controls, not a closed enum Roost can validate against, so an **unrecognized value is a first-class case, not an error**: the hook fires the notification (you should still hear about it) but leaves the lifecycle dot alone, because a false `needs-input` is worse than a missed one — the dot is sticky, so guessing wrong would leave a tab reading "blocked" long after Claude moved on.

An event that carries Claude's own `agent_id` field (i.e. it fired from inside a subagent, not the top-level turn) never mutates ownership or lifecycle — only its notification, if any, still fires. In practice this is defense-in-depth rather than a live fix: subagent completion arrives as a distinct `SubagentStop` event that Roost doesn't register a handler for, so this path isn't reachable through any hook Roost currently wires up.

## Install

Inside a Roost tab:

```bash
roostctl claude install
```

This writes `~/.config/roost/claude-settings.json` with entries for all six lifecycle events above — `SessionStart`, `UserPromptSubmit`, `Notification`, `Stop`, `StopFailure`, `SessionEnd` — each pointing at the absolute path of `roostctl`, and prints a bash alias snippet to stdout. `StopFailure` is new: an already-installed settings file from before this hook existed keeps working (it simply never sees error-state turns until you rerun `install --force`). `roostctl claude-hook` itself accepts both this canonical spelling and the older kebab-case one (`session-start`, `prompt-submit`, `notification`, `stop`, `session-end`) that earlier versions of `claude install` wrote, so an existing settings file is never broken by an upgrade. Add the snippet to your shell rc:

```bash
roostctl claude install >> ~/.bashrc
source ~/.bashrc
```

The generated alias looks like:

```bash
alias claude='claude --settings /Users/you/.config/roost/claude-settings.json'
```

`claude --help` documents `--settings` as "load additional settings from" — meaning the file is *merged* into Claude's other settings sources (user, project, local). Your `~/.claude/settings.json` (model, permissions, MCP servers, etc.) keeps working untouched.

To overwrite an existing settings file, pass `--force`:

```bash
roostctl claude install --force
```

To uninstall, remove the alias from your shell rc and delete the file:

```bash
rm ~/.config/roost/claude-settings.json
```

Run `roostctl doctor` any time to check the install without guessing.
By default it prints one line for the whole `Claude Code` section;
`roostctl doctor -v` breaks it into the individual checks —
`claude.hook_events` confirms the settings file registers all six
lifecycle events (naming any it's missing), and `claude.hook_command`
confirms each one resolves to a runnable `roostctl` — `fail` if a
command's path is missing or not executable, `warn` if it resolves to a
different `roostctl` than the one you're running.

## Verifying

Open a fresh Roost tab, source your rc if needed, then:

```bash
roostctl identify
```

You should see a handful of `key=value` lines (`socket`, `pid`, `active_tab`, `ui_version`, …) describing the running app — not JSON. If it errors, the GUI isn't running or `ROOST_SOCKET` is unset — re-launch `roost` and try again.

Now run `claude` and submit a prompt. Watch the tab indicator:

- **Running (blue)** while Claude is working.
- **Needs-input (orange)** if Claude asks for permission. A desktop banner fires; click it to focus the tab.
- **Idle (gray)** when the turn ends. A "turn complete" banner fires (unless background work is still in flight — then it stays running).
- **Failed (red)** if the turn ends in an error (`StopFailure`) — Claude hitting a rate limit, an auth failure, an overloaded model, etc.
- **No indicator** between sessions.

If a project has multiple tabs running Claude, the project's sidebar row picks up a left-edge stripe in the most actionable color across its tabs, ranked `failed > needs-input > running > idle`.

## Other shells (fish, zsh)

The install command emits a bash alias by default. For other shells, adapt the syntax:

- **zsh**: same as bash — paste into `~/.zshrc`.
- **fish**: replace `alias claude='...'` with `alias claude '...'` (no `=`) in `~/.config/fish/config.fish`, or use `alias --save`.
- **POSIX `sh`**: same as bash.

## Why an alias and not editing the global settings file?

Roost deliberately doesn't edit your `~/.claude/settings.json`. The alias approach:

- Leaves the user's global config untouched (no merge logic, no marker comments, no risk of clobbering existing hooks).
- Is trivially reversible (unset the alias, delete one file).
- Lets the user run `claude` without Roost integration just by typing `command claude` or unsetting the alias.

## Troubleshooting

Run `roostctl doctor` first. It's read-only, covers most of the cases
below in one pass, and names exactly which check is unhappy instead of
you guessing from the symptom.

- **Hooks don't fire** — check `which claude`. If it points to the real binary instead of the alias, the alias didn't take effect (rc not sourced, or running in a non-interactive shell). Doctor's `claude.hook_events` and `claude.hook_command` checks confirm the settings file itself is correct; `claude.observed` says whether Roost has actually seen a hook fire on this tab.
- **Click-through doesn't focus** — on Linux, your notification daemon must support default actions (mako, dunst, GNOME Shell all do). On Wayland without an XDG-activation token, the window may only request attention rather than raise.
- **OSC 9 banners still appear from inside Claude** — that means the `SessionStart` hook didn't reach Roost (no ownership claimed, so raw OSC isn't suppressed). Doctor's `tab.raw_osc` check reports whether suppression is currently active on the tab; check `roostctl identify` and re-source your rc if it isn't.
- **The dot stopped moving after Claude errored out mid-session** — if Claude was killed or crashed without a `SessionEnd`, an OSC 133 prompt mark from the shell (reaching a fresh prompt) automatically releases the stale lifecycle and falls back to shell state; typing a command and pressing enter should clear it. See [Notifications → Hook-session OSC suppression](notifications.md#hook-session-osc-suppression) for the mechanism.

See [Notifications](notifications.md) for the full pipeline architecture.
