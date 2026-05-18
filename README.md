# Roost

A macOS + Linux desktop terminal multiplexer for AI coding agents. Sidebar of projects on the left, tabs per project, one libghostty-vt terminal per tab. Companion CLI surfaces notifications when an agent in a tab needs attention.

## Direction

Roost is undergoing an architectural refactor toward a Rust daemon (`roost-core`) with native UIs — Swift + AppKit on macOS, Rust + gtk4-rs on Linux — communicating over a gRPC contract on a Unix domain socket. The current Go + GTK4 binary on `main` continues to build and ship throughout. See [docs/development/vision.md](docs/development/vision.md) for the target architecture and phased rollout, and the long-lived `feature/rust-port` branch for in-progress work. Phases 0–7 closed as of 2026-05-17; Phase 7.5 (polish + automation gaps) and Phase 8 (bundling) remain on the branch before the eventual merge to `main` per the milestone schedule in [`plans/README.md`](plans/README.md).

## Quick start

```bash
git clone https://github.com/charliek/roost
cd roost
mise install                  # provisions Go 1.24 + Zig 0.15.2
make libghostty               # clones Ghostty at the pinned SHA, builds libghostty-vt
make build                    # produces ./roost and ./roost-cli
./roost
```

System packages:

- macOS: `brew install gtk4 libadwaita pkgconf gobject-introspection`
- Ubuntu / Debian: `sudo apt install libgtk-4-dev libadwaita-1-dev pkgconf gobject-introspection libgirepository1.0-dev`

## Documentation

The full documentation site lives under `docs/` and builds with `mkdocs-material`:

```bash
make docs-serve               # http://127.0.0.1:7070
```

Highlights:

- [Installation](docs/getting-started/installation.md) — system packages and first build
- [First Run](docs/getting-started/first-run.md) — what happens on launch and where state lives
- [Keybindings](docs/getting-started/keybindings.md) — tab and project switching, clipboard, mouse, scrollback, platform-native modifiers
- [Working Directory Tracking](docs/guides/cwd-tracking.md) — one-line shell snippet that makes the header subtitle and tab labels follow `cd`
- [Notifications](docs/guides/notifications.md) — how `roost-cli` and OSC fallbacks surface in the UI
- [Claude Code Hooks](docs/guides/claude-code.md) — copy-paste `settings.json`
- [Architecture](docs/reference/architecture.md) — package layout and threading contract
- [Design Spec](docs/development/spec.md) — original design rationale

`CLAUDE.md` at the repo root captures the project conventions enforced by review.
