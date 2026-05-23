# Development Setup

Roost has two active development surfaces on the `feature/rust-port` branch:

1. The Rust workspace at `crates/` (`roost-ipc`, `roost-vt`, `roost-osc`, `roost-cli`, `roost-linux`).
2. The Swift package at `mac/` (the macOS UI, `Roost.app`).

Both link the same vendored `libghostty-vt` static archive built from `third_party/ghostty/`. For iterating on the legacy Go binary still shipping from `main`, see [Legacy → Development setup](../reference/legacy-go/development-setup.md).

## Prerequisites

| Tool | Use |
|---|---|
| `mise` | Provisions Rust 1.85.0 + Zig 0.15.x at the pinned versions |
| Xcode Command Line Tools | Builds the Mac UI (SwiftPM) |
| GTK4 + libadwaita dev packages | Builds the Linux UI (Linux + macOS dev) |
| `uv` | Builds the documentation site |

See [Installation](../getting-started/installation.md) for the per-platform package commands.

## Initial build

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install                           # Rust + Zig
./third_party/ghostty/build.sh         # libghostty-vt — only needs Zig
~/.cargo/bin/cargo build --workspace   # all Rust crates
./mac/scripts/bundle.sh debug          # macOS .app bundle
```

`third_party/ghostty/build.sh` is the only step that needs Zig. After it finishes, normal Rust + Swift workflows work without invoking Zig again.

## Iteration

| Goal | Command |
|---|---|
| Run the Linux UI | `~/.cargo/bin/cargo run -p roost-linux` |
| Run the Mac UI | `./mac/scripts/bundle.sh debug && open mac/build/Roost.app` |
| Smoke-test the CLI | `~/.cargo/bin/cargo run -p roost-cli -- identify` |
| Rust unit tests | `~/.cargo/bin/cargo test --workspace --exclude roost-linux` |
| Linux UI tests | `~/.cargo/bin/cargo test -p roost-linux` (needs GTK) |
| Mac unit tests | `cd mac && swift test` |
| Rust formatting | `~/.cargo/bin/cargo fmt --all` |
| Rust lint | `~/.cargo/bin/cargo clippy --workspace --all-targets` |
| Build the docs site | `make docs` (or `make docs-serve` for live-reload at `http://127.0.0.1:7070`) |

The IPC sockets live at:

| OS | Mac UI socket | Linux UI socket |
|---|---|---|
| macOS | `~/Library/Caches/Roost/roost.sock` | `~/Library/Caches/Roost-gtk/roost.sock` (Gtk dev profile) |
| Linux | n/a | `$XDG_RUNTIME_DIR/roost/roost.sock` (falls back to `/tmp/roost-<uid>/roost.sock` if `XDG_RUNTIME_DIR` is unset) |

Both UIs write a log file **and** tee to stdout: the Mac app to `~/Library/Logs/Roost/roost.log`, the Linux UI (`roost-linux`) to `$XDG_STATE_HOME/roost/roost.log` (default `~/.local/state/roost/roost.log`). On macOS the Gtk dev profile uses a distinct `~/Library/Logs/Roost-gtk/roost.log`, so the Swift app and `roost-linux` don't clobber each other when run side by side. `roostctl --help` and `docs/reference/ipc.md` document the wire surface.

## Tests

Rust tests live next to the code they exercise. Major coverage:

| Crate | What's covered |
|---|---|
| `roost-ipc` | Frame reader/writer, JSON wire vectors, target selection (probe alive + env precedence) |
| `roost-osc` | OSC 9 / 777 streaming parser, ST terminator, hook suppression |
| `roost-vt` | FFI smoke tests against the vendored `libghostty-vt` archive (gated on `--features ffi`) |
| `roost-linux` | Workspace state machine, PTY supervisor, IPC dispatch, single-instance flock |
| `roost-cli` | Escape decoder, shell quoter, target arg mapping |

Mac tests are under `mac/Tests/RoostTests/`; they cover the workspace state machine, PTY supervisor lifecycle, IPC server framing, single-instance flock, renderer, OSC scanner, key encoder, drag/drop math, and tab pill state machine. They run in headless `swift test` (no NSWindow required for any covered surface).

## Documentation site

Markdown sources live in `docs/`. mkdocs-material builds them through `uv`:

```bash
make docs                # static site under site-build/
make docs-serve          # live-reload server at http://127.0.0.1:7070
```

`uv sync --group docs` runs automatically; no global Python install needed beyond the `uv` binary.

The voice for new docs is set in `mkdocs.yml`: professional + direct (no marketing), tables for option lists, code blocks with language hints, admonitions only for important notes/warnings, copy-pasteable examples, one topic per page.

## Bumping the pinned Ghostty SHA

`libghostty-vt`'s API is documented as unstable. Bumps land in their own commit and move two scripts in lockstep:

1. Edit `third_party/ghostty/build.sh` and `build/build.sh` — update `GHOSTTY_SHA` in both.
2. `./third_party/ghostty/build.sh --force` to rebuild from the new SHA.
3. Fix any FFI breakage in `crates/roost-vt`. The C symbols are listed in `src/lib_vt.zig` of the Ghostty source.
4. Re-run `cargo test --workspace` and `swift test` from `mac/`.
5. Commit with the SHA + date in the message.

If the bump also moves past Zig 0.15.x, drop the `maybe_arm64_sdk_shim` helper in `third_party/ghostty/build.sh` — it exists only because Zig 0.15.x links host artifacts as `arm64-macos`, which Apple's macOS 26+ SDK no longer exposes.

## Code conventions

The full set is in `CLAUDE.md` at the repo root. Highlights:

- Concrete types until duplication forces an interface — no premature `Manager` / `Coordinator` abstractions.
- Errors are returned, not logged-and-swallowed. Log at the boundary that handles them.
- Default to no comments. Add one when the *why* is non-obvious (a hidden constraint, a workaround, a tricky invariant).
- UI calls happen on the main thread; background work marshals via `glib::idle_add` (Linux) or `DispatchQueue.main` / `@MainActor` (macOS).
- The JSON IPC schema is the durable boundary — change `crates/roost-ipc/src/messages.rs`, update vectors under `tests/ipc-vectors/`, and bump the Swift mirror in `mac/Sources/Roost/IPCMessages.swift` in the same commit.
