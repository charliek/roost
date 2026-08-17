# Roost render performance harness (`tools/perf/`)

Measures the **cost** of the Iced UI's render path — not correctness.
`tools/perf/` is a sibling of `tools/roosttest/`, `tools/screenshot/`, and
`tools/input/` (see [`../README.md`](../README.md)), not a fourth tier of
that ladder: those three verify *behavior*; this measures *how much CPU
work it took*.

Three readouts, backed by the counters `crates/roost-iced/src/perf.rs`
instruments:

| Readout | What it reads | Needs a running UI? |
|---|---|---|
| `cargo test -p roost-iced --release -- --ignored --nocapture` | Per-tab `TabRenderStats` (`refresh_calls`, `refresh_nanos`, `rows_rebuilt`, `cells_walked`) | No — an in-crate `#[ignore]`d test |
| `tools/perf/render-stats.sh <target>` (→ `roostctl render-stats`) | The process-global aggregate, same fields plus `draw_calls` / `draw_nanos` / `fill_text_calls` | Yes |
| `tools/perf/echo-latency.py` | `view_calls` / `view_nanos` / `elide_calls` / `elide_nanos` — the `App::view()` rebuild and its `chrome::elide_to_width` tab-pill eliding (plan 029, the F1 typing-latency regression) | Yes |

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
  **This workload is the control: expect little or no gain in
  rows-rebuilt, ever.** libghostty full-rebuilds its render state
  whenever the viewport's scroll pin changes
  (`third_party/ghostty/src/src/terminal/render.zig:299-302`), so no
  amount of dirty-tracking on our side reduces the rows it hands back —
  E3 measured 32.00 -> 27.50 rows/refresh here, against 32.00 -> 0.16
  for W1.

  Its *timing* can still improve, and did: 2.0x (116,609 -> 57,473
  ns/refresh) in the E3 measurement, because removing the dense
  `vec![vec![String::new(); cols]; rows]` allocation made even a full
  rebuild cheaper. So judge W3 on **rows_per_refresh staying high**, not
  on its clock. A W3 whose rows/refresh collapses means rows are being
  served from a stale cache across a viewport move — that is a bug, not
  a win.

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
the grid did this refresh actually touch." Pre-E3 every workload
reported the full 32 rows/refresh; the point of dirty tracking is
driving W1 and W2 toward 0 while W3 stays high.

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

## The typing-latency probe: `tools/perf/echo-latency.py`

```bash
tools/perf/echo-latency.py                                       # attach to the running dev-profile iced UI
tools/perf/echo-latency.py --socket /path/to/roost.sock           # attach elsewhere
tools/perf/echo-latency.py --launch target/debug/roost-iced       # quit + launch + probe a specific binary
tools/perf/echo-latency.py --launch target/release/roost-iced     # same, release profile
```

Python 3 stdlib only, no build/run of its own. Opens a scratch `/bin/cat`
tab titled `echo-latency-probe`, lets it settle for 2s, resets
`app.render_stats`, drives ~240 single-character `tab.write` echoes at
~30/s, reads `app.render_stats` again, and prints one JSON line:

```json
{"view_calls": 240, "view_avg_us": 163.8, "elide_calls": 0,
 "elide_avg_us_per_view": 0.0, "keystrokes": 240}
```

`view_avg_us` is `view_nanos / view_calls`, converted to microseconds —
the per-keystroke cost of `App::view()`. `elide_avg_us_per_view` is the
time `chrome::elide_to_width` spent *per `view()` call* (not per elide
call): near-zero means the memoized tab-pill cache (plan 029 C2) is
serving hits; a value close to `view_avg_us` means every keystroke is
re-shaping every pill's title, which is the F1 regression this probe
exists to catch. The scratch tab is always closed in a `finally` block,
and a `--launch`ed process is always terminated, even on error.

**Note on methodology**: `tab.write` over IPC is a `UiRequest`, which
marks the iced UI's event batch dirty and forces a `reconcile()` on
every keystroke — unlike a real winit keystroke, which does not
(`crates/roost-iced/src/engine_feed.rs`). This makes the IPC path
*stricter* than real typing (every keystroke pays the full reconcile +
view cost), which is why the guard and this probe both ride it rather
than trying to synthesize real key events.

**Caveat — absolute numbers are not portable.** They depend on the
machine, the build profile (debug vs. release; whether
`[profile.dev.package."*"]` opt-level tuning is present), and the
profile's existing tab-pill titles (a long title elides to more
candidate widths than a short one). Never compare a number from this
probe against a number from a different machine, a different day, or a
cited figure in a plan doc. The signal is the **A/B delta on one
machine, one run to the next** — run it before and after a change,
same binary build flow, same existing tabs, and diff the two JSON
lines.

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
