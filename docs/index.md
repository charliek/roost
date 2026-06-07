# Roost

Roost is a desktop terminal multiplexer for AI coding agents. It runs on macOS and Linux, presents a sidebar of projects with tabs inside each project, and notifies you when an agent in a tab needs your attention.

The terminal engine is [libghostty-vt](https://ghostty.org), the parser/screen-state library extracted from Ghostty. Roost ships two native UIs — Swift + AppKit on macOS (`Roost.app`) and Rust + gtk4-rs on Linux (`roost`). There is no daemon: each UI embeds the workspace, the PTY supervisor, and a JSON IPC server in-process. External tooling (`roostctl`, Claude Code hooks) reaches the running UI over a Unix domain socket speaking newline-delimited JSON; see [IPC](reference/ipc.md) for the wire format. Persistence is a small `state.json` written atomically.

The original Go + GTK4 prototype has been retired; its archived snapshot (code, docs, and migration history) lives in the separate `roost-legacy-go` repository.

## Install

Linux (Ubuntu Noble / Pop!\_OS 24.04+) via apt — add the `apt.stridelabs.ai` repo once, then:

```bash
sudo apt install roost
```

macOS — download `Roost-<version>.dmg` from the [latest GitHub release](https://github.com/charliek/roost/releases) and drag `Roost.app` into `/Applications`.

Building from source instead? See [Installation](getting-started/installation.md).

## What it does

- One window with a project sidebar and per-project tabs.
- One libghostty-vt terminal per tab, persistent across restarts.
- Companion CLI (`roostctl`) for firing notifications from inside a tab.
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
roostctl notify --title "Build done" --body "tests pass"
```

If the tab is not currently focused you'll see:

- A `needs attention` indicator on the tab in Roost.
- A native desktop notification (macOS Notification Center or freedesktop notifications on Linux).

## Next steps

- [Installation](getting-started/installation.md) — apt + DMG, plus the from-source toolchain and how to verify the install.
- [First Run](getting-started/first-run.md) — what happens on launch and where state lives.
- [Keybindings](getting-started/keybindings.md) — tab and project switching, platform-native modifiers.
- [Notifications](guides/notifications.md) — how the notification pipeline works.
- [Claude Code Hooks](guides/claude-code.md) — wire `roostctl notify` into Claude.
- [Architecture](reference/architecture.md) — how the UIs, IPC socket, and CLI fit together.
- [Vision](development/vision.md) — the durable design decisions and migration path.
