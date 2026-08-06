//! Render performance instrumentation. Counters and timings only — this
//! commit changes no rendering behavior, it just measures the current one
//! so a later commit has a baseline to optimize against.
//!
//! Two scopes, deliberately:
//!
//! - [`TabRenderStats`] is a plain (non-atomic) per-`TerminalTab` struct.
//!   `cargo test -p roost-iced` runs tests in parallel and the test fixture
//!   spawns a real PTY per test, so several tests refresh concurrently; a
//!   `after - before` delta read off a shared global counter would be
//!   polluted by whatever other tests are doing at the same moment. Per-tab
//!   counters are what unit tests assert on, and they sidestep that flake
//!   class entirely by construction.
//! - [`snapshot`] reads a process-global `AtomicU64` aggregate that every
//!   tab folds its work into. This is what the `app.render_stats` IPC op
//!   reads out of a running app; no test asserts on it.
//!
//! Two traps to know about before trusting a number out of this module:
//!
//! - `iced::window::screenshot` re-renders the window, so `roostctl
//!   screenshot` (and everything in `tools/screenshot/`) inflates
//!   `draw_calls` / `draw_nanos` / `fill_text_calls` just by having run.
//!   Read the counters before taking a screenshot, or [`reset`] afterward.
//! - The `draw_*` and `fill_text_calls` counters only exist in a running
//!   app: `TerminalWidget::draw` needs a live iced `Renderer`, which unit
//!   tests don't construct. There is no per-tab equivalent of them for the
//!   same reason — see [`TabRenderStats`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static REFRESH_CALLS: AtomicU64 = AtomicU64::new(0);
static REFRESH_NANOS: AtomicU64 = AtomicU64::new(0);
static ROWS_REBUILT: AtomicU64 = AtomicU64::new(0);
static CELLS_WALKED: AtomicU64 = AtomicU64::new(0);
static DRAW_CALLS: AtomicU64 = AtomicU64::new(0);
static DRAW_NANOS: AtomicU64 = AtomicU64::new(0);
static FILL_TEXT_CALLS: AtomicU64 = AtomicU64::new(0);

/// A read of the process-global aggregate at one instant. Every field is a
/// running total since process start (or the last [`reset`]).
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

/// Per-tab counters, folded into the global aggregate on every refresh.
/// There is deliberately no per-tab equivalent of `draw_calls` /
/// `draw_nanos` / `fill_text_calls`: `TerminalWidget::draw` renders from a
/// `TerminalSnapshot` clone handed to it by iced and has no way back to the
/// `TerminalTab` that produced it, so those three counters only exist in
/// the global aggregate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TabRenderStats {
    pub refresh_calls: u64,
    pub refresh_nanos: u64,
    pub rows_rebuilt: u64,
    pub cells_walked: u64,
}

impl TabRenderStats {
    pub(crate) fn record_refresh(
        &mut self,
        elapsed: Duration,
        rows_rebuilt: u64,
        cells_walked: u64,
    ) {
        self.refresh_calls += 1;
        self.refresh_nanos += elapsed.as_nanos() as u64;
        self.rows_rebuilt += rows_rebuilt;
        self.cells_walked += cells_walked;
    }
}

/// Read the global aggregate. Backs the `app.render_stats` IPC op.
pub fn snapshot() -> RenderStats {
    RenderStats {
        refresh_calls: REFRESH_CALLS.load(Ordering::Relaxed),
        refresh_nanos: REFRESH_NANOS.load(Ordering::Relaxed),
        rows_rebuilt: ROWS_REBUILT.load(Ordering::Relaxed),
        cells_walked: CELLS_WALKED.load(Ordering::Relaxed),
        draw_calls: DRAW_CALLS.load(Ordering::Relaxed),
        draw_nanos: DRAW_NANOS.load(Ordering::Relaxed),
        fill_text_calls: FILL_TEXT_CALLS.load(Ordering::Relaxed),
    }
}

/// Zero the global aggregate. Useful before an operation known to skew it
/// (see the screenshot trap above) so the next read is uncontaminated.
/// Exposed over IPC as `app.render_stats` with `reset: true`.
pub fn reset() {
    REFRESH_CALLS.store(0, Ordering::Relaxed);
    REFRESH_NANOS.store(0, Ordering::Relaxed);
    ROWS_REBUILT.store(0, Ordering::Relaxed);
    CELLS_WALKED.store(0, Ordering::Relaxed);
    DRAW_CALLS.store(0, Ordering::Relaxed);
    DRAW_NANOS.store(0, Ordering::Relaxed);
    FILL_TEXT_CALLS.store(0, Ordering::Relaxed);
}

/// Fold one `refresh_snapshot` call into the global aggregate.
pub(crate) fn record_refresh(elapsed: Duration, rows_rebuilt: u64, cells_walked: u64) {
    REFRESH_CALLS.fetch_add(1, Ordering::Relaxed);
    REFRESH_NANOS.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    ROWS_REBUILT.fetch_add(rows_rebuilt, Ordering::Relaxed);
    CELLS_WALKED.fetch_add(cells_walked, Ordering::Relaxed);
}

/// Fold one `TerminalWidget::draw` call into the global aggregate.
pub(crate) fn record_draw(elapsed: Duration, fill_text_calls: u64) {
    DRAW_CALLS.fetch_add(1, Ordering::Relaxed);
    DRAW_NANOS.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    FILL_TEXT_CALLS.fetch_add(fill_text_calls, Ordering::Relaxed);
}
