//! Pure dump-densification: turning a row of libghostty-vt [`Cell`]s into
//! the sparse draw-ready form both roost UIs render from.
//!
//! Lives in `roost-vt` (rather than a UI crate) because it is built
//! entirely on `roost-vt` types and has no rendering-toolkit dependency —
//! any consumer that walks a terminal's cells wants the same densified
//! shape, not just the iced UI.

use crate::{Cell, ColorRgb};

/// One resolved cell. Deliberately carries no row index: its row is the
/// index of the [`RenderedRow`] that owns it. Storing the row here as well
/// would let the two disagree, which is exactly the "right cells, wrong
/// row" failure the per-row cache could otherwise hide.
#[derive(Debug, Clone)]
pub struct DrawCell {
    pub col: u16,
    pub text: String,
    pub foreground: ColorRgb,
    pub background: ColorRgb,
    pub explicit_background: bool,
    pub bold: bool,
    pub italic: bool,
    pub inverse: bool,
}

/// One viewport row's render output, shared behind an `Arc` so cloning a
/// snapshot (which `App::view` does every frame) is O(rows) refcount bumps
/// rather than O(cells) `String` clones, and so a row libghostty reports
/// undirty survives a refresh without being rebuilt.
///
/// **Overlay invariant — a `RenderedRow` holds terminal content ONLY.**
/// Selection tint, link-hover underline and the cursor are snapshot-level
/// fields drawn in separate passes after the cell loop in
/// [`TerminalWidget::draw`], and must NEVER be baked into a [`DrawCell`]'s
/// colors. A row is cached across refreshes; folding selection into a
/// cell's background would freeze the tint in the cache, which surfaces as
/// "the selection sometimes doesn't clear".
#[derive(Debug, Default)]
pub struct RenderedRow {
    /// Sparse: only the cells that draw something, ascending by column.
    pub cells: Vec<DrawCell>,
    /// The row's text, joined and `trim_end`ed — what `tab.dump` returns.
    pub text: String,
}

impl RenderedRow {
    /// Resolve one viewport row from libghostty's cells for that row.
    ///
    /// Everything this reads is a parameter — the row's vt cells, the
    /// terminal's default fg/bg pair, and the grid width. That list IS the
    /// cache key `TerminalTab::refresh_snapshot` guards on; adding a
    /// fourth input here means extending those guards (see that function's
    /// caching invariant).
    pub fn build(cells: &[Cell], defaults: (ColorRgb, ColorRgb), cols: u16) -> Self {
        let mut row = RenderedRow {
            cells: Vec::new(),
            text: String::with_capacity(usize::from(cols)),
        };
        for cell in cells {
            // libghostty yields a row's cells in ascending, gapless column
            // order, so appending is the same string the old
            // index-into-a-dense-`Vec<String>`-then-`concat` build produced
            // — including for a short row, whose missing tail contributed
            // empty strings there and contributes nothing here.
            if cell.col >= cols {
                continue;
            }
            let text = if cell.text.is_empty() {
                " "
            } else {
                cell.text.as_str()
            };
            row.text.push_str(text);
            let (foreground, background) =
                resolve_colors(cell.fg, cell.bg, defaults, cell.style.inverse);
            if text != " " || cell.bg.is_some() || cell.style.inverse {
                row.cells.push(DrawCell {
                    col: cell.col,
                    text: text.to_string(),
                    foreground,
                    background,
                    explicit_background: cell.bg.is_some() || cell.style.inverse,
                    bold: cell.style.bold,
                    italic: cell.style.italic,
                    inverse: cell.style.inverse,
                });
            }
        }
        row.text.truncate(row.text.trim_end().len());
        row
    }
}

pub fn resolve_colors(
    foreground: Option<ColorRgb>,
    background: Option<ColorRgb>,
    defaults: (ColorRgb, ColorRgb),
    inverse: bool,
) -> (ColorRgb, ColorRgb) {
    let mut foreground = foreground.unwrap_or(defaults.0);
    let mut background = background.unwrap_or(defaults.1);
    if inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    (foreground, background)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_colors_defaults_when_unset() {
        let defaults = (ColorRgb { r: 1, g: 2, b: 3 }, ColorRgb { r: 4, g: 5, b: 6 });
        let (fg, bg) = resolve_colors(None, None, defaults, false);
        assert_eq!(fg, defaults.0);
        assert_eq!(bg, defaults.1);
    }

    #[test]
    fn resolve_colors_prefers_explicit_over_defaults() {
        let explicit_fg = ColorRgb {
            r: 10,
            g: 20,
            b: 30,
        };
        let explicit_bg = ColorRgb {
            r: 40,
            g: 50,
            b: 60,
        };
        let defaults = (ColorRgb { r: 1, g: 2, b: 3 }, ColorRgb { r: 4, g: 5, b: 6 });
        let (fg, bg) = resolve_colors(Some(explicit_fg), Some(explicit_bg), defaults, false);
        assert_eq!(fg, explicit_fg);
        assert_eq!(bg, explicit_bg);
    }

    #[test]
    fn resolve_colors_inverse_swaps_fg_and_bg() {
        let fg_in = ColorRgb {
            r: 10,
            g: 20,
            b: 30,
        };
        let bg_in = ColorRgb {
            r: 40,
            g: 50,
            b: 60,
        };
        let defaults = (ColorRgb { r: 1, g: 2, b: 3 }, ColorRgb { r: 4, g: 5, b: 6 });
        let (fg, bg) = resolve_colors(Some(fg_in), Some(bg_in), defaults, true);
        assert_eq!(fg, bg_in, "inverse swaps foreground and background");
        assert_eq!(bg, fg_in, "inverse swaps foreground and background");
    }
}
