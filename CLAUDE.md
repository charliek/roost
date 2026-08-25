# Roost — Project Conventions

## Direction (read first)

Roost is a cross-platform (Mac + Linux) desktop terminal multiplexer
built around libghostty-vt. It ships **two platform products** — Swift
+ AppKit on macOS and Rust + iced on Linux — that each embed the
workspace + PTY supervisor in-process and serve a JSON IPC socket for
external tooling (`roostctl`, Claude hooks). No daemon.

* Mac UI: Swift + AppKit, `mac/` (bundle id `ai.stridelabs.Roost`).
* Linux UI (shipped): Rust + iced, `crates/roost-iced/` (packaged as `/usr/bin/roost`).
* CLI: `crates/roost-cli/` (binary `roostctl`).
* IPC + path resolution: `crates/roost-ipc/`.
* libghostty-vt FFI + OSC: `crates/roost-vt/`, `crates/roost-osc/`.

**North star.** Every surface — UI clicks, hotkeys, `roostctl`, and Lua
scripts — routes through **one core: the workspace operation set**; the
UI is a *reaction* to the core's events, never its own source of truth.
One contract (`roost-ipc`'s op set), two implementations (Swift + AppKit,
Rust + iced) kept at behavioral parity. Optimize for **testability,
programmability, clean architecture**: adding a capability is "add an op
+ thin adapters", not per-surface logic. When in doubt, ask: *does this
route through the one op set, keep the UI reactive, and stay at parity
across both implementations?*

See [docs/development/vision.md](docs/development/vision.md) for the full
architecture, principles, and decision log;
[docs/reference/ipc.md](docs/reference/ipc.md) for the JSON IPC wire
format.

## Branch policy

`main` is the primary branch — the Rust + Swift port is the direction
(the `feature/rust-port` refactor branch merged into `main` and is
retired). Topic branches (`polish/*`, `refactor/*`, feature branches)
open PRs into `main`. **Merges are manual**: CI must be green, then the
committer merges — no auto-merge (the repo's `allow_auto_merge` is off;
use `/merge-pr`). The single required check is **`ci-success`** from
`.github/workflows/ci.yml` (rust/swift/iced build+test plus the functional
E2E jobs — `e2e-iced` (within `iced-build-e2e`) and `e2e-mac` — path-filtered
so jobs run only when relevant code changes). Releases gate on the same
`ci-success` via a `ci-gate` job in `release.yml`.

`claude/discuss-architecture-refactor-cjU3E` is the predecessor refactor
branch and is **frozen** at `00b3d10`. Do not start new work on it.

## What this is

Sidebar of projects, tabs per project, one terminal per tab. The
differentiator is multi-project workspace with notification routing
for AI coding agents (Claude Code, Codex, etc.). Inspiration: cmux.
Constraint: smaller scope than cmux.

See `docs/development/vision.md` for the design rationale and
`docs/reference/architecture.md` for diagrams.

## Architecture

- Two UIs (Swift Mac; Rust + iced Linux, what the package ships). Each
  embeds the workspace + PTY supervisor in-process.
- libghostty-vt is the terminal engine on both UIs — VT parsing,
  screen state, OSC parsing, key/mouse encoding.
- The renderer is ours on both: AppKit + Core Graphics on Mac, iced +
  wgpu on Linux. We walk libghostty-vt's render state and draw
  cell-aligned rects + text.
- The PTY is ours — `forkpty(3)` directly on Mac (`mac/Sources/Roost/
  PtySupervisor.swift`), `portable-pty` on Linux (`crates/roost-engine/
  src/pty.rs`, the shared engine crate the iced UI embeds). One PTY per
  tab.
- External tools dial the running UI process at the bundle profile's
  socket path (`~/Library/Caches/Roost/roost.sock` for Mac,
  `$XDG_RUNTIME_DIR/roost/roost.sock` for Linux — fallback
  `/tmp/roost-<uid>/roost.sock`). The wire format is newline-delimited
  JSON; see `docs/reference/ipc.md`.

## Threading (critical)

AppKit is strictly single-threaded for UI work; iced's winit event loop
is likewise single-threaded for its `update`/`view` cycle. Every widget
operation MUST happen on that thread. libghostty-vt handles + `vt_write`
calls are also main-thread-only.

| Layer                              | Thread / Actor                                                                |
|-------------------------------------|--------------------------------------------------------------------------------|
| AppKit widgets / iced `update`+`view` | Main thread only (iced: the winit event loop thread)                        |
| libghostty-vt handle + vt_write     | Main thread only, driven from `update` on Linux                              |
| PTY read                            | DispatchSourceRead (macOS) / per-tab `tokio::spawn_blocking` read loop (Linux, `roost-engine`); background thread |
| PTY write                           | Main thread (Mac, `@MainActor`) / a per-tab ordered `tokio` task draining an input+resize command channel (Linux) |
| Engine → UI feed                    | Workspace events + PTY output + IPC requests land on one channel from background tokio tasks; a `Notify`-backed iced `Subscription` wakes `update` to drain it on the main thread (`crates/roost-iced/src/engine_feed.rs`) |
| OSC dispatch                        | Lifted to main via `DispatchQueue.main.async` (Mac) / delivered through the engine feed above (Linux) |
| IPC server accept loop              | Detached background task; handler hops back to main via the engine feed for state mutations |

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
  truecolor` + `FORCE_HYPERLINK=1` injected before execve.

When in doubt: if it touches AppKit, iced's `update`/`view`, or
libghostty-vt, it runs on the main thread.

## Library preferences

| Concern              | Library                                                        | Notes                                                                                                |
|----------------------|----------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| AppKit (Mac)         | stdlib / direct                                                | SwiftPM executable target.                                                                            |
| PTY                  | `forkpty(3)` (Swift, Mac) / `portable-pty` (Rust, Linux)       | Mac uses raw C; `portable-pty` lives in `roost-engine`, shared by the iced UI. Both spawn one PTY per tab. |
| Persistence          | `state.json` (atomic tmp + rename; write-through, fsync on clean exit) | No SQLite. Projects, next_id, and per-project tab **layout** (title+cwd+position) + active selection — relaunch re-opens prior tabs as fresh shells in their dirs (no process/scrollback). Inline write-through during the session (page-cache cheap, no fsync); `Workspace::flush()` fsyncs on clean exit and freezes further writes. Crash loses at most the kernel writeback window; the atomic rename means the file is never torn. |
| libghostty-vt        | cgo via `roost-vt` (`--features ffi`)                          | Pinned Ghostty SHA in `third_party/ghostty/build.sh`.                                                |
| JSON IPC             | `roost-ipc` (server + client + framing + paths + target picker) | Newline-delimited JSON, 16 MiB frame cap; client + server share the wire-types module.               |
| swash (vendored patch) | `third_party/swash` via `[patch.crates-io]`                  | Pristine 0.2.10 pinned, plus a small set of malformed-font guards (issues #292 + #299 — debug SIGABRTs, an unbounded name-table read, and a hang when iced/cosmic-text shapes such a font). `README.roost.md` enumerates the deltas + removal condition. |
| zbus (Linux notifications) | iced-side `org.freedesktop.Notifications` (`crates/roost-iced`) | Spec session-bus client, not a DE-specific stack. macOS backend deliberately absent — issue #303. |
| arboard                | iced-side clipboard image read on paste (`crates/roost-iced`) | `image-data` + `wayland-data-control` with X11 fallback; PNG encoding stays on the existing `png` crate. |
| Inter (bundled font)   | `third_party/inter` (`include_bytes!` via `roost-iced`)       | v4.1 static Regular/Medium/SemiBold, SIL OFL 1.1; iced chrome font only (terminal cells keep the configured monospace); single `chrome_font()` seam so a future config swap is small. `README.roost.md` has provenance + removal condition. |

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
- **Linux UI logs**: the shipped iced UI writes `$XDG_STATE_HOME/roost/roost.log`
  (default `~/.local/state/roost/roost.log`) **and** tees to stdout
  (synchronous file appender in `crates/roost-iced/src/main.rs`, so
  entries survive a crash). `tail -f` it while reproducing; set
  `RUST_LOG=info,roost_ipc=debug` to adjust.
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
- **Test-harness map**: the harnesses are organized in three layers —
  see [`tools/README.md`](tools/README.md) (functional / visual /
  real-input) for which to reach for.
- **Linux testing on a Mac**: the iced UI's real-input tiers (X11/Xvfb,
  weston/Wayland, and the cage+uinput Wayland pointer-drag guard) only
  run on Linux, and Docker Desktop can't run the cage+uinput tier (no
  `/dev/uinput`). Use a **shed** (Apple VZ Linux microVM, real kernel +
  uinput) via the **`linux-test` skill** / `tools/shed/shed-test.sh`
  (mounts the repo, provisions via `.shed/provision.yaml`, builds
  `roost-iced` + `roostctl` shed-local, runs the three iced real-input
  lanes — mirrors CI's `e2e-iced-wayland-drag`).
- **Linux testing natively (Pop!_OS COSMIC)**: on a Linux dev box you
  don't need a VM — build + run the suite directly. The **`popos-test`
  skill** covers the apt deps, `make e2e-iced` / `e2e-iced-ci` under the
  live session and the weston headless tier, the **seat0 caveat** (the
  live COSMIC session owns input, so the cage+uinput real-input tier
  can't run locally — use CI/shed), workspace isolation, and where logs
  live.
- **Visual smoke (screenshots)**: `tools/screenshot/` drives either UI
  through `roostctl` (`launch.sh`/`quit.sh`/`smoke.sh <mac|iced>`),
  captures labeled screenshots + a `manifest.md` of per-shot
  expectations, and includes `pngtool.py` (stdlib PNG inspect/crop —
  cross-platform) for programmatic pixel assertions. One harness covers
  both UIs. See [`tools/screenshot/README.md`](tools/screenshot/README.md).
- **Real-input injection (Linux)**: [`tools/input/linux/`](tools/input/linux/README.md)
  exercises the actual key-encoder + mouse-gesture + clipboard path on
  Linux (COSMIC/Wayland) with no image libraries — `/dev/uinput`
  key/pointer injectors, a clipboard reader, and a single-monitor helper
  for reliable absolute-pointer injection. Linux-only (a Mac CGEvent
  sibling at `tools/input/mac/` is planned). See its README for the
  screen↔window coordinate mapping and gotchas.
- **Functional E2E (pytest)**: [`tools/roosttest/`](tools/roosttest/README.md)
  is the primary automated suite — a thin Python IPC client drives a real
  UI (`--roost-target mac|iced`) and asserts on the op set (`tab.dump` /
  `tab.list` / `palette.*` / `identify`), so it exercises exactly what
  users + `roostctl` drive. No sleeps (condition waits), content via text
  not pixels. `make e2e` dispatches to a curated lane per target (iced is
  the default; `make e2e-iced` / `e2e-mac` run them directly); **both
  `e2e-iced` (within `iced-build-e2e`) and `e2e-mac` are required CI
  gates** (iced headless under Xvfb/weston; Mac on a macOS GUI-session
  runner — the harness clears any stale instance before launch and scales
  its timeouts up via `ROOST_TEST_TIMEOUT_SCALE`). This is where new
  cross-cutting behavior gets a regression test; the screenshot
  (`tools/screenshot/`) and input-injection (`tools/input/linux/`) harnesses cover
  what it can't (pixels, real key/pointer events).
  - **Test-mode IPC ops** (`ROOST_TEST_MODE=1` at UI launch — CI sets
    it): `tab.feed_pty_bytes` injects bytes into a live tab's drain,
    `tab.capture_pty_input` reads what the UI queued onto the input
    side, and the (ungated) `tab.dump_resolved` walks the viewport
    through the production color resolver. Together they cover
    byte-level OSC/reply wiring end-to-end without needing a real
    shell — pattern walk in
    [`tools/roosttest/README.md`](tools/roosttest/README.md#osc-routed-regression-patterns).
    A sibling env gate, `ROOST_TEST_PANIC`, forces the crash-report +
    abort path in the iced UI for end-to-end verification: `=1` panics
    on the main thread at startup, `=thread` panics from a named
    background thread. It fires right after the panic hook is installed
    and before the single-instance lock, so it never touches a running
    instance.

## Docs

**Not part of `make check` or the `ci-success` gate.** The docs site has
its own toolchain (uv/Python) and its own CI workflows; the Rust/Swift
gates do not cover it. Run it for commits touching `docs/`,
`zensical.toml`, `pyproject.toml`, `uv.lock`, `CHANGELOG.md`, or either
docs workflow —
both workflows trigger on those shared inputs (and each additionally on
its own file), because a dependency or lockfile change can break the
build just as easily as a content change:

```bash
make docs            # uv run --locked zensical build --strict
make docs-serve      # preview on http://127.0.0.1:7070
```

The site is [Zensical](https://zensical.org) (not MkDocs — migrated 2026-08),
configured in `zensical.toml`, built into `site-build/`. `--strict` fails
on broken links and anchors and is what both CI workflows run, so run it
locally before pushing docs changes. Note `zensical serve --strict` is
unsupported; verify strictness via `build`.

The look comes from the shared
[stridelabs-docs-theme](https://github.com/charliek/stridelabs-docs-theme)
package, pinned by tag in `pyproject.toml`. Palette, fonts and feature
toggles live there, not here — do not add `theme.palette`, `theme.features`,
or a `[project.theme.font]` table to `zensical.toml`. The last is the
sharp edge: it re-enables Zensical's Google Fonts `<link>` on every page
while the theme's self-hosted faces keep loading anyway.

Working notes in `discovery/` live outside `docs/` on purpose. Zensical
has no `exclude_docs` equivalent; files under `docs_dir` are published.

Two gotchas worth knowing: Zensical **silently ignores unknown config
keys** even under `--strict`, so a green build does not prove a config
edit did what you meant; and the `pymdownx.emoji` callables live in the
`zensical.extensions.emoji` namespace — the Material for MkDocs
`material.extensions.emoji` namespace aborts the build.

## Build

- libghostty-vt is pinned to a specific Ghostty commit in
  `third_party/ghostty/build.sh`. Run it once before the first
  `cargo build` or `swift build`; it caches.
- Toolchain via `mise`: rust pinned in `rust-toolchain.toml`, zig
  `0.15.x`. Run `mise install` after cloning.
- Linux dev: the iced UI itself needs no GTK packages; `apt install
  libgtk-4-dev libadwaita-1-dev` (Ubuntu) is only needed to build
  `tools/input/linux/iced_native_file_drop_check.py`'s throwaway GTK
  app, which it launches as an XDND drag source to exercise
  `roost-iced`'s native file-drop target.
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
