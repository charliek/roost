# Development Setup

Roost has two active development surfaces on the `feature/rust-port` branch:

1. The Rust workspace at `crates/` (daemon `roost-core`, CLI `roost-cli-rs`, Linux UI `roost-linux`, plus shared crates).
2. The Swift package at `mac/` (the macOS UI, `Roost.app`).

Both link the same vendored `libghostty-vt` static archive built from `third_party/ghostty/`. For iterating on the legacy Go binary still shipping from `main`, see [Legacy → Development setup](../reference/legacy-go/development-setup.md).

## Prerequisites

| Tool | Use |
|---|---|
| `mise` | Provisions Rust 1.85.0 + Zig 0.15.x at the pinned versions |
| Xcode Command Line Tools | Builds the Mac UI (SwiftPM) |
| GTK4 + libadwaita dev packages | Builds the Linux UI (Linux only) |
| `protoc` | Generates Rust + Swift bindings from `proto/roost.proto` |
| `uv` | Builds the documentation site |

See [Installation](../getting-started/installation.md) for the per-platform package commands.

## Initial build

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install                           # Rust + Zig
./third_party/ghostty/build.sh         # libghostty-vt — only needs Zig
~/.cargo/bin/cargo build --workspace   # all crates except roost-linux
~/.cargo/bin/cargo build -p roost-linux  # opt in once GTK is installed
PROTOC_PATH=$(which protoc) ./mac/scripts/bundle.sh debug
```

`third_party/ghostty/build.sh` is the only step that needs Zig. After it finishes, normal Rust + Swift workflows work without invoking Zig again.

## Iteration

| Goal | Command |
|---|---|
| Rebuild + run the daemon | `~/.cargo/bin/cargo run -p roost-core` |
| Run the Linux UI | `~/.cargo/bin/cargo run -p roost-linux` |
| Run the Mac UI | `PROTOC_PATH=$(which protoc) ./mac/scripts/bundle.sh debug && open mac/build/Roost.app` |
| Smoke-test the CLI | `~/.cargo/bin/cargo run -p roost-cli-rs -- identify` |
| Rust unit tests | `~/.cargo/bin/cargo test --workspace` |
| Mac unit tests | `cd mac && PROTOC_PATH=$(which protoc) swift test` |
| Rust formatting | `~/.cargo/bin/cargo fmt --all` |
| Rust lint | `~/.cargo/bin/cargo clippy --workspace --all-targets` |
| Build the docs site | `make docs` (or `make docs-serve` for live-reload at `http://127.0.0.1:7070`) |

The daemon socket lives at `~/Library/Caches/roost/roost.sock` on macOS and `$XDG_RUNTIME_DIR/roost/roost.sock` on Linux. The daemon log is written via `tracing-appender` to `/private/tmp/roost-core.log` on macOS (verify with `lsof -p $(pgrep -f roost-core) | grep log` when in doubt).

## Tests

Rust tests live next to the code they exercise. Major coverage:

| Crate | What's covered |
|---|---|
| `roost-common` | Path resolution, socket discovery |
| `roost-osc` | OSC 9 / 777 streaming parser, ST terminator, hook suppression |
| `roost-core` | Workspace event emission, reorder math, rollup priority, hook-active state |
| `roost-vt` | FFI smoke tests against the vendored `libghostty-vt` archive |

Mac tests are under `mac/Tests/RoostTests/`; they cover the renderer, OSC scanner, key encoder, drag/drop math, and tab pill state machine. They run in headless `swift test` (no NSWindow required for any covered surface).

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
- UI calls happen on the main thread; background work marshals via `glib.IdleAdd` (Linux) or `DispatchQueue.main` / `@MainActor` (macOS).
- The proto schema is the durable boundary — change `proto/roost.proto` first, regenerate, then change the consumers.
