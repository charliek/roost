# Roost

Roost is a desktop terminal multiplexer for AI coding agents. It runs on macOS and Linux, presents a sidebar of projects with tabs inside each project, and notifies you when an agent in a tab needs your attention.

The terminal engine is [libghostty-vt](https://ghostty.org), the parser/screen-state library extracted from Ghostty. The core daemon (`roost-core`) is written in Rust; each platform has a native UI — Swift + AppKit on macOS (`Roost.app`), Rust + gtk4-rs on Linux. The wire contract between the daemon and UIs is defined in `proto/roost.proto` and runs over a Unix domain socket. Persistence is SQLite via `rusqlite`.

A legacy Go + GTK4 binary still ships from `main` while the Rust path comes up to full feature parity. See [Legacy (Go prototype)](reference/legacy-go/index.md) if you're running that binary.

## What it does

- One window with a project sidebar and per-project tabs.
- One libghostty-vt terminal per tab, persistent across restarts.
- Companion CLI (`roost-cli-rs`) for firing notifications from inside a tab.
- OSC 9 / OSC 777 fallback so non-CLI tools still trigger notifications.
- Native desktop notifications when a non-focused tab pings you.

## What it is not

- Not a Ghostty replacement.
- Not a tmux replacement.
- Not multi-window — one window, projects in the sidebar, tabs in each project.
- Not split-pane — one terminal per tab.
- Not a browser host.
- Not a Windows app — macOS and Linux only.

## Quick example

```bash
# From inside any Roost tab — fires a notification on this tab.
roost-cli-rs notify --title "Build done" --body "tests pass"
```

If the tab is not currently focused you'll see:

- A `needs attention` indicator on the tab in Roost.
- A native desktop notification (macOS Notification Center or freedesktop notifications on Linux).

## Next steps

- [Installation](getting-started/installation.md) — toolchain, building the daemon + UI, verifying the install.
- [First Run](getting-started/first-run.md) — what happens on launch and where state lives.
- [Keybindings](getting-started/keybindings.md) — tab and project switching, platform-native modifiers.
- [Notifications](guides/notifications.md) — how the notification pipeline works.
- [Claude Code Hooks](guides/claude-code.md) — wire `roost-cli-rs notify` into Claude.
- [Architecture](reference/architecture.md) — how the daemon, UIs, and CLI fit together.
- [Vision](development/vision.md) — the durable design decisions and migration path.
