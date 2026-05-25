# Roost — Project Conventions

## Direction (read first)

Roost is a cross-platform (Mac + Linux) desktop terminal multiplexer
built around libghostty-vt. The architecture is two native UIs that
each embed the workspace + PTY supervisor in-process and serve a JSON
IPC socket for external tooling (`roostctl`, Claude hooks). No daemon.

* Mac UI: Swift + AppKit, `mac/` (bundle id `ai.stridelabs.Roost`).
* Linux UI: gtk4-rs + libadwaita, `crates/roost-linux/`.
* CLI: `crates/roost-cli/` (binary `roostctl`).
* IPC + path resolution: `crates/roost-ipc/`.
* libghostty-vt FFI + OSC: `crates/roost-vt/`, `crates/roost-osc/`.

See [docs/development/vision.md](docs/development/vision.md) for the
target architecture and phased path; [docs/reference/ipc.md](docs/reference/ipc.md)
for the JSON IPC wire format.

## Branch policy

`main` is the primary branch — the Rust + Swift port is the direction
(the `feature/rust-port` refactor branch merged into `main` and is
retired). Topic branches (`polish/*`, `refactor/*`, feature branches)
open PRs into `main`. **Merges are manual**: CI must be green, then the
committer merges — no auto-merge (the repo's `allow_auto_merge` is off;
use `/merge-pr`). The single required check is **`ci-success`** from
`.github/workflows/ci.yml` (rust/swift/gtk, path-filtered so jobs run
only when relevant code changes). The legacy Go CI
(`.github/workflows/go-legacy.yml`) runs only on Go-file changes and is
not required.

The legacy Go code (`cmd/`, `internal/`, `go.mod`, `build/`) is retained
but secondary; it is removed in the **GODELETE** step once Rust/Swift
parity is confirmed — see [`plans/GODELETE.md`](plans/GODELETE.md).

`claude/discuss-architecture-refactor-cjU3E` is the predecessor refactor
branch and is **frozen** at `00b3d10`. Do not start new work on it.

## What this is

Sidebar of projects, tabs per project, one terminal per tab. The
differentiator is multi-project workspace with notification routing
for AI coding agents (Claude Code, Codex, etc.). Inspiration: cmux.
Constraint: smaller scope than cmux.

See `docs/development/spec.md` for the design doc and
`docs/reference/architecture.md` for diagrams.

## Architecture

- Two UIs (Swift Mac, gtk4-rs Linux). Each embeds the workspace + PTY
  supervisor in-process.
- libghostty-vt is the terminal engine on both UIs — VT parsing,
  screen state, OSC parsing, key/mouse encoding.
- The renderer is ours on both sides: AppKit + Core Graphics on Mac,
  Cairo + Pango on `GtkDrawingArea` on Linux. We walk
  libghostty-vt's render state and draw cell-aligned rects + text.
- The PTY is ours — `forkpty(3)` directly on Mac (`mac/Sources/Roost/
  PtySupervisor.swift`), `portable-pty` on Linux (`crates/roost-linux/
  src/daemon/pty.rs`). One PTY per tab.
- External tools dial the running UI process at the bundle profile's
  socket path (`~/Library/Caches/Roost/roost.sock` for Mac,
  `$XDG_RUNTIME_DIR/roost/roost.sock` for Linux — fallback
  `/tmp/roost-<uid>/roost.sock`). On macOS the Gtk dev profile uses
  `Roost-gtk` in place of `Roost`. The wire format is newline-delimited
  JSON; see `docs/reference/ipc.md`.

## Threading (critical)

GTK4 and AppKit are both strictly single-threaded for UI work. Every
widget operation MUST happen on the main thread. libghostty-vt
handles + `vt_write` calls are also main-thread-only.

| Layer                       | Thread / Actor                                                              |
|-----------------------------|------------------------------------------------------------------------------|
| GTK / AppKit widgets        | Main thread only                                                            |
| libghostty-vt handle + vt_write | Main thread only                                                        |
| PTY read                    | DispatchSourceRead (macOS) / per-tab tokio task (Linux); background thread  |
| PTY write                   | Main thread (called from `@MainActor` on Mac, `Workspace` on Linux)         |
| OSC dispatch                | Lifted to main via `DispatchQueue.main.async` (Mac) / `glib::idle_add` (Linux) |
| IPC server accept loop      | Detached background task; handler hops back to main for state mutations     |

### Swift threading subsection (Mac)

* libghostty-vt handles and `vt_write` calls: `@MainActor` only.
* PTY read: `DispatchSourceRead` on a background `DispatchQueue`.
  The handler is installed via a `nonisolated static` helper so the
  closure literal doesn't inherit `@MainActor` isolation — under
  Swift 6 strict concurrency, an inferred-MainActor closure body
  trips `dispatch_assert_queue(main)` from the dispatch worker
  thread. The handler yields onto a `Sendable AsyncStream<...>` that
  a separate `Task { @MainActor in ... }` drains.
* PTY write to master fd: from `@MainActor` (no concurrent writes
  per tab; ordering preserved).
* Resize: `ioctl(TIOCSWINSZ)` from `@MainActor`.
* Exit: SIGCHLD + `waitpid(WNOHANG)` from the main-actor drain
  task. The blocking reap loop (SIGHUP + waitpid loop + SIGKILL
  fallback) runs on a background DispatchQueue to avoid freezing
  AppKit; it signals completion back through the same AsyncStream
  bridge.
* Env: `ROOST_TAB_ID` + `ROOST_SOCKET` + `TERM` + `COLORTERM=
  truecolor` injected before execve.

When in doubt: if it touches GTK / AppKit or libghostty-vt, it runs
on the main thread.

## Library preferences

| Concern              | Library                                                        | Notes                                                                                                |
|----------------------|----------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| GTK4 (Linux)         | `gtk4-rs` + `libadwaita-rs`                                    | Sys deps from Homebrew on Mac dev, apt on Linux.                                                     |
| AppKit (Mac)         | stdlib / direct                                                | SwiftPM executable target.                                                                            |
| PTY                  | `forkpty(3)` (Swift, Mac) / `portable-pty` (Rust, Linux)       | Mac uses raw C; Linux uses a maintained safe-API crate. Both spawn one PTY per tab.                  |
| Persistence          | `state.json` (atomic tmp + rename; write-through, fsync on clean exit) | No SQLite. Projects, next_id, and per-project tab **layout** (title+cwd+position) + active selection — relaunch re-opens prior tabs as fresh shells in their dirs (no process/scrollback). Inline write-through during the session (page-cache cheap, no fsync); `Workspace::flush()` fsyncs on clean exit and freezes further writes. Crash loses at most the kernel writeback window; the atomic rename means the file is never torn. |
| libghostty-vt        | cgo via `roost-vt` (`--features ffi`)                          | Pinned Ghostty SHA in `third_party/ghostty/build.sh`.                                                |
| JSON IPC             | `roost-ipc` (server + client + framing + paths + target picker) | Newline-delimited JSON, 16 MiB frame cap; client + server share the wire-types module.               |

If you need a new dependency, prefer Sendable-safe / pure-Rust /
pure-Swift options. cgo via `roost-vt` is permitted because there's
no Swift binding for libghostty-vt's C API; reaching for it elsewhere
requires the same justification bar — name the constraint, keep the
wrapper small.

## Style

- Prefer flat package layouts and concrete types until duplication
  forces an interface. No `Manager`, `Coordinator`, `Service`,
  `Helper` — name things for what they are.
- Errors are returned, not logged-and-swallowed. Log at the boundary
  that handles them.
- Rust tests live in `_test.rs` files in `tests/`; Swift tests use
  `swift-testing` in `mac/Tests/RoostTests/`.
- Default to no comments. Add a comment only when the WHY is
  non-obvious — a hidden constraint, a workaround, a tricky
  invariant. Don't comment what well-named code already says.
- No `// TODO: ...` left in committed code. Either do it, file an
  issue, or leave a `// XXX:` for known dead-ends.

## Troubleshooting

- **Mac UI logs**: `~/Library/Logs/Roost/roost.log` (file appender,
  see `mac/Sources/Roost/Logging.swift`). Also `log show --predicate
  'process == "Roost"' --info --last 60s` for the os.Logger output.
  Note that `NSLog`/`os_log` redacts string interpolations as
  `<private>` by default; the file appender uses `privacy: .public`
  to defeat that. For raw values without redaction, prefer the file
  log.
- **Linux UI logs**: `roost-linux` writes `$XDG_STATE_HOME/roost/roost.log`
  (default `~/.local/state/roost/roost.log`) **and** tees to stdout
  (synchronous file appender in `crates/roost-linux/src/main.rs`, so
  entries survive a crash). `tail -f` it while reproducing; set
  `RUST_LOG=info,roost_ipc=debug` to adjust. On macOS the Gtk dev
  profile writes `~/Library/Logs/Roost-gtk/roost.log` — a **distinct**
  file from the Swift app's `~/Library/Logs/Roost/roost.log`, so both UIs
  can run side by side without clobbering each other's log.
- **IPC wire trace**: launch the UI with `RUST_LOG=roost_ipc=debug`
  (Linux) or `OS_ACTIVITY_MODE=disable` + `swift run` (Mac) to see
  per-frame logging. The wire format is human-readable JSON; `nc -U`
  can hand-craft requests against the socket for debugging.
- **Claude integration testing**: see
  [docs/development/claude-testing.md](docs/development/claude-testing.md)
  for end-to-end test instructions covering tab state, notification
  banners, sidebar rollup, and the hook lifecycle. To *see* the live
  UI when verifying a change, `roostctl screenshot --out /tmp/shot.png`
  renders the running window to a PNG in-process (no OS screen capture;
  works even when the window is unfocused or occluded).
- **Linux UI test harness**: [`tools/linux/`](tools/linux/README.md)
  drives the GTK app in an automated way on Linux (COSMIC/Wayland) with
  no image libraries — `/dev/uinput` key/pointer injectors, a stdlib PNG
  inspect/crop tool, a clipboard reader, and a single-monitor helper for
  reliable absolute-pointer injection. See its README for the
  screen↔window coordinate mapping and gotchas.

## Build

- libghostty-vt is pinned to a specific Ghostty commit in
  `third_party/ghostty/build.sh`. Run it once before the first
  `cargo build` or `swift build`; it caches.
- Toolchain via `mise`: rust pinned in `rust-toolchain.toml`, zig
  `0.15.x`. Run `mise install` after cloning.
- Linux dev: `apt install libgtk-4-dev libadwaita-1-dev` (Ubuntu).
- Mac dev: `brew install gtk4 libadwaita` (the GTK side links them
  on Mac too because `roost-linux` works on macOS for cross-platform
  development).
- Mac UI build: `cd mac && swift build` (no `protoc`, no plugin).
- Mac UI bundle: `mac/scripts/bundle.sh debug` produces
  `mac/build/Roost.app`.

## What Roost is NOT

- Not a Ghostty replacement.
- Not a tmux replacement.
- Not multi-window. One window, projects in sidebar, tabs in
  projects.
- Not split-pane. One terminal per tab, period.
- Not a browser host.
- Not Windows. Mac + Linux only.
- Not git-aware in MVP. Sidebar is `{name, cwd}` only.
- No task tabs in MVP. The schema reserves the column; the UI
  doesn't expose it yet.

## Useful references checked out next door

- `../ghostling/main.c` — single-file C reference for libghostty-vt
  embedding. Direct template for the spike.
- `../ghostty/include/ghostty.h` — the C API.
- `../ghostty/src/lib_vt.zig` — exhaustive list of exported C
  symbols.
- `../cmux/` — Swift/AppKit reference; data model and CLI protocol
  patterns.
