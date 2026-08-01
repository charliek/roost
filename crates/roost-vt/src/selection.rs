//! Renderer-neutral terminal selection state.
//!
//! Endpoints use libghostty screen coordinates so a selection follows its
//! rows through output and scrollback instead of drifting with the viewport.

use std::collections::HashMap;

use crate::{Point, PointTag, RenderState, Result, Terminal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellSelection {
    anchor_col: u16,
    anchor_screen_y: u32,
    cursor_col: u16,
    cursor_screen_y: u32,
    committed: bool,
}

impl CellSelection {
    fn is_visible(self) -> bool {
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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSelection {
    active: Option<CellSelection>,
}

impl TerminalSelection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin an uncommitted drag at a viewport cell.
    pub fn begin(&mut self, terminal: &Terminal, col: u16, row: u16) -> bool {
        let Some(screen_y) = screen_y(terminal, row) else {
            self.active = None;
            return false;
        };
        self.active = Some(CellSelection {
            anchor_col: col,
            anchor_screen_y: screen_y,
            cursor_col: col,
            cursor_screen_y: screen_y,
            committed: false,
        });
        true
    }

    /// Update a drag cursor. A failed coordinate conversion clears stale state.
    pub fn update(&mut self, terminal: &Terminal, col: u16, row: u16) -> bool {
        let Some(screen_y) = screen_y(terminal, row) else {
            self.active = None;
            return false;
        };
        let Some(selection) = self.active.as_mut() else {
            return false;
        };
        selection.cursor_col = col;
        selection.cursor_screen_y = screen_y;
        true
    }

    /// Set a deliberate, committed selection from viewport endpoints.
    pub fn set(&mut self, terminal: &Terminal, anchor: (u16, u16), cursor: (u16, u16)) -> bool {
        let Some(anchor_screen_y) = screen_y(terminal, anchor.1) else {
            self.active = None;
            return false;
        };
        let Some(cursor_screen_y) = screen_y(terminal, cursor.1) else {
            self.active = None;
            return false;
        };
        self.active = Some(CellSelection {
            anchor_col: anchor.0,
            anchor_screen_y,
            cursor_col: cursor.0,
            cursor_screen_y,
            committed: true,
        });
        true
    }

    pub fn clear(&mut self) -> bool {
        self.active.take().is_some()
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Return normalized selection rectangles currently visible in the viewport.
    pub fn visible_spans(&self, terminal: &Terminal, cols: u16, rows: u16) -> Vec<SelectionSpan> {
        let Some(selection) = self.active.filter(|selection| selection.is_visible()) else {
            return Vec::new();
        };
        let (start_col, start_y, end_col, end_y) = selection.normalized();
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
        let Some(selection) = self.active else {
            return Ok(None);
        };
        Ok(Some(SelectionSnapshot {
            text: self.selected_text(terminal, render_state, cols, rows)?,
            anchor_visible: viewport_row(terminal, selection.anchor_screen_y, rows).is_some(),
            cursor_visible: viewport_row(terminal, selection.cursor_screen_y, rows).is_some(),
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

    pub fn selected_text(
        &self,
        terminal: &Terminal,
        render_state: &mut RenderState,
        cols: u16,
        rows: u16,
    ) -> Result<Option<String>> {
        let Some(selection) = self.active.filter(|selection| selection.is_visible()) else {
            return Ok(None);
        };
        let (start_col, start_y, end_col, end_y) = selection.normalized();
        let total = (end_y - start_y) as usize;
        if total == 0 {
            return Ok(None);
        }
        let visible: HashMap<u32, usize> = (0..total)
            .filter_map(|offset| {
                viewport_row(terminal, start_y.saturating_add(offset as u32), rows)
                    .map(|row| (u32::from(row), offset))
            })
            .collect();
        if visible.is_empty() {
            return Ok(None);
        }

        let mut lines = vec![String::new(); total];
        render_state.update(terminal)?;
        render_state.walk(terminal, |row, cell| {
            let Some(&offset) = visible.get(&row) else {
                return;
            };
            let (col0, col1) = column_range(offset, total, start_col, end_col, cols);
            if cell.col < col0 || cell.col >= col1 {
                return;
            }
            if cell.text.is_empty() {
                lines[offset].push(' ');
            } else {
                lines[offset].push_str(&cell.text);
            }
        })?;

        let mut lines: Vec<String> = lines
            .into_iter()
            .map(|line| line.trim_end().to_string())
            .collect();
        while matches!(lines.first(), Some(line) if line.is_empty()) {
            lines.remove(0);
        }
        while matches!(lines.last(), Some(line) if line.is_empty()) {
            lines.pop();
        }
        Ok((!lines.is_empty()).then(|| lines.join("\n")))
    }
}

fn screen_y(terminal: &Terminal, viewport_row: u16) -> Option<u32> {
    terminal
        .convert_point(
            Point::viewport(0, u32::from(viewport_row)),
            PointTag::Screen,
        )
        .map(|point| point.y)
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
        let selection = CellSelection {
            anchor_col: 3,
            anchor_screen_y: 4,
            cursor_col: 3,
            cursor_screen_y: 4,
            committed: true,
        };
        assert!(selection.is_visible());
        assert_eq!(selection.normalized(), (3, 4, 4, 5));
    }

    #[test]
    fn uncommitted_single_cell_is_hidden() {
        let selection = CellSelection {
            anchor_col: 3,
            anchor_screen_y: 4,
            cursor_col: 3,
            cursor_screen_y: 4,
            committed: false,
        };
        assert!(!selection.is_visible());
    }

    #[test]
    fn multi_row_ranges_fill_interior_rows() {
        assert_eq!(column_range(0, 3, 4, 8, 80), (4, 80));
        assert_eq!(column_range(1, 3, 4, 8, 80), (0, 80));
        assert_eq!(column_range(2, 3, 4, 8, 80), (0, 8));
    }
}
