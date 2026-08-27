//! Renderer-neutral terminal selection state.
//!
//! Endpoints are [`TrackedRef`]s — owned pins libghostty rewrites as the
//! page list scrolls, prunes, and reflows — so a selection follows its
//! *content* on both axes instead of a coordinate that the next eviction
//! or reflow would point somewhere else.
//!
//! Two consequences shape everything below:
//!
//! * A tracked ref can lose its value (the row was pruned, or the
//!   terminal was reset). Then the selection resolves to nothing:
//!   [`TerminalSelection::selected_text`] returns `None` and
//!   [`TerminalSelection::visible_spans`] is empty. It never drifts onto
//!   whatever content took the old coordinate's place.
//! * A tracked ref stays attached to the screen (primary or alternate)
//!   it was created on and keeps resolving against it even while the
//!   other screen is displayed. Painting or copying that would be
//!   nonsense, and libghostty's formatter requires endpoints on the
//!   *active* screen, so the screen is captured at selection start and
//!   every read is gated on it still matching. A selection made on the
//!   primary screen therefore disappears while an alt-screen app runs
//!   and comes back when it exits.

use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

use crate::formatter::{self, UNWRAP_SOFT_WRAPPED_LINES};
use crate::{
    ActiveScreen, CellWide, Error, Point, PointTag, RenderState, Result, RowWrap, Terminal,
    TrackedRef,
};

/// A terminal row rendered as exact grapheme text plus reversible mappings
/// between Unicode scalar indices and terminal cell columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowTextProjection {
    text: String,
    char_cells: Vec<(u16, u16)>,
    cell_chars: Vec<Option<usize>>,
}

impl RowTextProjection {
    fn from_cells<I>(cols: u16, cells: I) -> Self
    where
        I: IntoIterator<Item = (u16, String)>,
    {
        let mut projection = Self {
            text: String::new(),
            char_cells: Vec::new(),
            cell_chars: vec![None; usize::from(cols)],
        };
        let mut covered_until = 0_u16;
        let mut covered_char = None;
        for (col, cell_text) in cells {
            if col >= cols {
                continue;
            }
            for gap_col in covered_until..col {
                let char_index = projection.char_cells.len();
                projection.text.push(' ');
                projection.char_cells.push((gap_col, gap_col));
                projection.cell_chars[usize::from(gap_col)] = Some(char_index);
            }
            if cell_text.is_empty() && col < covered_until {
                projection.cell_chars[usize::from(col)] = covered_char;
                continue;
            }
            let text = if cell_text.is_empty() {
                " "
            } else {
                cell_text.as_str()
            };
            let width = UnicodeWidthStr::width(text).max(1);
            let cell_end = col
                .saturating_add(u16::try_from(width.saturating_sub(1)).unwrap_or(u16::MAX))
                .min(cols.saturating_sub(1));
            let first_char = projection.char_cells.len();
            projection.text.push_str(text);
            for _ in text.chars() {
                projection.char_cells.push((col, cell_end));
            }
            for cell in col..=cell_end {
                projection.cell_chars[usize::from(cell)] = Some(first_char);
            }
            covered_until = cell_end.saturating_add(1);
            covered_char = Some(first_char);
        }
        projection
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn char_index_at_cell(&self, col: u16) -> Option<usize> {
        self.cell_chars.get(usize::from(col)).copied().flatten()
    }

    pub fn cell_span_for_chars(&self, col0: usize, col1: usize) -> Option<(u16, u16)> {
        let start = self.char_cells.get(col0)?.0;
        let end = self.char_cells.get(col1)?.1;
        Some((start, end))
    }
}

/// A live selection: two tracked endpoints plus the screen they were
/// taken on. Holding tracked refs costs it `Copy`/`Clone`/`Eq` — the
/// refs are owned handles — so everything downstream works from
/// [`Endpoints`], the resolved coordinate pair.
#[derive(Debug)]
struct CellSelection {
    anchor: TrackedRef,
    cursor: TrackedRef,
    screen: ActiveScreen,
    committed: bool,
}

impl CellSelection {
    /// Whether the refs still belong to `terminal` *and* the screen they
    /// were taken on is the one on display. libghostty resolves a ref
    /// against its own screen regardless of what is displayed, so every
    /// read below is gated on this.
    fn is_resolvable(&self, terminal: &Terminal) -> bool {
        self.anchor.is_owned_by(terminal) && terminal.active_screen() == self.screen
    }

    /// Resolve both endpoints into screen coordinates, or `None` when
    /// the selection currently names nothing: a different screen is
    /// active, the terminal is not the one that owns the refs, or the
    /// tracked content has been discarded.
    fn resolve(&self, terminal: &Terminal) -> Option<Endpoints> {
        if !self.is_resolvable(terminal) {
            return None;
        }
        let anchor = self.anchor.point(PointTag::Screen)?;
        let cursor = self.cursor.point(PointTag::Screen)?;
        Some(Endpoints {
            anchor_col: anchor.x,
            anchor_screen_y: anchor.y,
            cursor_col: cursor.x,
            cursor_screen_y: cursor.y,
            committed: self.committed,
        })
    }

    /// [`Self::resolve`], filtered down to selections that should be
    /// painted and copied.
    fn resolve_visible(&self, terminal: &Terminal) -> Option<Endpoints> {
        self.resolve(terminal).filter(Endpoints::is_visible)
    }

    /// Whether `endpoint` currently sits inside the visible viewport.
    /// Answered from the tracked ref itself, so a pruned endpoint reads
    /// as not visible rather than as some other row.
    fn endpoint_visible(&self, endpoint: &TrackedRef, terminal: &Terminal, rows: u16) -> bool {
        self.is_resolvable(terminal)
            && endpoint
                .point(PointTag::Viewport)
                .is_some_and(|point| point.y < u32::from(rows))
    }
}

/// A selection's endpoints resolved into screen coordinates. Every
/// geometry rule (visibility, normalization, per-row column ranges)
/// works from this, so the rules are unchanged from when the endpoints
/// were stored as plain screen scalars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Endpoints {
    anchor_col: u16,
    anchor_screen_y: u32,
    cursor_col: u16,
    cursor_screen_y: u32,
    committed: bool,
}

impl Endpoints {
    fn is_visible(&self) -> bool {
        self.committed
            || self.anchor_col != self.cursor_col
            || self.anchor_screen_y != self.cursor_screen_y
    }

    fn normalized(self) -> (u16, u32, u16, u32) {
        if self.anchor_screen_y == self.cursor_screen_y {
            return (
                self.anchor_col.min(self.cursor_col),
                self.anchor_screen_y,
                self.anchor_col.max(self.cursor_col).saturating_add(1),
                self.anchor_screen_y.saturating_add(1),
            );
        }
        if self.anchor_screen_y < self.cursor_screen_y {
            return (
                self.anchor_col,
                self.anchor_screen_y,
                self.cursor_col.saturating_add(1),
                self.cursor_screen_y.saturating_add(1),
            );
        }
        (
            self.cursor_col,
            self.cursor_screen_y,
            self.anchor_col.saturating_add(1),
            self.anchor_screen_y.saturating_add(1),
        )
    }
}

/// One visible row of a normalized selection, in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSpan {
    pub row: u16,
    pub col0: u16,
    pub col1: u16,
}

/// IPC/debug snapshot of a terminal selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSnapshot {
    pub text: Option<String>,
    pub anchor_visible: bool,
    pub cursor_visible: bool,
}

/// Per-terminal selection state shared by Rust UI adapters.
///
/// Owning two tracked refs makes this a resource, not a value: it is
/// neither `Copy`/`Clone` nor comparable. UIs hold one per tab and
/// mutate it in place, which is what they already did.
#[derive(Debug, Default)]
pub struct TerminalSelection {
    active: Option<CellSelection>,
}

impl TerminalSelection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the selection with tracked pins for both viewport cells.
    ///
    /// `Ok(false)` means a cell named nothing (an out-of-range row, a
    /// terminal that is tearing down) and any stale selection was
    /// dropped; `Err` is reserved for a real libghostty failure such as
    /// OOM, which the repo's conventions say to return rather than
    /// swallow.
    fn anchor(
        &mut self,
        terminal: &Terminal,
        anchor: (u16, u16),
        cursor: (u16, u16),
        committed: bool,
    ) -> Result<bool> {
        // Clear first so a tracking failure (either coordinate, or a real
        // libghostty error) never leaves the previous selection installed.
        self.active = None;
        let (Some(anchor), Some(cursor)) = (
            track_viewport(terminal, anchor.0, anchor.1)?,
            track_viewport(terminal, cursor.0, cursor.1)?,
        ) else {
            return Ok(false);
        };
        self.active = Some(CellSelection {
            anchor,
            cursor,
            screen: terminal.active_screen(),
            committed,
        });
        Ok(true)
    }

    /// Begin an uncommitted drag at a viewport cell.
    pub fn begin(&mut self, terminal: &Terminal, col: u16, row: u16) -> Result<bool> {
        self.anchor(terminal, (col, row), (col, row), false)
    }

    /// Update a drag cursor. A failed coordinate conversion clears stale state.
    pub fn update(&mut self, terminal: &Terminal, col: u16, row: u16) -> Result<bool> {
        let Some(selection) = self.active.as_mut() else {
            return Ok(false);
        };
        if !selection.cursor.is_owned_by(terminal) {
            self.active = None;
            return Ok(false);
        }
        // A drag that started on the other screen must not drag one
        // endpoint across page lists; leave it alone until its screen is
        // back.
        if terminal.active_screen() != selection.screen {
            return Ok(false);
        }
        let point = Point::viewport(clamp_col(terminal, col), u32::from(row));
        match selection.cursor.set(terminal, point) {
            Ok(()) => Ok(true),
            Err(Error::InvalidValue) | Err(Error::NoValue) => {
                self.active = None;
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    /// Set a deliberate, committed selection from viewport endpoints.
    pub fn set(
        &mut self,
        terminal: &Terminal,
        anchor: (u16, u16),
        cursor: (u16, u16),
    ) -> Result<bool> {
        self.anchor(terminal, anchor, cursor, true)
    }

    pub fn clear(&mut self) -> bool {
        self.active.take().is_some()
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Return normalized selection rectangles currently visible in the viewport.
    pub fn visible_spans(&self, terminal: &Terminal, cols: u16, rows: u16) -> Vec<SelectionSpan> {
        let Some(endpoints) = self
            .active
            .as_ref()
            .and_then(|selection| selection.resolve_visible(terminal))
        else {
            return Vec::new();
        };
        let (start_col, start_y, end_col, end_y) = endpoints.normalized();
        let total = (end_y - start_y) as usize;
        (0..total)
            .filter_map(|offset| {
                let screen = start_y.saturating_add(offset as u32);
                let row = viewport_row(terminal, screen, rows)?;
                let (col0, col1) = column_range(offset, total, start_col, end_col, cols);
                Some(SelectionSpan { row, col0, col1 })
            })
            .collect()
    }

    pub fn snapshot(
        &self,
        terminal: &Terminal,
        render_state: &mut RenderState,
        cols: u16,
        rows: u16,
    ) -> Result<Option<SelectionSnapshot>> {
        let Some(selection) = self.active.as_ref() else {
            return Ok(None);
        };
        Ok(Some(SelectionSnapshot {
            text: self.selected_text(terminal, render_state, cols, rows)?,
            anchor_visible: selection.endpoint_visible(&selection.anchor, terminal, rows),
            cursor_visible: selection.endpoint_visible(&selection.cursor, terminal, rows),
        }))
    }

    pub fn row_text(
        terminal: &Terminal,
        render_state: &mut RenderState,
        target_row: u16,
    ) -> Result<String> {
        render_state.update(terminal)?;
        let mut line = String::new();
        render_state.walk(terminal, |row, cell| {
            if row != u32::from(target_row) {
                return;
            }
            line.push(cell.text.chars().next().unwrap_or(' '));
        })?;
        Ok(line)
    }

    /// Preserve a row's complete grapheme text while mapping regex/string
    /// indices back to terminal cells. Wide-cell tails stay attached to their
    /// leading grapheme and combining scalars are never discarded.
    pub fn row_text_projection(
        terminal: &Terminal,
        render_state: &mut RenderState,
        target_row: u16,
        cols: u16,
    ) -> Result<RowTextProjection> {
        render_state.update(terminal)?;
        let mut cells = Vec::new();
        render_state.walk(terminal, |row, cell| {
            if row == u32::from(target_row) {
                cells.push((cell.col, cell.text));
            }
        })?;
        Ok(RowTextProjection::from_cells(cols, cells))
    }

    /// Text covered by the selection, or `None` if nothing is selected.
    ///
    /// A selection entirely inside the viewport is read from the render
    /// state; anything reaching into scrollback — or any selection the
    /// walk declines — goes through libghostty's formatter, which is the
    /// only API that can see rows the viewport does not show.
    ///
    /// `None` also covers a selection whose tracked content is gone —
    /// pruned out of scrollback, or on the screen that is not currently
    /// active. Those are empty selections, not failures.
    pub fn selected_text(
        &self,
        terminal: &Terminal,
        render_state: &mut RenderState,
        cols: u16,
        rows: u16,
    ) -> Result<Option<String>> {
        let Some(selection) = self.active.as_ref() else {
            return Ok(None);
        };
        let Some(endpoints) = selection.resolve_visible(terminal) else {
            return Ok(None);
        };
        let (_, start_y, _, end_y) = endpoints.normalized();
        if end_y == start_y {
            return Ok(None);
        }
        // The viewport is a contiguous window over screen rows, so both
        // edges being visible means every row between them is too.
        let fully_visible = viewport_row(terminal, start_y, rows).is_some()
            && viewport_row(terminal, end_y.saturating_sub(1), rows).is_some();
        if fully_visible {
            if let ViewportCopy::Text(text) =
                self.viewport_text(terminal, render_state, endpoints, cols, rows)?
            {
                return Ok(text);
            }
        }

        // The tracked endpoints go straight to the formatter, which pins
        // them itself: its range is inclusive on both ends, unlike
        // `normalized`'s half-open one, and it orders reversed endpoints
        // via `Selection.order`. No column clamp is needed here — a
        // tracked ref only ever exists for a cell that was in range when
        // it was created.
        let text = formatter::selection_text(terminal, &selection.anchor, &selection.cursor)?;
        Ok(text.filter(|text| !text.is_empty()))
    }

    fn viewport_text(
        &self,
        terminal: &Terminal,
        render_state: &mut RenderState,
        endpoints: Endpoints,
        cols: u16,
        rows: u16,
    ) -> Result<ViewportCopy> {
        let (start_col, start_y, end_col, end_y) = endpoints.normalized();
        let total = (end_y - start_y) as usize;
        if total == 0 {
            return Ok(ViewportCopy::Text(None));
        }
        let visible: HashMap<u32, usize> = (0..total)
            .filter_map(|offset| {
                viewport_row(terminal, start_y.saturating_add(offset as u32), rows)
                    .map(|row| (u32::from(row), offset))
            })
            .collect();
        if visible.is_empty() {
            return Ok(ViewportCopy::Text(None));
        }

        let mut lines = vec![SelectedRow::default(); total];
        // Text of the cell just before a row's first selected column,
        // kept so a selection starting on a wide grapheme's spacer can
        // reach back and pick the grapheme up — libghostty's formatter
        // applies the same rule to its start column.
        let mut preceding = String::new();
        // Whether the selection's very last cell is the placeholder a
        // wide grapheme left behind when it did not fit and wrapped.
        let mut ends_on_spacer_head = false;
        let last_col = end_col.min(cols).saturating_sub(1);
        render_state.update(terminal)?;
        render_state.walk(terminal, |row, cell| {
            let Some(&offset) = visible.get(&row) else {
                return;
            };
            let (col0, col1) = column_range(offset, total, start_col, end_col, cols);
            if cell.col.saturating_add(1) == col0 {
                preceding.clear();
                preceding.push_str(&cell.text);
            }
            if offset == total - 1 && cell.col == last_col {
                ends_on_spacer_head = cell.wide == CellWide::SpacerHead;
            }
            if cell.col < col0 || cell.col >= col1 {
                return;
            }
            // Spacers are placeholders for a wide grapheme, not blank
            // cells: emitting anything for them would double-space every
            // wide character.
            if cell.wide.is_spacer() {
                if cell.col != col0 {
                    return;
                }
                match cell.wide {
                    // The grapheme lives one column back, inside the
                    // selection's intent even though its cell is not.
                    CellWide::SpacerTail if !preceding.is_empty() => {
                        lines[offset].push_text(&preceding)
                    }
                    // A grapheme that did not fit wrapped to the next
                    // row, leaving nothing here to select.
                    CellWide::SpacerHead => lines[offset].skipped = true,
                    _ => {}
                }
                return;
            }
            if cell.text.is_empty() {
                lines[offset].pending_blanks += 1;
            } else {
                lines[offset].push_text(&cell.text);
            }
        })?;

        // When unwrapping, a selection ending on a spacer head reaches
        // into the row BELOW for the grapheme that wrapped — and how far
        // it may reach is decided in page coordinates the viewport walk
        // cannot see (`PageFormatter.formatWithState`, which declines the
        // reach at a page boundary). Rather than guess, hand these back
        // to the formatter, which is authoritative by construction.
        if UNWRAP_SOFT_WRAPPED_LINES && ends_on_spacer_head {
            return Ok(ViewportCopy::Unsupported);
        }

        let mut wraps = vec![RowWrap::default(); total];
        if UNWRAP_SOFT_WRAPPED_LINES {
            // Without the wrap flags the walk would mark every row "not
            // wrapped" and emit per-row text while the formatter joins,
            // so the same selection would copy differently depending on
            // scroll position. Defer rather than diverge — and defer
            // rather than fail the copy outright, since the formatter
            // can still answer.
            let Ok(by_viewport_row) = render_state.row_wraps(terminal) else {
                return Ok(ViewportCopy::Unsupported);
            };
            for (&row, &offset) in &visible {
                wraps[offset] = by_viewport_row
                    .get(row as usize)
                    .copied()
                    .unwrap_or_default();
            }
        }

        // Same shape as libghostty's formatter so the two paths cannot
        // disagree. Rows with no text at all are held back as pending
        // newlines and only flushed once a later row has text, which
        // preserves leading and interior blanks while dropping trailing
        // ones. A row whose only text is spaces counts as having text —
        // it trims to nothing but still ends a line. A soft-wrapped row
        // contributes no newline at all when unwrapping, and its trailing
        // blank cells carry into the continuation row instead of being
        // dropped, so the rejoined line keeps its interior spacing.
        let mut out = String::new();
        let mut pending_newlines = 0_usize;
        let mut carried_blanks = 0_usize;
        for (offset, line) in lines.iter().enumerate() {
            if line.skipped {
                continue;
            }
            if !line.has_text {
                pending_newlines += 1;
                continue;
            }
            for _ in 0..pending_newlines {
                out.push('\n');
            }
            pending_newlines = usize::from(!(UNWRAP_SOFT_WRAPPED_LINES && wraps[offset].wrap));
            if !(UNWRAP_SOFT_WRAPPED_LINES && wraps[offset].wrap_continuation) {
                carried_blanks = 0;
            }
            for _ in 0..carried_blanks + line.leading_blanks {
                out.push(' ');
            }
            out.push_str(&line.text);
            carried_blanks = line.pending_blanks;
        }
        // Only 0x20 is trimmed, and only at the end of a line — which
        // when unwrapping means the end of the JOINED line, so a wrapped
        // row's trailing spaces are interior and survive.
        let out = formatter::trim_trailing_spaces(&out);
        Ok(ViewportCopy::Text((!out.is_empty()).then_some(out)))
    }
}

/// What the viewport fast path came back with.
enum ViewportCopy {
    /// The selection's text (`None` once every row turned out blank).
    Text(Option<String>),
    /// The walk cannot mirror the formatter for this selection, so the
    /// caller has to use the formatter itself.
    Unsupported,
}

/// One row of a selection while it is being assembled from the viewport.
#[derive(Debug, Default, Clone)]
struct SelectedRow {
    text: String,
    /// True once a cell with actual content lands in the row — a space
    /// counts, an empty cell does not.
    has_text: bool,
    /// The row starts on a wrapped grapheme's placeholder, so it
    /// contributes nothing at all — not even a line break.
    skipped: bool,
    /// Textless cells seen before the row's first text cell. Held apart
    /// from `text` because the formatter emits them only after whatever
    /// carried over from a soft-wrapped predecessor.
    leading_blanks: usize,
    /// Textless cells seen since the row's last text cell. Dropped at the
    /// end of a line; carried into the next row when that row continues
    /// this one's soft wrap.
    pending_blanks: usize,
}

impl SelectedRow {
    /// Append a cell's text, materializing any textless cells that
    /// turned out to be interior rather than trailing.
    fn push_text(&mut self, text: &str) {
        if self.has_text {
            for _ in 0..self.pending_blanks {
                self.text.push(' ');
            }
        } else {
            self.leading_blanks = self.pending_blanks;
        }
        self.pending_blanks = 0;
        self.text.push_str(text);
        self.has_text = true;
    }
}

/// Track a viewport cell, folding "libghostty says there is no such
/// cell" into `Ok(None)` and leaving genuine failures (OOM) as `Err`.
fn track_viewport(terminal: &Terminal, col: u16, row: u16) -> Result<Option<TrackedRef>> {
    match terminal.track(Point::viewport(clamp_col(terminal, col), u32::from(row))) {
        Ok(tracked) => Ok(Some(tracked)),
        Err(Error::InvalidValue) | Err(Error::NoValue) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Pull a column into the grid. A column equal to `cols` has no cell to
/// track and libghostty would reject the whole endpoint, but `set` takes
/// IPC-supplied coordinates without validating them and the old
/// screen-coordinate code accepted them by clamping at copy time. Clamp
/// at track time instead so the stored endpoint is real.
fn clamp_col(terminal: &Terminal, col: u16) -> u16 {
    match terminal.cols() {
        Some(cols) if cols > 0 => col.min(cols - 1),
        _ => col,
    }
}

fn viewport_row(terminal: &Terminal, screen_y: u32, rows: u16) -> Option<u16> {
    let point = terminal.convert_point(Point::screen(0, screen_y), PointTag::Viewport)?;
    (point.y < u32::from(rows)).then_some(point.y as u16)
}

fn column_range(
    offset: usize,
    total: usize,
    start_col: u16,
    end_col: u16,
    cols: u16,
) -> (u16, u16) {
    if total == 1 {
        (start_col, end_col)
    } else if offset == 0 {
        (start_col, cols)
    } else if offset == total - 1 {
        (0, end_col)
    } else {
        (0, cols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_single_cell_is_visible() {
        let endpoints = Endpoints {
            anchor_col: 3,
            anchor_screen_y: 4,
            cursor_col: 3,
            cursor_screen_y: 4,
            committed: true,
        };
        assert!(endpoints.is_visible());
        assert_eq!(endpoints.normalized(), (3, 4, 4, 5));
    }

    #[test]
    fn uncommitted_single_cell_is_hidden() {
        let endpoints = Endpoints {
            anchor_col: 3,
            anchor_screen_y: 4,
            cursor_col: 3,
            cursor_screen_y: 4,
            committed: false,
        };
        assert!(!endpoints.is_visible());
    }

    #[test]
    fn multi_row_ranges_fill_interior_rows() {
        assert_eq!(column_range(0, 3, 4, 8, 80), (4, 80));
        assert_eq!(column_range(1, 3, 4, 8, 80), (0, 80));
        assert_eq!(column_range(2, 3, 4, 8, 80), (0, 8));
    }

    #[test]
    fn row_projection_preserves_combining_text_and_maps_wide_tails() {
        let projection = RowTextProjection::from_cells(
            5,
            [
                (0, "a\u{301}".into()),
                (1, "界".into()),
                (2, String::new()),
                (3, "x".into()),
                (4, String::new()),
            ],
        );
        assert_eq!(projection.text(), "a\u{301}界x ");
        assert_eq!(projection.char_index_at_cell(0), Some(0));
        assert_eq!(projection.char_index_at_cell(2), Some(2));
        assert_eq!(projection.cell_span_for_chars(0, 3), Some((0, 3)));

        let sparse = RowTextProjection::from_cells(4, [(0, "a".into()), (3, "b".into())]);
        assert_eq!(sparse.text(), "a  b");
        assert_eq!(sparse.char_index_at_cell(2), Some(2));
    }
}
