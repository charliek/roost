# Development Setup

Roost is a Go module with two cgo packages — `internal/ghostty` (links the Zig-built libghostty-vt static library) and `internal/pangoextra` (a small wrapper around `pango_cairo_context_set_font_options`, linked dynamically via `pkg-config: pangocairo`). Outside of the Zig step for libghostty-vt, day-to-day development is normal `go build` / `go test`.

## Prerequisites

| Tool                     | Use                                          |
|--------------------------|----------------------------------------------|
| `mise`                   | Provisions Go 1.24 + Zig 0.15.2              |
| GTK4 + libadwaita        | Linker dependencies (and runtime UI)         |
| `pkgconf` + GObject Introspection | gotk4 build prerequisites           |
| `uv`                     | Documentation site (mkdocs-material)         |

See [Installation](../getting-started/installation.md) for the per-platform package commands.

## Initial build

```bash
git clone https://github.com/charliek/roost.git
cd roost
mise install            # Go + Zig at pinned versions
make libghostty         # clones Ghostty + builds libghostty-vt under build/out/
make build              # produces ./roost and ./roost-cli
```

`make libghostty` is the only step that needs Zig. After it finishes, normal Go workflows work without invoking Zig again.

## Iteration loop

```bash
make build              # rebuild both binaries
./roost                 # run the GUI
./roost-cli identify    # smoke-test the IPC path
go test ./...           # run unit tests across all packages
go vet ./...            # static checks
```

`make build` runs `go build` against the already-built libghostty-vt artifact under `build/out/`. You don't need to rerun `make libghostty` unless the pinned Ghostty SHA changes.

## Tests

Tests live next to the code they exercise (no separate `tests/` tree):

| Package              | What's covered                                                      |
|----------------------|---------------------------------------------------------------------|
| `internal/config`    | XDG / Application Support path resolution                           |
| `internal/store`     | SQLite schema, migrations, CRUD, cascade delete                     |
| `internal/core`      | Workspace event emission, default-state bootstrap                   |
| `internal/ghostty`   | cgo smoke tests, render-state walking, title/pwd getters            |
| `internal/pangoextra` | cgo smoke tests for the Pango/Cairo font-options wrapper           |
| `internal/pty`       | PTY spawn → vt_write → render-state pipeline (full backend, no GTK) |
| `internal/osc`       | OSC 9 / 777 streaming parser, split reads, ST terminator, truncation |
| `internal/ipc`       | JSON-RPC roundtrip, error envelope, close behaviour                  |

The cgo tests link against `build/out/lib/libghostty-vt.a`, so they require a successful `make libghostty` first.

## Documentation site

Docs are written in Markdown under `docs/`, built with mkdocs-material via `uv`:

```bash
make docs               # build static site under site-build/
make docs-serve         # live-reload server at http://127.0.0.1:7070
```

`uv sync --group docs` is invoked automatically by both targets — no global Python install needed beyond the `uv` binary.

## Bumping the pinned Ghostty SHA

The libghostty-vt API is documented as unstable. Bumps land in their own commit:

1. Edit `build/build.sh` and update `GHOSTTY_SHA`.
2. `make clean && make libghostty`.
3. Fix any build/test breakage in `internal/ghostty`. The C symbols are listed in `src/lib_vt.zig` of the Ghostty source.
4. Run `go test ./internal/ghostty/...` and `go test ./internal/pty/...` to validate the round-trip.
5. Commit with a SHA + date in the message.

## Code conventions

The full set is in `CLAUDE.md` at the repo root. Highlights:

- Concrete types until duplication forces an interface — no premature `Manager` / `Coordinator` abstractions.
- Errors are returned, not logged-and-swallowed. Log at the boundary that handles them.
- Default to no comments. Add one when the *why* is non-obvious.
- All UI calls happen on the main thread; goroutines marshal via `glib.IdleAdd`. See [Architecture](../reference/architecture.md#threading-contract).
- cgo is allowed only in `internal/ghostty` (libghostty-vt embedding) and `internal/pangoextra` (gotk4 binding workaround for Pango/Cairo font options). Every other package is pure Go.
