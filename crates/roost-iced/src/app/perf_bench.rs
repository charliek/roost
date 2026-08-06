//! In-crate performance harness for `TerminalTab::refresh_snapshot`.
//!
//! `roost-iced` has no `[lib]` target (`crates/roost-iced/Cargo.toml`), so
//! nothing outside the crate can import `TerminalTab` — an external
//! Criterion-style bench binary is not an option here. This lives
//! in-crate instead, `#[ignore]`d so it never runs under a normal `cargo
//! test -p roost-iced` or in CI. Invoke it explicitly:
//!
//!   cargo test -p roost-iced --release -- --ignored --nocapture
//!
//! It reuses the `attach_test_terminal` fixture the rest of the app's
//! unit tests use and reads its numbers off each tab's own
//! `TabRenderStats`, not the process-global aggregate in `crate::perf` —
//! `cargo test` runs tests concurrently and a shared counter would be
//! polluted by whatever other test is refreshing its own tab at the same
//! moment (see `crate::perf`'s module doc for the same reasoning).
//!
//! This measures `refresh_snapshot` only. The `draw_*` / `fill_text_calls`
//! counters in `crate::perf::RenderStats` need a live iced `Renderer` that
//! only the windowing system hands `TerminalWidget::draw` — no unit test
//! constructs one, and there is no per-tab equivalent of them for the same
//! reason (see `TabRenderStats`'s doc comment). Read those with `roostctl
//! render-stats` against a running app instead — see `tools/perf/`.
use super::*;

const ITERATIONS: usize = 200;

struct WorkloadResult {
    name: &'static str,
    iterations: usize,
    wall: Duration,
    stats: crate::perf::TabRenderStats,
}

impl WorkloadResult {
    /// Stable, greppable `key   value` lines — commit C7 diffs two runs of
    /// this output (this commit vs. HEAD, via a `git worktree`) to build
    /// the before/after table, so the format here must not drift.
    fn print(&self) {
        let ns_per_refresh = self
            .stats
            .refresh_nanos
            .checked_div(self.stats.refresh_calls)
            .map_or_else(|| "-".to_string(), |ns| ns.to_string());
        let rows_per_refresh = if self.stats.refresh_calls > 0 {
            self.stats.rows_rebuilt as f64 / self.stats.refresh_calls as f64
        } else {
            0.0
        };
        println!("=== {} ===", self.name);
        println!("iterations       {}", self.iterations);
        println!("wall_ns          {}", self.wall.as_nanos());
        println!("refresh_calls    {}", self.stats.refresh_calls);
        println!("refresh_nanos    {}", self.stats.refresh_nanos);
        println!("ns_per_refresh   {ns_per_refresh}");
        println!("rows_rebuilt     {}", self.stats.rows_rebuilt);
        println!("cells_walked     {}", self.stats.cells_walked);
        println!("rows_per_refresh {rows_per_refresh:.2}");
        println!();
    }
}

/// Attach a fresh test terminal, run `iterations` steps of `mutate` +
/// `refresh_snapshot`, and return the per-tab counter delta. A fresh tab
/// per workload (rather than one shared tab) keeps each workload's
/// counters at exactly its own numbers with no baseline subtraction to
/// get wrong.
fn run_workload(
    tab_id: i64,
    name: &'static str,
    iterations: usize,
    mut mutate: impl FnMut(&mut TerminalTab, usize),
) -> WorkloadResult {
    let (feed_tx, _feed_rx) = engine_feed::channel();
    let (mut tab, supervisor) = attach_test_terminal(tab_id, feed_tx);
    assert_eq!(
        tab.render_stats,
        crate::perf::TabRenderStats::default(),
        "a freshly attached tab has untouched counters"
    );

    let wall_started = Instant::now();
    for i in 0..iterations {
        mutate(&mut tab, i);
        tab.refresh_snapshot().expect("refresh_snapshot");
    }
    let wall = wall_started.elapsed();

    let stats = tab.render_stats;
    supervisor.close(tab_id);

    WorkloadResult {
        name,
        iterations,
        wall,
        stats,
    }
}

/// Runs three workloads back to back and prints each one's per-tab
/// `refresh_snapshot` counters. No numbers are asserted on or persisted
/// here — this commit only builds the harness; C7 runs it against this
/// commit and against `HEAD` back to back via a `git worktree` (a
/// same-machine, same-moment A/B) and diffs the two printed tables.
///
/// The three workloads are deliberately distinct shapes, not three sizes
/// of the same thing:
///
/// - **W1 — pointer-motion storm.** No terminal mutation between
///   refreshes at all. This is what `interactions.rs`'s pointer-motion
///   handler produces: `refresh_or_warn` (wrapping `refresh_snapshot`)
///   runs on *every* mouse-motion event over a terminal
///   (`interactions.rs:1341`), even though nothing in the grid changed.
///   This is the headline case for dirty tracking — today it rebuilds
///   the entire grid for zero-content-change motion.
/// - **W2 — in-place TUI redraw.** Each iteration writes to a couple of
///   fixed rows via absolute cursor positioning (`\x1b[{row};1H...`), so
///   the viewport never scrolls. This is the vim/htop shape: a full
///   redraw of a bounded region, not a scroll.
/// - **W3 — scrolling stream (CONTROL).** Plain `line\r\n` output that
///   scrolls the viewport, same as a chatty build log. **This workload is
///   expected to show NO improvement, ever**: libghostty full-rebuilds
///   the render state whenever the viewport's scroll pin changes
///   (`third_party/ghostty/src/src/terminal/render.zig:299-302`). It is
///   in this harness precisely so a before/after table has a control
///   that stays flat — a future reader seeing W3 unchanged should read
///   that as the harness working correctly, not as something to "fix".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf harness — run explicitly: cargo test -p roost-iced --release -- --ignored --nocapture"]
async fn refresh_snapshot_perf_harness() {
    let w1 = run_workload(9_101, "W1 pointer-motion-storm", ITERATIONS, |_tab, _i| {
        // Deliberately empty: the whole point of W1 is a refresh with no
        // preceding terminal mutation.
    });

    let w2 = run_workload(9_102, "W2 in-place-tui-redraw", ITERATIONS, |tab, i| {
        let row = 3 + (i % 2); // alternate two fixed rows; never scrolls
        tab.write_vt(format!("\x1b[{row};1Hframe {i:04}").as_bytes());
    });

    let w3 = run_workload(
        9_103,
        "W3 scrolling-stream-CONTROL",
        ITERATIONS,
        |tab, i| {
            tab.write_vt(format!("line-{i:04}\r\n").as_bytes());
        },
    );

    println!();
    w1.print();
    w2.print();
    w3.print();
}
