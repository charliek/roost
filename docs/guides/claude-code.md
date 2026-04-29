# Claude Code Hooks

Wire Claude Code's hook system to Roost so each tab gets a sticky agent-state indicator (running / needs-input / idle), a click-through desktop banner when Claude is blocked or done, and noise-free output (OSC suppression).

## How it works

Roost ships a `roost-cli claude-hook EVENT` subcommand that Claude Code invokes for each lifecycle event. The hook reads Claude's JSON payload from stdin, looks up `$ROOST_TAB_ID` (auto-set in every Roost tab), and tells the GUI:

| Hook event         | What Roost does                                                   |
|--------------------|-------------------------------------------------------------------|
| `SessionStart`     | Engage OSC suppression for this tab (no visible state change)     |
| `UserPromptSubmit` | Clear pending-attention badge, set tab state = running            |
| `Notification`     | Set tab state = needs-input, fire a desktop banner                |
| `Stop`             | Set tab state = idle, fire a desktop banner ("turn complete")     |
| `SessionEnd`       | Release OSC suppression, clear state, clear any pending badge     |

The hook is a silent no-op when run outside a Roost tab (no `$ROOST_TAB_ID`), so installing it doesn't break Claude when you launch it from a regular terminal.

## Install

Inside a Roost tab:

```bash
roost-cli claude install
```

This writes `~/.config/roost/claude-settings.json` with the five hook entries (each pointing at the absolute path of `roost-cli`) and prints a bash alias snippet to stdout. Add the snippet to your shell rc:

```bash
roost-cli claude install >> ~/.bashrc
source ~/.bashrc
```

The generated alias looks like:

```bash
alias claude='claude --settings /Users/you/.config/roost/claude-settings.json'
```

`claude --help` documents `--settings` as "load additional settings from" — meaning the file is *merged* into Claude's other settings sources (user, project, local). Your `~/.claude/settings.json` (model, permissions, MCP servers, etc.) keeps working untouched.

To overwrite an existing settings file, pass `--force`:

```bash
roost-cli claude install --force
```

To uninstall, remove the alias from your shell rc and delete the file:

```bash
rm ~/.config/roost/claude-settings.json
```

## Verifying

Open a fresh Roost tab, source your rc if needed, then:

```bash
roost-cli identify
```

You should see a JSON object describing the running app. If it errors, the GUI isn't running or `ROOST_SOCKET` is unset — re-launch `roost` and try again.

Now run `claude` and submit a prompt. Watch the tab indicator:

- **Running (blue)** while Claude is working.
- **Needs-input (orange)** if Claude asks for permission. A desktop banner fires; click it to focus the tab.
- **Idle (gray)** when the turn ends. A "turn complete" banner fires.
- **No indicator** between sessions.

If a project has multiple tabs running Claude, the project's sidebar row picks up a left-edge stripe in the most actionable color across its tabs (needs-input > running > idle > none).

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

- **Hooks don't fire** — check `which claude`. If it points to the real binary instead of the alias, the alias didn't take effect (rc not sourced, or running in a non-interactive shell).
- **No banners on macOS** — `terminal-notifier` is required. `brew install terminal-notifier`. The in-app tab indicator works without it.
- **Click-through doesn't focus** — on Linux, your notification daemon must support default actions (mako, dunst, GNOME Shell all do). On Wayland without an XDG-activation token, the window may only request attention rather than raise.
- **OSC 9 banners still appear from inside Claude** — that means the `SessionStart` hook didn't reach Roost. Check `roost-cli identify` and re-source your rc.

See [Notifications](notifications.md) for the full pipeline architecture.
