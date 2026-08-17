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
//! `fill_text_calls` counts glyph draws the pass emitted: one per
//! `fill_text` **plus** one per sprite-rendered cell, because a sprite
//! *replaces* a glyph draw. GTK counts its sprite draws the same way, so
//! this field is comparable across the two UIs.
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

use std::time::Duration;

/// `RenderStats` + `TabRenderStats` + the atomic aggregate machinery moved
/// to `roost-ui-model::render_stats` once GTK needed the identical shape
/// (plan 018 D4); re-exported here so every call site in this crate keeps
/// compiling unchanged.
pub use roost_ui_model::render_stats::{RenderStats, TabRenderStats};

use roost_ui_model::render_stats::RenderStatsAggregate;

/// This UI's process-global aggregate. GTK holds its own in
/// `roost-linux::perf`; the two never mix.
static AGGREGATE: RenderStatsAggregate = RenderStatsAggregate::new();

/// Read the global aggregate. Backs the `app.render_stats` IPC op.
pub fn snapshot() -> RenderStats {
    AGGREGATE.snapshot()
}

/// Zero the global aggregate. Useful before an operation known to skew it
/// (see the screenshot trap above) so the next read is uncontaminated.
/// Exposed over IPC as `app.render_stats` with `reset: true`.
pub fn reset() {
    AGGREGATE.reset();
}

/// Fold one `refresh_snapshot` call into the global aggregate.
pub(crate) fn record_refresh(elapsed: Duration, rows_rebuilt: u64, cells_walked: u64) {
    AGGREGATE.record_refresh(elapsed, rows_rebuilt, cells_walked);
}

/// Fold one `TerminalWidget::draw` call into the global aggregate.
pub(crate) fn record_draw(elapsed: Duration, fill_text_calls: u64) {
    AGGREGATE.record_draw(elapsed, fill_text_calls);
}

/// Fold one `App::view()` call into the global aggregate.
pub(crate) fn record_view(elapsed: Duration) {
    AGGREGATE.record_view(elapsed);
}

/// Fold one `chrome::elide_to_width` call into the global aggregate.
pub(crate) fn record_elide(elapsed: Duration) {
    AGGREGATE.record_elide(elapsed);
}
