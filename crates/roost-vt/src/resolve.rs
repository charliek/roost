//! Pure dump-densification: turning a row of libghostty-vt [`Cell`]s into
//! the sparse draw-ready form both roost UIs render from.
//!
//! Lives in `roost-vt` (rather than a UI crate) because it is built
//! entirely on `roost-vt` types and has no rendering-toolkit dependency —
//! any consumer that walks a terminal's cells wants the same densified
//! shape, not just the iced UI.

use crate::{Cell, CellWide, ColorRgb, Style};

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
    /// The grapheme spans this column and the next (libghostty's
    /// [`CellWide::Wide`]).
    pub wide: bool,
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
    /// terminal's default fg/bg pair, the theme's bold color, and the grid
    /// width. That list IS the cache key `TerminalTab::refresh_snapshot`
    /// guards on; adding a fifth input here means extending those guards
    /// (see that function's caching invariant).
    ///
    /// `bold` is the theme's `bold-color` (see [`resolve_colors`]). Only
    /// the UI has a theme; the headless builders (the engine's and
    /// session's dumps, `vt_dump`) pass `None`, so their bold text keeps
    /// the plain default foreground.
    pub fn build(
        cells: &[Cell],
        defaults: (ColorRgb, ColorRgb),
        bold: Option<ColorRgb>,
        cols: u16,
    ) -> Self {
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
                resolve_colors(cell.fg, cell.bg, defaults, cell.style, bold);
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
                    wide: cell.wide == CellWide::Wide,
                });
            }
        }
        row.text.truncate(row.text.trim_end().len());
        row
    }
}

/// A cell's (foreground, background): its own colors, else the terminal
/// defaults, swapped when inverse.
///
/// `bold` is the theme's `bold-color`. A bold cell with no foreground of
/// its own that is not inverse takes it — the Swift app's
/// `resolveCellColors` rule.
pub fn resolve_colors(
    foreground: Option<ColorRgb>,
    background: Option<ColorRgb>,
    defaults: (ColorRgb, ColorRgb),
    style: Style,
    bold: Option<ColorRgb>,
) -> (ColorRgb, ColorRgb) {
    let mut foreground = match foreground {
        Some(own) => own,
        None if style.bold && !style.inverse => bold.unwrap_or(defaults.0),
        None => defaults.0,
    };
    let mut background = background.unwrap_or(defaults.1);
    if style.inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    (foreground, background)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: (ColorRgb, ColorRgb) =
        (ColorRgb { r: 1, g: 2, b: 3 }, ColorRgb { r: 4, g: 5, b: 6 });
    const BOLD_COLOR: ColorRgb = ColorRgb {
        r: 70,
        g: 80,
        b: 90,
    };
    const PLAIN: Style = Style {
        bold: false,
        italic: false,
        inverse: false,
    };
    const BOLD: Style = Style {
        bold: true,
        ..PLAIN
    };
    const INVERSE: Style = Style {
        inverse: true,
        ..PLAIN
    };

    #[test]
    fn resolve_colors_defaults_when_unset() {
        let (fg, bg) = resolve_colors(None, None, DEFAULTS, PLAIN, None);
        assert_eq!(fg, DEFAULTS.0);
        assert_eq!(bg, DEFAULTS.1);
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
        let (fg, bg) = resolve_colors(Some(explicit_fg), Some(explicit_bg), DEFAULTS, PLAIN, None);
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
        let (fg, bg) = resolve_colors(Some(fg_in), Some(bg_in), DEFAULTS, INVERSE, None);
        assert_eq!(fg, bg_in, "inverse swaps foreground and background");
        assert_eq!(bg, fg_in, "inverse swaps foreground and background");
    }

    #[test]
    fn bold_text_in_the_default_foreground_takes_the_bold_color() {
        let (fg, bg) = resolve_colors(None, None, DEFAULTS, BOLD, Some(BOLD_COLOR));
        assert_eq!(fg, BOLD_COLOR);
        assert_eq!(bg, DEFAULTS.1);
    }

    #[test]
    fn preservation_bold_text_with_its_own_foreground_keeps_it() {
        let own = ColorRgb {
            r: 200,
            g: 10,
            b: 10,
        };
        let (fg, _) = resolve_colors(Some(own), None, DEFAULTS, BOLD, Some(BOLD_COLOR));
        assert_eq!(fg, own);
    }

    #[test]
    fn preservation_inverse_bold_text_ignores_the_bold_color() {
        let style = Style {
            inverse: true,
            ..BOLD
        };
        let (fg, bg) = resolve_colors(None, None, DEFAULTS, style, Some(BOLD_COLOR));
        assert_eq!((fg, bg), (DEFAULTS.1, DEFAULTS.0));
    }

    #[test]
    fn preservation_bold_text_without_a_bold_color_keeps_the_default() {
        let (fg, _) = resolve_colors(None, None, DEFAULTS, BOLD, None);
        assert_eq!(fg, DEFAULTS.0);
    }

    #[test]
    fn preservation_plain_text_ignores_the_bold_color() {
        let (fg, _) = resolve_colors(None, None, DEFAULTS, PLAIN, Some(BOLD_COLOR));
        assert_eq!(fg, DEFAULTS.0);
    }
}
