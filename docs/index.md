# Roost

Roost is a desktop terminal multiplexer for AI coding agents. It runs on macOS and Linux, presents a sidebar of projects with tabs inside each project, and notifies you when an agent in a tab needs your attention.

The terminal engine is [libghostty-vt](https://ghostty.org), the parser/screen-state library extracted from Ghostty. The UI is GTK4 + libadwaita via [gotk4](https://github.com/diamondburned/gotk4). Persistence is SQLite. Everything outside the cgo boundary is pure Go.

## What it does

- One window with a project sidebar and per-project tabs
- One libghostty-vt terminal per tab, persistent across restarts
- Companion CLI (`roost-cli`) for sending notifications from inside a tab
- OSC 9 / OSC 777 fallback so non-CLI tools still trigger notifications
- Native desktop notifications when a non-focused tab pings you

## What it is not

- Not a Ghostty replacement
- Not a tmux replacement
- Not multi-window — one window, projects in sidebar, tabs in projects
- Not split-pane — one terminal per tab
- Not a browser host
- Not a Windows app — macOS and Linux only

## Quick example

```bash
# From inside any Roost tab — fires a notification on this tab.
roost-cli notify --title "Build done" --body "tests pass"
```

If the tab is not currently focused you will see:

- A `needs attention` indicator on the tab in Roost
- A native desktop notification (macOS Notification Center or freedesktop notifications on Linux)

## Next steps

- [Installation](getting-started/installation.md) — system packages, building from source
- [First Run](getting-started/first-run.md) — what happens on launch and where state lives
- [Keybindings](getting-started/keybindings.md) — tab and project switching, platform-native modifiers
- [Notifications](guides/notifications.md) — how the notification pipeline works
- [Claude Code Hooks](guides/claude-code.md) — wire `roost-cli notify` into Claude
