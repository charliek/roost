# Roost render performance harness (`tools/perf/`)

Measures the **cost** of the Iced UI's render path — not correctness.
`tools/perf/` is a sibling of `tools/roosttest/`, `tools/screenshot/`, and
`tools/input/` (see [`../README.md`](../README.md)), not a fourth tier of
that ladder: those three verify *behavior*; this measures *how much CPU
work it took*.

Two readouts, backed by the counters `crates/roost-iced/src/perf.rs`
instruments:

| Readout | What it reads | Needs a running UI? |
|---|---|---|
| `cargo test -p roost-iced --release -- --ignored --nocapture` | Per-tab `TabRenderStats` (`refresh_calls`, `refresh_nanos`, `rows_rebuilt`, `cells_walked`) | No — an in-crate `#[ignore]`d test |
| `tools/perf/render-stats.sh <target>` (→ `roostctl render-stats`) | The process-global aggregate, same fields plus `draw_calls` / `draw_nanos` / `fill_text_calls` | Yes |

## The in-crate test: `cargo test -p roost-iced --release -- --ignored --nocapture`

`roost-iced` has no `[lib]` target
(`crates/roost-iced/Cargo.toml`), so nothing outside the crate can import
`TerminalTab` — an external bench binary isn't an option. The harness is
instead an `#[ignore]`d test,
`crates/roost-iced/src/app/perf_bench.rs::refresh_snapshot_perf_harness`,
reusing the same `attach_test_terminal` fixture the rest of the app's
unit tests use. It is genuinely `#[ignore]`d: a normal `cargo test -p
roost-iced` (what CI runs) never executes it. Always run it `--release`
— debug-profile timings aren't representative and aren't comparable
across runs.

It runs three workloads, N=200 refreshes each, and prints one block of
`key   value` lines per workload. **The format is stable and greppable
on purpose** — a before/after comparison (see below) diffs two runs of
this output.

### The three workloads

- **W1 — pointer-motion storm.** 200 refreshes with **no terminal
  mutation** between them. This is the shape
  `crates/roost-iced/src/app/interactions.rs:1341` produces:
  `refresh_snapshot` runs on every single mouse-motion event over a
  terminal, even though nothing in the grid changed. **This is the
  headline case** — today it rebuilds the entire grid for zero-content
  motion.
- **W2 — in-place TUI redraw.** 200 iterations, each writing to a couple
  of fixed rows via absolute cursor positioning (`\x1b[{row};1H...`) so
  the viewport never scrolls, then refreshing. The vim/htop shape.
- **W3 — scrolling stream (CONTROL).** 200 iterations of plain
  `line\r\n` output that scrolls the viewport, then refreshing.
  **This workload is expected to show NO improvement, ever.** libghostty
  full-rebuilds its render state whenever the viewport's scroll pin
  changes (`third_party/ghostty/src/src/terminal/render.zig:299-302`),
  so no amount of dirty-tracking on our side changes what it hands back.
  It's in the harness precisely so a before/after table has a control
  that stays flat — **a flat W3 is a PASS**, not a disappointment. Do not
  "fix" it.

### What it can't measure

This harness only exercises `refresh_snapshot` — the snapshot rebuild
that walks libghostty's render state. It cannot produce the `draw_*` /
`fill_text_calls` counters: `TerminalWidget::draw` needs a live iced
`Renderer`, which only the windowing system hands it, and no unit test
constructs one. For those, use `render-stats.sh` against a running app.

### Before/after comparison

A single run's numbers mean little on their own; what matters is the
delta between two runs of the *same machine, same moment* — CPU
frequency scaling, thermal throttling, and background load all move the
absolute numbers around run to run. Compare two runs taken back to back
(e.g. via a `git worktree` checking out two commits side by side, so
each build is fresh and neither run waits on the other's cache) rather
than trusting a number captured hours or days apart.

```bash
cargo test -p roost-iced --release -- --ignored --nocapture > before.txt
# ...checkout the candidate change...
cargo test -p roost-iced --release -- --ignored --nocapture > after.txt
diff before.txt after.txt
```

Read `rows_per_refresh` first — it's the direct measure of "how much of
the grid did this refresh actually touch." W1 today reports the full 32
rows/refresh; the point of dirty tracking is driving that toward 0
without moving W3.

## The running-app readout: `tools/perf/render-stats.sh`

```bash
tools/perf/render-stats.sh iced           # interactive: prompt before reading
tools/perf/render-stats.sh iced 10        # reset, sleep 10s, read
tools/perf/render-stats.sh mac
tools/perf/render-stats.sh gtk            # GTK has no instrumentation yet — reports all zeros
```

Resets the counters, waits for you to exercise the running UI (either
interactively or for a fixed duration), then prints `roostctl
render-stats`'s delta since the reset. `--target` follows the rest of
`tools/`: `mac|gtk|iced` (see [`../screenshot/README.md`](../screenshot/README.md)
for the per-target launch/socket details this script reuses via
`../screenshot/lib.sh`).

This is the *only* way to read `draw_calls` / `draw_nanos` /
`fill_text_calls` — they require a live iced `Renderer`, which only a
running UI has. `refresh_*` / `rows_rebuilt` / `cells_walked` are also
available here (same fields the in-crate test prints), aggregated across
every tab in the process rather than per-tab.

## Two traps to know about before trusting a number

- **The locked-Mac caveat.** External/presented frame rate is
  meaningless on a locked or occluded Mac — macOS throttles
  presentation regardless of how much CPU work the app is doing, so a
  wall-clock or FPS-style measurement taken against a locked/backgrounded
  window tells you about the OS's compositor policy, not about Roost.
  This is exactly why this harness measures CPU-side spans
  (`refresh_nanos`, `draw_nanos`) and deterministic counters
  (`rows_rebuilt`, `cells_walked`, `draw_calls`) instead of frame rate.
  There is no workaround for this — presented-frame timing on a
  locked/occluded window is not a signal this harness can produce, so it
  doesn't try.
- **The screenshot trap.** `iced::window::screenshot` re-renders the
  window, so `roostctl screenshot` and everything in
  [`../screenshot/`](../screenshot/README.md) inflate `draw_calls` /
  `draw_nanos` / `fill_text_calls` just by having run. If a screenshot
  happens inside your measurement window, either read the counters
  *before* taking it, or `roostctl render-stats --reset` (or
  `render-stats.sh`'s reset step) *after* it, before the window you
  actually care about.

See `crates/roost-iced/src/perf.rs`'s module doc for the underlying
counters, and [`docs/reference/cli.md`](../../docs/reference/cli.md) /
[`docs/reference/ipc.md`](../../docs/reference/ipc.md) for the `roostctl
render-stats` / `app.render_stats` wire contract.
