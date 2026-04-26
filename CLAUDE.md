# Roost — Project Conventions

## What this is

A cross-platform (Mac + Linux) desktop terminal multiplexer built around libghostty-vt. Sidebar of projects, tabs per project, one terminal per tab. The differentiator is multi-project workspace with notification routing for AI coding agents (Claude Code, Codex, etc.). Inspiration: cmux. Constraint: smaller scope than cmux.

See `docs/development/spec.md` for the design doc and `docs/reference/architecture.md` for diagrams.

## Architecture

- Single GTK4 + libadwaita app written in Go.
- libghostty-vt is the terminal engine. Used for VT parsing, screen state, OSC parsing, key/mouse encoding.
- The renderer is ours — Cairo + Pango on a `GtkDrawingArea`. We walk libghostty-vt's render state and draw cell-aligned rects + text.
- The PTY is ours — `creack/pty`, one per tab.
- cgo lives ONLY in `internal/ghostty`. No cgo elsewhere.
- UI ↔ core boundary is via interfaces in `internal/core`. The UI never reaches into core internals directly.
- Treat the architecture as if core were a separate daemon, even though it isn't yet. This preserves a future option to split into a Go daemon + native UI clients without rewrites.

## Threading (critical)

GTK4 is strictly single-threaded. Every widget operation MUST happen on the main thread.

| Layer                       | Thread                              |
|-----------------------------|-------------------------------------|
| GTK widgets, draw, input    | Main thread only                    |
| PTY read/write              | Goroutine per tab                   |
| `ghostty_terminal_vt_write` | **Main thread**                     |
| `ghostty_render_state_*`    | **Main thread**                     |
| SQLite writes               | Goroutine-safe (database/sql handles locking) |
| OSC handler dispatch        | Goroutine; marshal via `glib.IdleAdd` |

PTY-read goroutine pushes raw bytes onto a per-tab buffered channel. A `glib.IdleAdd`-installed drain handler on the main thread pulls bytes and calls `ghostty_terminal_vt_write`. Never touch a libghostty-vt terminal handle from a goroutine. Never call any `gtk.*` or `glib.*` widget API from a goroutine.

When in doubt: if it touches GTK or libghostty-vt, it runs on the main thread.

## Library preferences

| Concern  | Library                                              | Notes                                  |
|----------|------------------------------------------------------|----------------------------------------|
| GTK4     | `github.com/diamondburned/gotk4/pkg/gtk/v4`          |                                        |
| Adwaita  | `github.com/diamondburned/gotk4-adwaita/pkg/adw`     |                                        |
| PTY      | `github.com/creack/pty`                              | Pure Go, no cgo                        |
| SQLite   | `modernc.org/sqlite`                                 | Pure Go, no cgo                        |
| libghostty-vt | cgo, only in `internal/ghostty`                 | Pinned Ghostty SHA in `build/build.sh` |

If you need a new dependency, prefer pure-Go. If you reach for cgo for anything other than libghostty-vt, stop and reconsider.

## Known gotk4 binding gotchas

- `pangocairo.ContextSetFontOptions` crashes — it expects `cairo.FontOptions` to follow the gextras "record" struct convention, but the cairo package uses a raw native pointer. Don't try to set Cairo font options through it; we accept the loss of integer-pixel hint metrics in exchange. Revisit if upstream fixes the mismatch.
- `pango_font_description_set_family` accepts a comma-separated list (Pango ≥ 1.46) but on macOS its fallback is unreliable — when the head of the list is missing it can drop to a *proportional* font (Verdana), giving wide cells with narrow glyphs. We resolve the family ourselves via `pickFontFamily` before calling `SetFamily`. Add new font defaults through that helper, not via raw comma-separated strings.

## Style

- Prefer flat package layouts and concrete types until duplication forces an interface. No `Manager`, `Coordinator`, `Service`, `Helper` — name things for what they are.
- Errors are returned, not logged-and-swallowed. Log at the boundary that handles them.
- Tests live in `_test.go` files next to the code, not in a separate `tests/` tree.
- Default to no comments. Add a comment only when the WHY is non-obvious — a hidden constraint, a workaround, a tricky invariant. Don't comment what well-named code already says.
- No `// TODO: ...` left in committed code. Either do it, file an issue, or leave a `// XXX:` for known dead-ends.

## Build

- `go build` does NOT build libghostty-vt. Run `./build/build.sh` for a full build.
- libghostty-vt is pinned to a specific Ghostty commit in `build/build.sh`. Don't bump without a separate PR and a rebuild test.
- Toolchain is managed via `mise`: `go 1.24+`, `zig 0.15.x`. Run `mise install` after cloning.
- Linux dev requires GTK4 + libadwaita dev packages: `apt install libgtk-4-dev libadwaita-1-dev` on Ubuntu.
- Mac dev requires Homebrew GTK4: `brew install gtk4 libadwaita`.

## What Roost is NOT

- Not a Ghostty replacement.
- Not a tmux replacement.
- Not multi-window. One window, projects in sidebar, tabs in projects.
- Not split-pane. One terminal per tab, period.
- Not a browser host.
- Not Windows. Mac + Linux only.
- Not git-aware in MVP. Sidebar is `{name, cwd}` only.
- No task tabs in MVP. Schema reserves the column; UI doesn't expose it yet.

## Useful references checked out next door

- `../ghostling/main.c` — single-file C reference for libghostty-vt embedding. Direct template for the spike.
- `../ghostty/include/ghostty.h` — the C API.
- `../ghostty/src/lib_vt.zig` — exhaustive list of exported C symbols.
- `../cmux/` — Swift/AppKit reference; data model and CLI protocol patterns.
