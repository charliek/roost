//! Shared render-performance counter shapes and machinery.
//! `RenderStats` is the process-global aggregate snapshot;
//! `TabRenderStats` is the per-tab non-atomic twin;
//! [`RenderStatsAggregate`] is the atomic accumulator behind the
//! snapshot. Moved here from `roost-iced::perf` once GTK needed the
//! identical shape (CLAUDE.md's "duplication forces an interface"
//! threshold — second consumer, identical shape). Each UI holds its own
//! `static AGGREGATE: RenderStatsAggregate` plus thin free-fn shims and
//! its own module doc; the counter machinery itself lives here.

use std::sync::atomic::{AtomicU64, Ordering};
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
    pub view_calls: u64,
    pub view_nanos: u64,
    pub elide_calls: u64,
    pub elide_nanos: u64,
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

/// The process-global counter aggregate every tab folds its work into.
/// Const-constructible so each UI can hold it in a plain
/// `static AGGREGATE: RenderStatsAggregate = RenderStatsAggregate::new();`
/// without a lazy-init wrapper.
///
/// All operations are `Ordering::Relaxed`: these are monotonic
/// diagnostic counters read out of band by an IPC op, never a
/// synchronization edge between threads. A read that races a record can
/// see one field folded and another not; that skew is smaller than the
/// sampling noise the counters already carry.
#[derive(Debug, Default)]
pub struct RenderStatsAggregate {
    refresh_calls: AtomicU64,
    refresh_nanos: AtomicU64,
    rows_rebuilt: AtomicU64,
    cells_walked: AtomicU64,
    draw_calls: AtomicU64,
    draw_nanos: AtomicU64,
    fill_text_calls: AtomicU64,
    view_calls: AtomicU64,
    view_nanos: AtomicU64,
    elide_calls: AtomicU64,
    elide_nanos: AtomicU64,
}

impl RenderStatsAggregate {
    pub const fn new() -> Self {
        Self {
            refresh_calls: AtomicU64::new(0),
            refresh_nanos: AtomicU64::new(0),
            rows_rebuilt: AtomicU64::new(0),
            cells_walked: AtomicU64::new(0),
            draw_calls: AtomicU64::new(0),
            draw_nanos: AtomicU64::new(0),
            fill_text_calls: AtomicU64::new(0),
            view_calls: AtomicU64::new(0),
            view_nanos: AtomicU64::new(0),
            elide_calls: AtomicU64::new(0),
            elide_nanos: AtomicU64::new(0),
        }
    }

    /// Read every counter. Backs the `app.render_stats` IPC op.
    pub fn snapshot(&self) -> RenderStats {
        RenderStats {
            refresh_calls: self.refresh_calls.load(Ordering::Relaxed),
            refresh_nanos: self.refresh_nanos.load(Ordering::Relaxed),
            rows_rebuilt: self.rows_rebuilt.load(Ordering::Relaxed),
            cells_walked: self.cells_walked.load(Ordering::Relaxed),
            draw_calls: self.draw_calls.load(Ordering::Relaxed),
            draw_nanos: self.draw_nanos.load(Ordering::Relaxed),
            fill_text_calls: self.fill_text_calls.load(Ordering::Relaxed),
            view_calls: self.view_calls.load(Ordering::Relaxed),
            view_nanos: self.view_nanos.load(Ordering::Relaxed),
            elide_calls: self.elide_calls.load(Ordering::Relaxed),
            elide_nanos: self.elide_nanos.load(Ordering::Relaxed),
        }
    }

    /// Zero every counter. Useful before an operation known to skew them
    /// so the next read is uncontaminated.
    pub fn reset(&self) {
        self.refresh_calls.store(0, Ordering::Relaxed);
        self.refresh_nanos.store(0, Ordering::Relaxed);
        self.rows_rebuilt.store(0, Ordering::Relaxed);
        self.cells_walked.store(0, Ordering::Relaxed);
        self.draw_calls.store(0, Ordering::Relaxed);
        self.draw_nanos.store(0, Ordering::Relaxed);
        self.fill_text_calls.store(0, Ordering::Relaxed);
        self.view_calls.store(0, Ordering::Relaxed);
        self.view_nanos.store(0, Ordering::Relaxed);
        self.elide_calls.store(0, Ordering::Relaxed);
        self.elide_nanos.store(0, Ordering::Relaxed);
    }

    /// Fold one refresh (walk / cache-rebuild) phase into the aggregate.
    pub fn record_refresh(&self, elapsed: Duration, rows_rebuilt: u64, cells_walked: u64) {
        self.refresh_calls.fetch_add(1, Ordering::Relaxed);
        self.refresh_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
        self.rows_rebuilt.fetch_add(rows_rebuilt, Ordering::Relaxed);
        self.cells_walked.fetch_add(cells_walked, Ordering::Relaxed);
    }

    /// Fold one draw phase into the aggregate.
    pub fn record_draw(&self, elapsed: Duration, fill_text_calls: u64) {
        self.draw_calls.fetch_add(1, Ordering::Relaxed);
        self.draw_nanos
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
        self.fill_text_calls
            .fetch_add(fill_text_calls, Ordering::Relaxed);
    }

    /// Fold one `App::view()` call into the aggregate.
    pub fn record_view(&self, elapsed: Duration) {
        Self::fold(&self.view_calls, &self.view_nanos, elapsed);
    }

    /// Fold one `elide_to_width` call into the aggregate.
    pub fn record_elide(&self, elapsed: Duration) {
        Self::fold(&self.elide_calls, &self.elide_nanos, elapsed);
    }

    fn fold(calls: &AtomicU64, nanos: &AtomicU64, elapsed: Duration) {
        calls.fetch_add(1, Ordering::Relaxed);
        nanos.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }
}
