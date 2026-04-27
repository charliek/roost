# Roost — Design Document

A Mac + Linux terminal multiplexer for AI coding agents, built as a single Go + GTK4 codebase using libghostty as the rendering engine. Similar in shape to cmux, but cross-platform and scoped tighter.

The name draws on the metaphor of a place where flocks gather between flights — each project is a roost where coding agents perch, work, and signal when they need attention. The CLI binary is `roost`; configuration lives in `~/.config/roost/` on Linux and `~/Library/Application Support/Roost/` on Mac.

## Overview

The product is a desktop application with a left sidebar of **projects**, where each project contains one or more **tabs**, and each tab hosts a single libghostty-rendered terminal pane. The primary use case is running multiple parallel Claude Code / Codex sessions across different repos and being able to tell at a glance which agents need attention.

Key differentiator vs. running multiple Ghostty windows: project-grouped vertical tab navigation, persistent per-project state, and notification routing surfaced in the sidebar when an agent is waiting for input.

## Goals

Cross-platform Mac + Linux from a single Go codebase. Native libghostty terminal rendering (GPU-accelerated via Metal/OpenGL, low memory, fast). Multi-project workspace with persistent tabs across restarts. Notification routing via OSC 9/99/777 escape sequences (post-MVP). Architecture friendly to AI-driven development — clear package boundaries, conventional Go idioms, agents can extend safely without architectural drift.

## Non-Goals (Explicit MVP Constraints)

No Windows support. No xterm.js or web-based terminals. No Electron, Tauri, or webview shell. No split panes within a tab — one terminal per tab, period. No embedded browser. No git worktree management (cmux feature, out of scope for MVP). One window with all projects — no multi-window. GTK4 on Mac will look like a GTK app on Mac; this is accepted.

## Technical Stack

### Core

| Concern | Choice | Notes |
|---|---|---|
| Language | Go 1.24+ (mise-managed) | Single language for ~95% of the codebase |
| UI toolkit | GTK4 + libadwaita | Single codebase for both platforms |
| GTK bindings | `github.com/diamondburned/gotk4` | Most actively maintained Go GTK4 bindings |
| Adwaita bindings | `github.com/diamondburned/gotk4-adwaita` | For headerbar, toast overlays, styled widgets |
| Terminal engine | libghostty (embedded apprt) | C API, Zig-compiled static library |
| PTY | `github.com/creack/pty` | Pure Go, no cgo |
| SQLite | `modernc.org/sqlite` | Pure Go, no cgo |
| FFI | cgo | Quarantined to `internal/ghostty` package only |

### Build & Distribution

Zig toolchain (for compiling libghostty from source). `gtk-mac-bundler` for Mac `.app` packaging. Linux distribution path TBD — likely AppImage or Flatpak. Mac distribution via Homebrew tap with a notarized `.dmg` (Phase 4).

## Architecture

### High-Level Shape

```
┌─────────────────────────────────────────────────────┐
│                  GTK4 UI Layer                      │
│   sidebar | tab strip | libghostty surface host     │
└──────────────────────┬──────────────────────────────┘
                       │  in-process function calls
┌──────────────────────┴──────────────────────────────┐
│                  Core (Go)                          │
│  workspace state | PTY supervisor | OSC parser      │
│  persistence (SQLite) | notification router         │
└──────────────────────┬──────────────────────────────┘
                       │  cgo (quarantined)
┌──────────────────────┴──────────────────────────────┐
│             libghostty (C API)                      │
│   surface lifecycle | rendering | font shaping      │
└─────────────────────────────────────────────────────┘
```

### Future-Proofing Note

Although the MVP is in-process (UI and core in one binary), the package boundaries should be drawn as if the core were a separate daemon. This preserves a future option to split into a Go daemon + native UI clients (e.g., a Swift/AppKit Mac client against the same daemon over a Unix socket) without rewrites. Concretely: the UI layer must only call into the core via well-defined Go interfaces and event channels, never reach into core internals directly.

### Suggested Package Layout

```
cmd/
  app/                  # main entrypoint
internal/
  core/                 # workspace + tab state, lifecycle
  pty/                  # PTY supervision (creack/pty wrapper)
  ghostty/              # cgo wrapper around libghostty (the only cgo package)
  osc/                  # OSC 9/99/777 parser
  store/                # SQLite persistence (modernc/sqlite)
  ui/
    sidebar/            # project sidebar widget
    tabs/               # tab strip widget
    surface/            # GTK widget hosting libghostty surface
    app.go              # GtkApplication setup, window, glue
  notify/               # native notifications (gio.Notification)
  config/               # config file parsing, paths
resources/
  terminfo/             # bundled terminfo entries (libghostty needs xterm-ghostty)
  shell-integration/    # bash/zsh/fish integration scripts
build/
  build.sh              # zig build for libghostty + go build
```

### Threading Model (Critical)

GTK4 is strictly single-threaded — all widget operations must happen on the main thread. PTY reads happen in goroutines; OSC parsing happens in goroutines; libghostty rendering is owned by libghostty itself on its own thread. **All UI updates from background goroutines must marshal back to the GTK main loop via `glib.IdleAdd`.**

This is the area most likely to produce subtle bugs that AI agents will not catch on their own. Design this contract explicitly before writing UI code, document it in `CLAUDE.md`, and make it a review checklist item.

## Key References

Read these before writing code; they are the primary sources of truth:

**Kytos blog post** (`jwintz.gitlabpages.inria.fr/jwintz/blog/2026-03-14-kytos-terminal-on-ghostty/`) — the best practical writeup of libghostty embedding, including xcframework wrapping, resource bundle requirements, and lifecycle gotchas. Mac-specific but the libghostty API patterns translate.

**cmux source** (`github.com/manaflow-ai/cmux`) — production reference for "libghostty + multiplexer-style UI." Swift/AppKit, but the data model, OSC notification handling, and CLI/socket API design are language-agnostic.

**Ghostling** (referenced from `github.com/ghostty-org/ghostty` README) — minimal complete project example of using libghostty. Read first for the embedding "hello world."

**Ghostty source** — specifically `src/apprt/embedded.zig` (the C API surface you'll bind against) and `src/apprt/gtk.zig` (Ghostty's own GTK4 app, the closest reference for what you're building). Read these as primary docs since libghostty has no stable external documentation yet.

**gotk4 reference apps** — `gotktrix` and `gtkcord4` (both by diamondburned) are real-world apps using gotk4 + libadwaita. Skim for idiomatic patterns, especially around custom widgets and async work.

## Phases

### Phase 0 — Spike (Days 1-2)

**Goal:** Prove the riskiest unknown — can libghostty be embedded in a GTK4 widget hosted in a Go app — before building anything else.

Empty Go binary. Single GTK4 window. Embed a child widget that hosts a libghostty surface. Run `$SHELL`. Type into it. Bytes flow. Window resizes correctly. That's the entire deliverable. No tabs, no sidebar, no persistence, no styling.

The two specific unknowns this de-risks: (1) the cgo binding to libghostty's embedded apprt API, and (2) the surface-hosting story on GTK4 specifically (Kytos demonstrated NSView; the GTK equivalent will likely involve a `GtkGLArea` or custom drawing area, this is where the most exploration happens).

**Exit criteria:** Working `go run` on both Mac (with brew GTK4) and Linux. Repo committed with a working build script.

### Phase 1 — Architectural Skeleton (Days 3-5)

**Goal:** Lock in the package structure, data model, and persistence layer so the agent has a clear scaffold to build into for the rest of the project.

Project structure per the layout above. Workspace data model: `Workspace` has many `Tab`s; `Tab` has `cwd`, `command`, `env`, `created_at`, `last_active`. SQLite schema and migrations. CRUD via `internal/store`. PTY supervisor that can spawn and kill processes, attach stdin/stdout/stderr, and survive process exit cleanly. Threading model document committed to `CLAUDE.md`. Config file location and parsing (XDG on Linux, `~/Library/Application Support/...` on Mac).

No UI work this phase beyond the spike window staying functional. The deliverable is a clean skeleton an agent can pattern-match into.

### Phase 2 — Multiplexer Core (Week 2)

**Goal:** First usable version. You can dogfood it.

Sidebar widget showing list of projects. Tab strip showing tabs in the active project. Click project → switch tab strip. Click tab → swap libghostty surface. New tab button creates a fresh shell tab. Close tab kills PTY and removes from DB. Rename tab and project. Persist all of this — restart the app, your projects and tabs come back (open shells with last cwd, do not auto-restart commands). Default keybindings (platform-native modifiers — Cmd on macOS, Ctrl plus Alt on Linux): `Cmd/Ctrl+T` new tab, `Cmd/Ctrl+W` close tab, `Cmd/Alt+R` rename tab, `Cmd/Ctrl+Shift+[` and `]` cycle tabs. Projects: `Cmd/Alt+N` new project, `Cmd/Alt+Shift+R` rename project, `Cmd+1..9` (macOS) / `Alt+1..9` (Linux) switches projects. `Ctrl+1..9` switches tabs in the active project on both platforms. Clipboard: `Cmd/Alt+C` copy selection, `Cmd/Alt+V` paste system clipboard, `Ctrl+Shift+C/V` as terminal-convention alternates; bare `Ctrl+C` is left as SIGINT. Click-drag selects cells, mouse wheel scrolls the local scrollback (or pass-through to the foreground app when it has mouse tracking on, with Shift bypassing per xterm convention), and on the alt screen the wheel is translated to ArrowUp/Down keystrokes for app navigation.

**Exit criteria:** You replace your daily Ghostty + manual project juggling with this for a week.

### Phase 3 — Notifications & Polish (Week 3-4)

**Goal:** The differentiator that justifies building this instead of using Ghostty.

OSC 9 / 99 / 777 parser intercepting bytes between PTY and libghostty (parser only — bytes still pass through to the terminal unchanged). On notification, fire a native desktop notification, set a "needs attention" badge on the sidebar entry for that project, ring the tab if not currently focused. Companion CLI binary (`roost notify --title "..." --body "..."`, `roost set-title`, `roost identify`, etc.) communicating with the running app over a Unix socket — same pattern as `cmux notify`. Document Claude Code hooks for `Stop` and `Notification` events that call the CLI.

Polish: file menu (or hamburger menu), preferences pane for theme/font, keybinding customization, error handling for crashes in libghostty surface.

**Exit criteria:** You can leave 4-5 Claude Code sessions running and be reliably notified when any of them needs you.

### Phase 4 — Distribution (Week 5+)

`gtk-mac-bundler` recipe producing a `.app` with all GTK4 + libadwaita + libghostty resources bundled. Code signing and notarization for Mac. Homebrew tap. Linux: AppImage or Flatpak (pick one based on what's pleasant to ship for your usage). Auto-updater (Sparkle on Mac, custom or OS-package-managed on Linux). Crash reporting.

This phase is genuinely a few weekends of unfun work. Defer until you actually want other people running it.

## Risks & Mitigations

**libghostty API instability.** The C API is unversioned and breaks across Ghostty commits. Mitigation: pin to a specific Ghostty commit in your build, treat upgrades as deliberate events with their own PR, and read Ghostty release notes before bumping.

**GTK4 ↔ Go cgo callback overhead.** Heavy use of cgo for hot paths (e.g., bytes flowing from PTY → ghostty surface) will be slow. Mitigation: keep the cgo boundary at a coarse granularity — pass buffers, not bytes. Profile early.

**Threading bugs from goroutine → GTK main thread crossings.** As noted, this is the most dangerous area. Mitigation: design the contract first, document in `CLAUDE.md`, make it a code review focus.

**AI agents over-architecting early.** Strong tendency to introduce `Manager` / `Coordinator` / `Service` interfaces for code that should be three structs. Mitigation: state in `CLAUDE.md` "prefer flat package layouts and concrete types until duplication forces an interface."

**AI agents reaching for cgo when pure Go alternatives exist.** Mitigation: explicit list in `CLAUDE.md` of "always pure Go" vs "cgo allowed" packages.

**libghostty resource bundle.** libghostty needs its terminfo and shell integration scripts findable at runtime. Forgetting these gives confusing failures. Mitigation: bundle in `resources/`, codify the path resolution in `internal/ghostty` early.

## Starter `CLAUDE.md` Content

Lift this into the project root and adapt:

```markdown
# Project Conventions

## Architecture
- Single GTK4 app, Go everywhere except libghostty FFI.
- libghostty cgo wrappers live ONLY in `internal/ghostty`. No cgo elsewhere.
- UI ↔ Core boundary is via interfaces in `internal/core`. The UI never reaches into core internals.
- Treat the architecture as if core were a separate daemon, even though it isn't yet.

## Threading
- GTK4 is single-threaded. All widget operations happen on the main thread.
- Background work (PTY reads, OSC parsing, DB writes) happens in goroutines.
- Marshalling work back to the UI MUST go through `glib.IdleAdd`.
- Never call GTK functions from a goroutine. Never.

## Library Preferences
- PTY: `github.com/creack/pty` (pure Go, no cgo)
- SQLite: `modernc.org/sqlite` (pure Go, no cgo)
- GTK4: `github.com/diamondburned/gotk4`
- Adwaita: `github.com/diamondburned/gotk4-adwaita`
- libghostty: cgo, in `internal/ghostty` only

## Style
- Prefer flat package layouts and concrete types until duplication forces an interface.
- Avoid `Manager`, `Coordinator`, `Service` types unless they're really earning it.
- Errors are returned, not logged-and-swallowed. Log at the boundary that handles them.
- Tests are in `_test.go` next to the code, not in a separate `tests/` tree.

## Build
- `go build` does not build libghostty. Run `./build/build.sh` for a full build.
- libghostty is pinned to commit `<TBD>` in `build/build.sh`. Don't bump without discussion.
- terminfo and shell-integration scripts must be present in `resources/` for libghostty to work at runtime.

## What This Project Is Not
- Not a Ghostty replacement. Not a tmux replacement.
- Not multi-window. One window, projects in sidebar, tabs in projects.
- Not split-pane. One terminal per tab.
- Not a browser host.
```

## Open Questions

These need answers before or during Phase 1:

1. **Tab content semantics.** Are tabs always plain shells, or do you want "task tabs" that auto-launch `claude`, `codex`, etc. with a saved command? cmux supports both.
2. **Restart behavior.** On app restart, should tabs re-spawn their previous command, or always open a fresh shell at last cwd? (My default suggestion: always fresh shell, but remember command history per tab so re-launching is one keystroke.)
3. **Theme & config.** Inherit Ghostty config (`~/.config/ghostty/config`) for fonts/colors like cmux does, or own config file, or both?
4. **Companion CLI design.** Do you want a CLI that can be invoked from inside a tab (`roost notify ...`, `roost set-title ...`)? Strong recommend yes for Claude Code hook integration in Phase 3.
5. **Project model.** Is a "project" just `{name, default cwd}`, or do you want git-aware features (current branch in sidebar, like cmux)?
6. **Default shell.** Inherit user's `$SHELL`, or per-project configurable?
7. **Linux primary target.** Which distros do you actually run this on? Affects libghostty Zig build target and what package format makes sense.
8. **Mac distribution path.** Brew tap with notarized `.dmg`, or are you fine running unsigned during dev?
9. **Future Mac client exit.** Do you want to actively design for the two-UI exit (Swift/AppKit Mac client later), or accept this is unlikely and design accordingly? Affects how strictly to police the UI ↔ core boundary.

## Decision Log

| Date | Decision | Rationale |
|---|---|---|
| 2026-04-26 | Mac + Linux only, drop Windows | libghostty Windows embedding is bleeding-edge, not worth the cost |
| 2026-04-26 | GTK4 single codebase over two-UI | Tolerable Mac aesthetics, dramatic dev cost reduction |
| 2026-04-26 | Go over Rust | AI-driven dev favors Go's training corpus + fast compile loop |
| 2026-04-26 | libghostty over xterm.js | Performance, smaller binary, native rendering |
| 2026-04-26 | gotk4 over alternatives | Most active maintenance, libadwaita support |
| 2026-04-26 | Name: Roost | Single-word, evocative metaphor (gathering place for agents), no major dev-tool conflict |
