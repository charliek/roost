//! GTK render performance instrumentation. Counters and timings only —
//! the shapes and the atomic machinery live in
//! [`roost_ui_model::render_stats`]; this module is just this UI's
//! process-global instance plus thin shims. `roost-iced` has the exact
//! same shape over its own aggregate; the two never mix.
//!
//! GTK counter semantics (they differ from iced's, so read this before
//! comparing numbers across the two UIs):
//!
//! - GTK's `paint` is refresh *and* draw in one pass. The seam is
//!   `TerminalViewState::refresh_passes` (renderer-free: `update` +
//!   walk + counters, no Cairo) — `refresh_*` counts that phase,
//!   `draw_*` counts the Cairo phase that consumes its output. One
//!   `paint` therefore folds in exactly one refresh and one draw, unlike
//!   iced where a refresh (on PTY output) and a draw (on window redraw)
//!   are independently scheduled.
//! - `fill_text_calls` counts `pango_cairo::show_layout` calls **plus**
//!   sprite draws, because a sprite *replaces* a glyph draw — the number
//!   means "glyph draws the pass emitted". iced has no sprite path
//!   (roadmap E5), so this field is not apples-to-apples across UIs.
//! - `rows_rebuilt` / `cells_walked` mean the same as iced: rows visited
//!   by the walk and cells handed to the per-cell callback.
//!
//! Per-tab counters live on `TerminalViewState::render_stats` (a
//! non-atomic [`roost_ui_model::render_stats::TabRenderStats`]); the
//! aggregate here is what the `app.render_stats` IPC op reads.

use std::time::Duration;

use roost_ui_model::render_stats::{RenderStats, RenderStatsAggregate};

static AGGREGATE: RenderStatsAggregate = RenderStatsAggregate::new();

/// Read the global aggregate. Backs the `app.render_stats` IPC op.
pub(crate) fn snapshot() -> RenderStats {
    AGGREGATE.snapshot()
}

/// Zero the global aggregate. Exposed over IPC as `app.render_stats`
/// with `reset: true`.
pub(crate) fn reset() {
    AGGREGATE.reset();
}

/// Fold one `refresh_passes` call into the global aggregate.
pub(crate) fn record_refresh(elapsed: Duration, rows_rebuilt: u64, cells_walked: u64) {
    AGGREGATE.record_refresh(elapsed, rows_rebuilt, cells_walked);
}

/// Fold one `paint` Cairo phase into the global aggregate.
pub(crate) fn record_draw(elapsed: Duration, fill_text_calls: u64) {
    AGGREGATE.record_draw(elapsed, fill_text_calls);
}
