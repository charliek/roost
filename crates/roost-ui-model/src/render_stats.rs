//! Shared render-performance counter shapes. `RenderStats` is the
//! process-global aggregate snapshot; `TabRenderStats` is the per-tab
//! non-atomic twin. Moved here from `roost-iced::perf` once GTK needed
//! the identical shape (CLAUDE.md's "duplication forces an interface"
//! threshold — second consumer, identical shape). Each UI keeps its own
//! `static` atomics and record/snapshot/reset functions; only the plain
//! data shapes + the per-tab accumulator helper live here.

use std::time::Duration;

/// A read of a process-global counter aggregate at one instant. Every
/// field is a running total since process start (or the owning UI's
/// last reset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub refresh_calls: u64,
    pub refresh_nanos: u64,
    pub rows_rebuilt: u64,
    pub cells_walked: u64,
    pub draw_calls: u64,
    pub draw_nanos: u64,
    pub fill_text_calls: u64,
}

/// Per-tab counters, folded into a UI's global aggregate on every
/// refresh. Non-atomic and per-tab is deliberate: a UI's test suite
/// runs tests in parallel, each spawning a real PTY per test, so several
/// tests refresh concurrently — an `after - before` delta read off a
/// shared global counter would be polluted by whatever other tests are
/// doing at the same moment. Per-tab counters are what unit tests assert
/// on, and they sidestep that flake class entirely by construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TabRenderStats {
    pub refresh_calls: u64,
    pub refresh_nanos: u64,
    pub rows_rebuilt: u64,
    pub cells_walked: u64,
}

impl TabRenderStats {
    pub fn record_refresh(&mut self, elapsed: Duration, rows_rebuilt: u64, cells_walked: u64) {
        self.refresh_calls += 1;
        self.refresh_nanos += elapsed.as_nanos() as u64;
        self.rows_rebuilt += rows_rebuilt;
        self.cells_walked += cells_walked;
    }
}
