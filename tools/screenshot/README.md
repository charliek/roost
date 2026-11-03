# Roost screenshot harness (`tools/screenshot/`)

The **visual** layer: screenshot-driven smoke testing for the Roost UI,
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

## What it drives

A dev `roost-iced` build (`target/debug/roost-iced`, built if missing) on
its `Roost-iced` profile, so it runs beside an installed Roost.app:

| Concern | |
|---|---|
| Launch | `target/debug/roost-iced` |
| Quit | `SIGTERM` to the pid `identify` reports |
| Socket on macOS | `~/Library/Caches/Roost-iced/roost.sock` |
| Socket on Linux | `$XDG_RUNTIME_DIR/roost-iced/roost.sock` |

**The GL fallback renderer.** Under the GLES/GL fallback (rather than
Vulkan/Metal), `roostctl screenshot` can return geometry without text
and a frozen frame (#496). Check which renderer is live from the iced
UI log's `Selected: AdapterInfo { … backend: … }` line
(`RUST_LOG=info` or above); use `tools/wayland/weston-run.sh`
(headless weston) for a capture you can trust the renderer of.

## Quick start

```bash
# Launch the UI (idempotent — no-op if already running)
tools/screenshot/launch.sh

# Run the full smoke scenario; writes PNGs + manifest.md to an outdir
tools/screenshot/smoke.sh iced /tmp/ut-iced

# Quit cleanly (exercises fsync-on-exit; next launch restores the layout)
tools/screenshot/quit.sh
```

`smoke.sh` is self-contained: it creates a throwaway `uitest` project +
two tabs, walks the scenario, and cascade-closes the project at the end,
so it doesn't depend on or disturb your existing projects.

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
That's exactly how a `roostctl tab focus` regression was caught —
`03-focus-clears.png` calls it out explicitly.

## Scenario steps (`smoke.sh`)

| Shot | Drives | Expect |
|---|---|---|
| `01-states`        | A=running, B=needs_input | blue + amber dots; amber rollup stripe |
| `02-notify`        | notify the inactive tab  | amber dot + blue badge on B; project badge |
| `03-focus-clears`  | focus B                  | view switches to B; badge clears |
| `04-hook-idle`     | claude-hook lifecycle    | A gray (idle) |
| `05-cascade-closed`| close both tabs          | `uitest` project gone |

## Building blocks (`lib.sh`)

Source `lib.sh` and call `ut_init` to write your own scenario:

- `rc …` — run `roostctl --target iced …`
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
