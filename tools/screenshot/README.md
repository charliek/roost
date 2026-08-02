# Roost screenshot harness (`tools/screenshot/`)

The **visual** layer: screenshot-driven smoke testing for all three Roost UIs,
driven entirely through `roostctl`, plus `pngtool.py` to inspect the
captures with no image libraries. Use it to verify what IPC can't *see* —
pill-dot/badge colors, theme rendering, which tab is on screen, reflow —
and to look at the result without an OS screen-capture permission.

Two halves:
- **Capture + scenarios** (`lib.sh`/`launch.sh`/`quit.sh`/`smoke.sh`,
  bash + `roostctl`) — launch a UI, walk a scenario, write labeled PNGs.
- **Inspection** (`pngtool.py`, stdlib Python) — `info` / `pixel` /
  `textscan` / `findcolor` / `crop` a PNG for programmatic assertions.
  Cross-platform, so the Linux input harness uses it too
  ([`../input/linux/`](../input/linux/README.md)).

See [`../README.md`](../README.md) for how this fits the three test layers.

## Why one harness for three UIs

The Swift (Mac), gtk4-rs, and Iced UIs speak the **same** workspace and
newline-delimited JSON IPC contract, so
the test driver is a single `roostctl` parameterized by
`--target {mac,gtk,iced}`. Only two things differ per UI, and `lib.sh`
isolates them:

| Concern | Mac | GTK | Iced |
|---|---|---|---|
| Launch | `open mac/build/Roost.app` | `target/debug/roost` | `target/debug/roost-iced` |
| Quit | AppleScript | `SIGTERM` identified pid | `SIGTERM` identified pid |
| Socket on macOS | `~/Library/Caches/Roost/roost.sock` | `~/Library/Caches/Roost-gtk/roost.sock` | `~/Library/Caches/Roost-iced/roost.sock` |
| Socket on Linux | n/a | `$XDG_RUNTIME_DIR/roost/roost.sock` | `$XDG_RUNTIME_DIR/roost-iced/roost.sock` |

Both Rust binaries run on macOS, so all three UIs can be driven side by
side there; their profiles keep sockets, locks, logs, and state distinct.

## Quick start

```bash
# Launch a UI (idempotent — no-op if already running)
tools/screenshot/launch.sh mac        # or: gtk / iced

# Run the full smoke scenario; writes PNGs + manifest.md to an outdir
tools/screenshot/smoke.sh mac /tmp/ut-mac
tools/screenshot/smoke.sh gtk /tmp/ut-gtk
tools/screenshot/smoke.sh iced /tmp/ut-iced

# Hermetic same-fixture comparison. Defaults to mac+gtk+iced on macOS and
# gtk+iced on Linux; every environment gets a unique provenance directory.
make visual-parity
python3 tools/screenshot/parity.py --out target/visual-parity --targets iced

# Quit cleanly (exercises fsync-on-exit; next launch restores the layout)
tools/screenshot/quit.sh mac
```

`smoke.sh` is self-contained: it creates a throwaway `uitest` project +
two tabs, walks the scenario, and cascade-closes the project at the end,
so it doesn't depend on or disturb your existing projects.

**Warning:** `parity.py` passes `--roost-fresh`, which force-quits any running
instance of each requested target before launching a throwaway state/config.
Save work in live Roost sessions before running it. The disposable fixture never
reads or writes developer state, but closing the existing UI is destructive.
Each target receives exactly one `Parity Project` with four fixed lifecycle tabs,
one inactive notification, a visible 220pt sidebar, and five deterministic
palette states where the product API supports them. It writes `shell.png`,
`palette.png` (root commands), `palette-query.png` (filtered commands),
`palette-agents.png`, `palette-notifications.png`, `palette-provider.png`
(including a disabled provider), and schema-versioned `measurements.json`, then
aggregates current-run documents into `manifest.md`. Schema 2 requires and links
all five palette variants; readers must reject incompatible schemas rather than
silently interpreting older documents.
Artifacts are keyed by target, OS, display backend, renderer, scale, commit,
and run ID so X11/Wayland or wgpu/tiny-skia output cannot overwrite each other.
PNG hashes are provenance, not golden assertions. The captures and their basic
geometry/color measurements are reusable after the POC; visual inspection is
the acceptance gate while Iced converges, and focused product tests protect the
behavior afterward. This is an opt-in local/shed review tool, not a dedicated
long-running CI parity suite. Cursor shape is seeded steady; elapsed agent times
remain dynamic and are explicitly excluded from visual comparison.

Local runs rebuild every requested target before capture and record both dirty
source state and the launched executable's path/SHA-256. In the Linux shed,
build with `tools/shed/shed-test.sh --build-only`, point `ROOST_GTK_BIN` or
`ROOST_ICED_BIN` at the shed-local `~/rt/debug` executable, and pass
`--no-build`; this prevents Linux outputs from overwriting macOS artifacts on
the shared mount.

AppKit's product screenshot renders the main window content view but not its
child `NSPanel`, so the Mac document records palette capture as unavailable and
does not write misleading palette images. Shell captures compare all three
targets; the five palette variants compare GTK and Iced. A future AppKit
compositor for child panels can remove that declared capability gap.

## How verification works

The harness splits checks two ways:

1. **Mechanical assertions** the script makes itself — agent-state
   strings (via `tab list`), the claude-hook lifecycle transitions, and
   the project cascade-close. A failure exits non-zero with
   `ASSERT FAILED: …`.
2. **Visual expectations** that need eyes — pill-dot colors, sidebar
   rollup stripe, notification badges, which tab is on screen. Each
   screenshot is paired with a one-line expectation in
   `<outdir>/manifest.md`. Read the manifest and inspect the matching
   PNG (an agent reads them directly; a human just opens them).

Screenshots are byte-comparable: if an action that *should* change the
view produces a PNG identical to the prior one, the UI didn't react.
That's exactly how the `roostctl tab focus` Mac regression was
caught — `03-focus-clears.png` calls it out explicitly.

## Scenario steps (`smoke.sh`)

| Shot | Drives | Expect |
|---|---|---|
| `01-states`        | A=running, B=needs_input | blue + amber dots; amber rollup stripe |
| `02-notify`        | notify the inactive tab  | amber dot + blue badge on B; project badge |
| `03-focus-clears`  | focus B                  | view switches to B; badge clears |
| `04-hook-idle`     | claude-hook lifecycle    | A gray (idle) |
| `05-cascade-closed`| close both tabs          | `uitest` project gone |

## Building blocks (`lib.sh`)

Source `lib.sh` and call `ut_init <mac|gtk|iced>` to write your own scenario:

- `rc …` — run `roostctl --target <target> …`
- `ut_launch` / `ut_quit` / `ut_alive` / `ut_wait_alive`
- `shot <outdir> <name>` — capture `<name>.png` (2x)
- `expect <outdir> <name> <text>` — append an expectation row to `manifest.md`
- `ut_reset_states <tab…>` — clear state + notification on tabs

`ut_init` resolves a freshly-built `roostctl` from `target/`. It never
uses the stale `./roost-cli` at the repo root (a pre-port binary).

## Manual CLI reference

For the underlying `roostctl` commands and the full T1–T7 checklist this
harness automates, see
[`docs/development/claude-testing.md`](../../docs/development/claude-testing.md).
