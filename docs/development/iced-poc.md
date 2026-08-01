# Iced POC development

The `roost-iced` binary is the isolated third Roost UI. It is a long-lived POC,
not the production Linux replacement. It consumes `roost-engine` directly and
never depends on `roost-linux`, GTK, libadwaita, Pango, or Cairo.

## Current walking skeleton

The first vertical slice is a usable terminal, not a static mock. It restores
the authoritative engine workspace, starts real engine-supervised PTYs, applies
PTY output to libghostty-vt on Iced's event thread, paints the resulting cells
and cursor with an Iced canvas, encodes native keyboard events with
`roost_vt::KeyEncoder`, and resizes the VT grid and PTY together. The common IPC
server supports identify, project/tab operations, PTY write/resize, tab list,
terminal dump, activation, resolved-cell dump, and the test-mode PTY byte ports.

Workspace UI state reconciles from a full snapshot every tick, so a slow UI
consumer recovers without replaying stale deltas. PTY broadcast lag is different:
lost terminal bytes cannot be reconstructed, so the adapter logs and displays a
specific error rather than silently pretending the render is current. All
toolkit or platform ports not implemented in this slice reply with an explicit
error; none leave an IPC caller waiting on a dropped reply.

The canvas currently covers foreground/background colors, inverse, bold,
italic, cursor shapes, grapheme cell text, clipping by the widget, and resize.
Selection, clipboard, links, mouse protocols, theme/config application,
notifications, palette behavior, in-process screenshots, and full product
styling remain the next parity slices; they are not hidden behind target-wide
test skips.

## Build, run, and test

```sh
make build-iced
make run-iced
make test-iced
make check-iced
make e2e-iced
make e2e-iced-ci
```

`make e2e-iced-ci` launches a fresh harness-owned process with an isolated
temporary `ROOST_STATE_DIR`. It never reads or writes developer session state.
Select a renderer explicitly when diagnosing backend behavior:

```sh
ICED_BACKEND=wgpu make e2e-iced-ci
ICED_BACKEND=tiny-skia make e2e-iced-ci
```

On Linux, Iced is built with both X11 and Wayland support. CI runs the functional
slice under Xvfb and headless Weston with both renderers. The shed is the local
Linux authority and keeps ELF, Cargo, and Ghostty artifacts outside the mounted
macOS output directories:

```sh
tools/shed/shed-test.sh --build-only
```

## Target and coexistence

Address this UI explicitly with:

```sh
roostctl --target iced identify
roostctl --target iced tab list
roostctl --target iced tab dump --tab <id>
```

The Iced profile uses app id `ai.stridelabs.Roost.iced` and these isolated paths:

| Host | Socket and lock | State | Log |
|---|---|---|---|
| macOS | `~/Library/Caches/Roost-iced/` | `~/Library/Application Support/Roost-iced/state.json` | `~/Library/Logs/Roost-iced/roost.log` |
| Linux | `$XDG_RUNTIME_DIR/roost-iced/` | `$XDG_DATA_HOME/roost-iced/state.json` | `$XDG_STATE_HOME/roost-iced/roost.log` |

The documented `/tmp/roost-iced-<uid>` fallback applies when Linux XDG roots
are unavailable. These paths let Swift/AppKit, GTK, and Iced run together on
macOS without sharing sockets, locks, state, or logs.

The reviewed renderer choice, acceptance matrix, future Swift ABI, and deferred
FFI decision remain in the [Iced POC plan](iced-poc-plan.md).
