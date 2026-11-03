# Development Setup

Roost is one Rust workspace at `crates/` — `roost-ipc`, `roost-engine`, `roost-ui-model`, `roost-vt`, `roost-osc`, `roost-url`, `roost-agent`, `roost-cli`, `roost-iced` (the UI, on macOS and Linux), `roost-client`, and `roost-session` (the headless host-session daemon). `mac/` holds the macOS bundle scripts and resources, not code. Everything links the same vendored `libghostty-vt` static archive built from `third_party/ghostty/`.

## Prerequisites

| Tool | Use |
|---|---|
| `mise` | Provisions Rust 1.97.1 + Zig 0.17.x at the pinned versions |
| Xcode Command Line Tools | Builds the Mac bundle (macOS) |
| `libclang-dev` + `pkg-config` | Build-time deps of `roost-vt`'s FFI bindings (Linux) |
| `uv` | Builds the documentation site and runs the E2E harness |

There are **no GTK packages** to install — `roost-iced` is GTK-free, and CI
enforces that with a dependency-boundary check (`make check-iced`). See
[Installation](../getting-started/installation.md#prerequisites) for the
per-platform package commands and the runtime library set the iced UI needs.

## Initial build

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install                 # Rust + Zig
make setup                   # + builds libghostty-vt (idempotent on cache hit)
make build                   # cargo build the workspace (iced UI + roostctl)
make bundle-iced             # macOS: assemble mac/build/Roost-Iced.app (debug)
```

`third_party/ghostty/build.sh` (what `make setup` runs) is the only step that
needs Zig. After it finishes, normal Rust workflows work without
invoking Zig again.

## Iteration

| Goal | Command |
|---|---|
| Build the Rust workspace | `make build` — or `cargo build -p roost-iced -p roost-cli` for just the UI + CLI |
| Run the UI | `make run-iced` — runs the dev `Iced` profile, so it never touches a packaged install's state |
| Start/stop the headless session daemon | `cargo build -p roost-session -p roost-cli && ROOST_SESSION_BIN=target/debug/roost-session target/debug/roostctl session start` (`session stop` to tear down) |
| Bundle the dev macOS app | `make bundle-iced` → `mac/build/Roost-Iced.app` (the shipped `Roost.app` is the same script with `ROOST_BUNDLE_IDENTITY=production`, normally CI's job) |
| Smoke-test the CLI | `cargo run -p roost-cli -- identify` |
| Rust unit tests | `cargo test --workspace` (`make test-rust` adds the cfg-gated `roost-vt` ffi tests) |
| Iced UI tests | `cargo test -p roost-iced` (`make test-iced`) |
| Harness unit tests | `make test-harness` |
| Everything above | `make test` |
| Rust formatting | `make fmt` (check-only: `make fmt-check`) |
| Rust lint at CI parity | `make clippy` |
| Iced gate incl. dependency boundaries + the `linux-package` build | `make check-iced` |
| Pre-push gate | `make check` — fmt-check + clippy + tests |
| Build the docs site | `make docs` (or `make docs-serve` for live reload at `http://127.0.0.1:7070`) |

`make` with no target lists everything.

### Profiles and sockets

Each UI resolves a **bundle profile** that owns its socket, state, lock, and
log paths, so an installed `Roost.app`, a packaged Linux install, and a dev iced build never
collide. `ROOST_BUNDLE_PROFILE` (`mac` \| `linux` \| `iced`) overrides the
compiled-in default.

| Profile | Who resolves it | Socket |
|---|---|---|
| `mac` | `Roost.app` | `~/Library/Caches/Roost/roost.sock` |
| `linux` | the packaged `.deb` (`/usr/bin/roost`) | `$XDG_RUNTIME_DIR/roost/roost.sock` |
| `iced` | every dev iced build, and the dev `Roost-Iced.app` | `$XDG_RUNTIME_DIR/roost-iced/roost.sock`, or `~/Library/Caches/Roost-iced/roost.sock` on macOS |
| `session` | the headless `roost-session` daemon (opt-in, `roostctl session start`) | `~/.local/state/roost-session/run/roost.sock` under the uid's passwd home on Linux (not `XDG_RUNTIME_DIR`), or `~/Library/Caches/RoostSession/roost.sock` on macOS — debug builds use `roost-session-dev` / `RoostSessionDev`; `ROOST_SESSION_SOCKET_DIR` replaces the directory |

Without `XDG_RUNTIME_DIR`, Linux falls back to `/tmp/roost-<uid>` and
`/tmp/roost-iced-<uid>` respectively. The session's socket never follows
`XDG_RUNTIME_DIR` ([why](../reference/paths.md#session-socket-linux)), so
a test or second dev session isolates itself with
`ROOST_SESSION_SOCKET_DIR=<short private dir>`. Select a live UI with `roostctl --target mac|linux|iced` —
auto-detect and `ROOST_BUNDLE_PROFILE` never consider `session`, but
`--target session` reaches one explicitly (#475). `roostctl session
start|stop|status` still keep their own pre-connect carve-out, driving
it directly ahead of all of that; any other op reaches a running
session via `--target session` or an explicit `roostctl --socket <path>`
(see [`cli.md`](../reference/cli.md#session-subcommands)). The full
table — state, locks, logs, and the macOS dev paths for the `linux`
profile — is in [Paths & Environment](../reference/paths.md).

The UI writes a log file **and** tees to stdout. It logs to
`$XDG_STATE_HOME/roost/roost.log` when it resolves the `linux` profile and
`.../roost-iced/roost.log` in a dev build; `roostctl --help` and
[`ipc.md`](../reference/ipc.md) document the wire surface.

## Tests

Rust tests live next to the code they exercise. Major coverage:

| Crate | What's covered |
|---|---|
| `roost-ipc` | Frame reader/writer, JSON wire vectors, path/profile resolution, target selection (probe alive + env precedence), agent-state fixtures |
| `roost-osc` | OSC 9 / 777 streaming parser, ST terminator, hook suppression |
| `roost-url` | URL detection, pinned against `tests/url-fixtures/` |
| `roost-vt` | FFI smoke tests against the vendored `libghostty-vt` archive (gated on `--features ffi`) |
| `roost-engine` | Workspace, PTY supervision, persistence, events, OSC routing, IPC dispatch, instance lock |
| `roost-ui-model` | Config, theme, keybind, palette, provider, and agent projection models |
| `roost-iced` | iced presentation, native ports, input, and the terminal rendering adapter — the UI both platforms ship |
| `roost-cli` | Escape decoder, shell quoter, target arg mapping, doctor checks + doc anchors, `session` verb decisions (verdict classification, stop-completion) |
| `roost-session` | Daemonize handshake, readiness verdicts, headless hydration, per-tab OSC drains, socket-guard identity-checked unlink (unlink-if-ours), lifecycle + fast-exit races |

Above the unit tier sit the functional E2E suite and the visual + real-input
harnesses:

```bash
make e2e-iced         # functional E2E against the iced UI (reuses a running one)
make e2e-iced-ci      # CI parity: ROOST_TEST_MODE=1 + --roost-fresh. DESTRUCTIVE.
make e2e-iced-bundle  # macOS: the bundle smoke against the dev Roost-Iced.app
```

See [Test automation](test-automation.md) for the layer map, the test-mode IPC
ops, the harness flags, and the CI lanes.

## Documentation site

Markdown sources live in `docs/`. [Zensical](https://zensical.org) builds them through `uv`:

```bash
make docs                # static site under site-build/ (`zensical build --strict`)
make docs-serve          # live-reload server at http://127.0.0.1:7070
```

`uv sync --locked --group docs` runs automatically; no global Python install needed beyond the `uv` binary. `zensical serve --strict` is unsupported — verify with `make docs`.

The docs are **not** part of `make check` or the `ci-success` gate; they have
their own workflows. Run `make docs` for any commit touching `docs/`,
`zensical.toml`, `pyproject.toml`, `uv.lock`, `CHANGELOG.md`, or either docs
workflow.

The voice for new docs: professional + direct (no marketing), tables for option lists, code blocks with language hints, admonitions only for important notes/warnings, copy-pasteable examples, one topic per page.

## Bumping the pinned Ghostty SHA

`libghostty-vt`'s API is documented as unstable. Bumps land in their own commit:

1. Edit `third_party/ghostty/build.sh` — update `GHOSTTY_SHA`.
2. `make ghostty-force` to rebuild from the new SHA.
3. Fix any FFI breakage in `crates/roost-vt`. The C symbols are listed in `src/lib_vt.zig` of the Ghostty source.
4. Re-run `make test`.
5. Commit with the SHA + date in the message.

Zig 0.15.x linked host artifacts as `arm64-macos`, which Apple's macOS 26+ SDK no longer exposed; `third_party/ghostty/build.sh` carried a `maybe_arm64_sdk_shim` helper to work around it. The toolchain pinned by the 2026-08-26 ghostty main-tip bump, version 0.16.0, resolved the system SDK correctly on its own, so the shim was deleted. If a future bump regresses this, restore the `sdk_has_plain_arm64`/`maybe_arm64_sdk_shim` block from git history (see `b6c46f7`). The 2026-10-27 bump to ghostty main `6ba14415` moved the pin to Zig 0.17.0.

## Code conventions

The full set is in `CLAUDE.md` at the repo root. Highlights:

- Concrete types until duplication forces an interface — no premature `Manager` / `Coordinator` abstractions.
- Errors are returned, not logged-and-swallowed. Log at the boundary that handles them.
- Default to no comments. Add one when the *why* is non-obvious (a hidden constraint, a workaround, a tricky invariant).
- UI calls happen on the main thread. That is the winit event-loop thread, reached from `update`; background work arrives through the single engine feed.
- The JSON IPC schema is the durable boundary — change `crates/roost-ipc/src/messages.rs`, and update the vectors under `tests/ipc-vectors/` in the same commit.
