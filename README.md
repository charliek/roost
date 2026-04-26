# Roost

A Mac + Linux desktop terminal multiplexer for AI coding agents. Sidebar of projects on the left, tabs per project, one terminal per tab. Built around libghostty-vt for terminal correctness, GTK4 + libadwaita for cross-platform UI, and Go for everything not on the cgo boundary.

This is an early work in progress. See `docs/spec.md` for the design and `CLAUDE.md` for project conventions.

## Building from source

Prerequisites: [`mise`](https://mise.jdx.dev/) for tool versioning. Clone next to the repo or anywhere convenient.

```bash
git clone https://github.com/charliek/roost
cd roost
mise install                  # provisions go + zig at the pinned versions
./build/build.sh libghostty   # clones Ghostty at the pinned SHA, builds libghostty-vt
./build/build.sh build        # builds ./roost and ./roost-cli
./roost
```

System packages:

- macOS: `brew install gtk4 libadwaita`
- Ubuntu/Debian: `sudo apt install libgtk-4-dev libadwaita-1-dev`

## Layout

- `cmd/roost/` — GUI binary entrypoint
- `cmd/roost-cli/` — companion CLI (Phase 3)
- `internal/ghostty/` — cgo bindings to libghostty-vt (the only cgo package)
- `build/build.sh` — orchestrates the libghostty-vt build and the Go build
- `docs/spec.md` — design doc
- `CLAUDE.md` — project conventions and the GTK threading contract
