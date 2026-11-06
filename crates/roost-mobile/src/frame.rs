//! Frame payload v1 (plan 089 C6): what one pull of a tab's screen hands
//! Flutter. One flat little-endian buffer per frame, so the FFI cost is
//! one `Uint8List` however many cells changed.
//!
//! ```text
//! u8  version = 1
//! u8  flags        bit0 FULL bit1 CURSOR_VISIBLE bit2 MOUSE_TRACKING
//!                  bit3 ALT_SCREEN bit4 BRACKETED_PASTE
//! u16 cols   u16 rows
//! u32 default_fg   u32 default_bg   u32 cursor_color      (0x00RRGGBB)
//! u16 cursor_col   u16 cursor_row   u8 cursor_style
//! u32 scroll_total  u32 scroll_offset  u32 scroll_len
//! u16 row_count
//!   repeat: u16 row_index  u16 run_count
//!     repeat: u16 col  u16 cells  u32 fg  u32 bg  u8 attrs  u16 text_len  text
//! ```

use roost_vt::{resolve_colors, Cell, CellWide, ColorRgb, CursorVisualStyle};

pub const VERSION: u8 = 1;

/// Header `flags` bits.
pub mod flags {
    /// The consumer drops its row cache: every row of the grid follows.
    pub const FULL: u8 = 1 << 0;
    pub const CURSOR_VISIBLE: u8 = 1 << 1;
    pub const MOUSE_TRACKING: u8 = 1 << 2;
    pub const ALT_SCREEN: u8 = 1 << 3;
    pub const BRACKETED_PASTE: u8 = 1 << 4;
}

/// Run `attrs` bits.
pub mod attrs {
    pub const BOLD: u8 = 1 << 0;
    pub const ITALIC: u8 = 1 << 1;
    pub const UNDERLINE: u8 = 1 << 2;
    pub const STRIKE: u8 = 1 << 3;
    pub const FAINT: u8 = 1 << 4;
    /// The background is the cell's own (an SGR background, or inverse),
    /// not the default canvas behind it.
    pub const EXPLICIT_BG: u8 = 1 << 5;
    /// One double-width grapheme spanning `cells == 2`.
    pub const WIDE: u8 = 1 << 6;
}

/// `cursor_style` values.
pub mod cursor_style {
    pub const BLOCK: u8 = 0;
    pub const BAR: u8 = 1;
    pub const UNDERLINE: u8 = 2;
    pub const BLOCK_HOLLOW: u8 = 3;
}

pub fn cursor_style_code(style: CursorVisualStyle) -> u8 {
    match style {
        CursorVisualStyle::Block => cursor_style::BLOCK,
        CursorVisualStyle::Bar => cursor_style::BAR,
        CursorVisualStyle::Underline => cursor_style::UNDERLINE,
        CursorVisualStyle::BlockHollow => cursor_style::BLOCK_HOLLOW,
    }
}

pub fn rgb(color: ColorRgb) -> u32 {
    (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)
}

/// Everything in a frame except its rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub cols: u16,
    pub rows: u16,
    pub default_fg: u32,
    pub default_bg: u32,
    pub cursor_color: u32,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_style: u8,
    pub scroll_total: u32,
    pub scroll_offset: u32,
    pub scroll_len: u32,
}

/// Consecutive cells drawn alike. `text` holds one grapheme per cell
/// (a blank cell inside the run is a space), or the one grapheme of a
/// [`attrs::WIDE`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub col: u16,
    pub cells: u16,
    pub fg: u32,
    pub bg: u32,
    pub attrs: u8,
    pub text: String,
}

/// One viewport row, replacing whatever the consumer held for it. No
/// runs means the row is all default background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub index: u16,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    pub header: Header,
    pub rows: Vec<Row>,
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let text_bytes: usize = self
            .rows
            .iter()
            .flat_map(|row| row.runs.iter())
            .map(|run| 15 + run.text.len())
            .sum();
        let mut out = Vec::with_capacity(41 + self.rows.len() * 4 + text_bytes);
        out.push(VERSION);
        out.push(h.flags);
        put_u16(&mut out, h.cols);
        put_u16(&mut out, h.rows);
        put_u32(&mut out, h.default_fg);
        put_u32(&mut out, h.default_bg);
        put_u32(&mut out, h.cursor_color);
        put_u16(&mut out, h.cursor_col);
        put_u16(&mut out, h.cursor_row);
        out.push(h.cursor_style);
        put_u32(&mut out, h.scroll_total);
        put_u32(&mut out, h.scroll_offset);
        put_u32(&mut out, h.scroll_len);
        put_u16(&mut out, saturate_u16(self.rows.len()));
        for row in self.rows.iter().take(usize::from(u16::MAX)) {
            put_u16(&mut out, row.index);
            put_u16(&mut out, saturate_u16(row.runs.len()));
            for run in row.runs.iter().take(usize::from(u16::MAX)) {
                put_u16(&mut out, run.col);
                put_u16(&mut out, run.cells);
                put_u32(&mut out, run.fg);
                put_u32(&mut out, run.bg);
                out.push(run.attrs);
                // `row_runs` closes a run before its text outgrows u16.
                put_u16(&mut out, saturate_u16(run.text.len()));
                out.extend_from_slice(run.text.as_bytes());
            }
        }
        out
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn saturate_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// A run being grown. `blank_tail_*` counts the drawless cells at its
/// end, which are cut off when it closes: a run never begins or ends on
/// a cell that draws nothing, while one inside it stays as a space so a
/// line of words is one run rather than one per word.
struct Open {
    run: Run,
    blank_tail_cells: u16,
    blank_tail_bytes: usize,
}

impl Open {
    fn close(mut self, runs: &mut Vec<Run>) {
        self.run.cells -= self.blank_tail_cells;
        if self.run.cells == 0 {
            return;
        }
        let keep = self.run.text.len() - self.blank_tail_bytes;
        self.run.text.truncate(keep);
        runs.push(self.run);
    }
}

/// Group one row's cells into C6 runs: resolved colors (inverse already
/// applied, `bold` being the theme's bold color), a wide cell always its
/// own two-cell run, and cells that draw nothing — no glyph, no
/// background of their own, no line through or under them — left out.
pub fn row_runs(
    cells: &[Cell],
    defaults: (ColorRgb, ColorRgb),
    bold: Option<ColorRgb>,
    cols: u16,
) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut open: Option<Open> = None;
    for cell in cells {
        if cell.col >= cols || cell.wide == CellWide::SpacerTail {
            continue;
        }
        let style = cell.style;
        let (fg, bg) = resolve_colors(cell.fg, cell.bg, defaults, style, bold);
        let explicit_bg = cell.bg.is_some() || style.inverse;
        let wide = cell.wide == CellWide::Wide;
        let glyph = if style.invisible || cell.wide == CellWide::SpacerHead {
            ""
        } else {
            cell.text.as_str()
        };
        let blank = glyph.is_empty() || glyph == " ";
        let text = if blank { " " } else { glyph };
        let drawless = blank && !explicit_bg && !style.underline && !style.strikethrough;
        let mut bits = 0;
        for (on, bit) in [
            (style.bold, attrs::BOLD),
            (style.italic, attrs::ITALIC),
            (style.underline, attrs::UNDERLINE),
            (style.strikethrough, attrs::STRIKE),
            (style.faint, attrs::FAINT),
            (explicit_bg, attrs::EXPLICIT_BG),
            (wide, attrs::WIDE),
        ] {
            if on {
                bits |= bit;
            }
        }
        let (fg, bg) = (rgb(fg), rgb(bg));

        let joins = open.as_ref().is_some_and(|open| {
            !wide
                && open.run.attrs & attrs::WIDE == 0
                && (open.run.fg, open.run.bg, open.run.attrs) == (fg, bg, bits)
                && open.run.col + open.run.cells == cell.col
                && open.run.text.len() + text.len() <= usize::from(u16::MAX)
        });
        if joins {
            let Some(open) = open.as_mut() else { continue };
            open.run.cells += 1;
            open.run.text.push_str(text);
            if drawless {
                open.blank_tail_cells += 1;
                open.blank_tail_bytes += text.len();
            } else {
                open.blank_tail_cells = 0;
                open.blank_tail_bytes = 0;
            }
            continue;
        }
        if let Some(done) = open.take() {
            done.close(&mut runs);
        }
        if drawless {
            continue;
        }
        let run = Run {
            col: cell.col,
            cells: if wide { 2 } else { 1 },
            fg,
            bg,
            attrs: bits,
            text: text.to_string(),
        };
        if wide {
            runs.push(run);
        } else {
            open = Some(Open {
                run,
                blank_tail_cells: 0,
                blank_tail_bytes: 0,
            });
        }
    }
    if let Some(done) = open {
        done.close(&mut runs);
    }
    runs
}

/// Why a buffer is not a v1 frame.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("frame ends at byte {0}, mid-field")]
    Truncated(usize),
    #[error("unknown frame version {0}")]
    Version(u8),
    #[error("run text at byte {0} is not UTF-8")]
    Utf8(usize),
    #[error("{0} trailing bytes after the last row")]
    Trailing(usize),
}

/// The Rust twin of the Dart decoder, for tests: the golden fixtures
/// and the end-to-end checks read frames back through it.
#[cfg(any(test, feature = "test-support"))]
pub fn decode(bytes: &[u8]) -> Result<Frame, DecodeError> {
    let mut at = Reader { bytes, at: 0 };
    let version = at.u8()?;
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    let header = Header {
        flags: at.u8()?,
        cols: at.u16()?,
        rows: at.u16()?,
        default_fg: at.u32()?,
        default_bg: at.u32()?,
        cursor_color: at.u32()?,
        cursor_col: at.u16()?,
        cursor_row: at.u16()?,
        cursor_style: at.u8()?,
        scroll_total: at.u32()?,
        scroll_offset: at.u32()?,
        scroll_len: at.u32()?,
    };
    let row_count = at.u16()?;
    let mut rows = Vec::with_capacity(usize::from(row_count));
    for _ in 0..row_count {
        let index = at.u16()?;
        let run_count = at.u16()?;
        let mut runs = Vec::with_capacity(usize::from(run_count));
        for _ in 0..run_count {
            let col = at.u16()?;
            let cells = at.u16()?;
            let fg = at.u32()?;
            let bg = at.u32()?;
            let attrs = at.u8()?;
            let len = usize::from(at.u16()?);
            let start = at.at;
            let text = std::str::from_utf8(at.take(len)?)
                .map_err(|_| DecodeError::Utf8(start))?
                .to_string();
            runs.push(Run {
                col,
                cells,
                fg,
                bg,
                attrs,
                text,
            });
        }
        rows.push(Row { index, runs });
    }
    if at.at != bytes.len() {
        return Err(DecodeError::Trailing(bytes.len() - at.at));
    }
    Ok(Frame { header, rows })
}

#[cfg(any(test, feature = "test-support"))]
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

#[cfg(any(test, feature = "test-support"))]
impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(DecodeError::Truncated(self.at))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// A row's text as a painter lays it out: each run's graphemes at their
/// cells, gaps as spaces, trailing blanks trimmed. One `char` per narrow
/// cell, which holds for the single-codepoint text tests use.
#[cfg(any(test, feature = "test-support"))]
pub fn row_text(row: &Row, cols: u16) -> String {
    let mut cells = vec![" ".to_string(); usize::from(cols)];
    for run in &row.runs {
        let col = usize::from(run.col);
        if run.attrs & attrs::WIDE != 0 {
            if let Some(cell) = cells.get_mut(col) {
                cell.clone_from(&run.text);
            }
            if let Some(tail) = cells.get_mut(col + 1) {
                tail.clear();
            }
            continue;
        }
        for (offset, ch) in run.text.chars().enumerate() {
            if let Some(cell) = cells.get_mut(col + offset) {
                *cell = ch.to_string();
            }
        }
    }
    cells.concat().trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use roost_vt::Style;

    use super::*;

    const FG: ColorRgb = ColorRgb::new(0xe5, 0xe5, 0xe5);
    const BG: ColorRgb = ColorRgb::new(0x1c, 0x1c, 0x1c);
    const RED: ColorRgb = ColorRgb::new(0xcc, 0x00, 0x00);

    fn cell(col: u16, text: &str) -> Cell {
        Cell {
            col,
            bg: None,
            fg: None,
            text: text.to_string(),
            style: Style::default(),
            wide: CellWide::Narrow,
        }
    }

    fn row_of(text: &str) -> Vec<Cell> {
        text.chars()
            .enumerate()
            .map(|(col, ch)| cell(col as u16, if ch == ' ' { "" } else { &text[col..col + 1] }))
            .collect()
    }

    fn runs(cells: &[Cell]) -> Vec<Run> {
        row_runs(cells, (FG, BG), None, 80)
    }

    #[test]
    fn words_on_one_line_are_one_run_with_their_spaces() {
        let got = runs(&row_of("hello big world   "));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].col, 0);
        assert_eq!(got[0].cells, 15);
        assert_eq!(got[0].text, "hello big world");
        assert_eq!((got[0].fg, got[0].bg, got[0].attrs), (rgb(FG), rgb(BG), 0));
    }

    #[test]
    fn leading_blanks_are_a_gap_not_part_of_the_run() {
        let got = runs(&row_of("   ok"));
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].col, got[0].cells), (3, 2));
        assert_eq!(got[0].text, "ok");
    }

    #[test]
    fn a_style_change_starts_a_new_run() {
        let mut cells = row_of("ab cd");
        cells[3].style.bold = true;
        cells[4].style.bold = true;
        let got = runs(&cells);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(
            (got[0].col, got[0].text.as_str(), got[0].attrs),
            (0, "ab", 0)
        );
        assert_eq!(
            (got[1].col, got[1].text.as_str(), got[1].attrs),
            (3, "cd", attrs::BOLD)
        );
    }

    #[test]
    fn a_wide_cell_is_its_own_two_cell_run_and_its_spacer_is_skipped() {
        let mut cells = vec![cell(0, "a"), cell(1, "中"), cell(2, ""), cell(3, "b")];
        cells[1].wide = CellWide::Wide;
        cells[2].wide = CellWide::SpacerTail;
        // The spacer wears the wide cell's background: drawn on its own
        // it would paint over the glyph's second half.
        cells[1].bg = Some(RED);
        cells[2].bg = Some(RED);
        let got = runs(&cells);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(
            (got[0].col, got[0].cells, got[0].text.as_str()),
            (0, 1, "a")
        );
        assert_eq!(
            (got[1].col, got[1].cells, got[1].text.as_str(), got[1].attrs),
            (1, 2, "中", attrs::WIDE | attrs::EXPLICIT_BG)
        );
        assert_eq!(
            (got[2].col, got[2].cells, got[2].text.as_str()),
            (3, 1, "b")
        );
    }

    #[test]
    fn blank_cells_with_their_own_background_are_drawn() {
        let mut cells = row_of("x   ");
        for blank in &mut cells[2..4] {
            blank.bg = Some(RED);
        }
        let got = runs(&cells);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].text, "x");
        assert_eq!(
            (got[1].col, got[1].cells, got[1].text.as_str()),
            (2, 2, "  ")
        );
        assert_eq!(got[1].bg, rgb(RED));
        assert_eq!(got[1].attrs, attrs::EXPLICIT_BG);
    }

    #[test]
    fn an_underlined_space_is_drawn_and_a_plain_one_is_not() {
        let mut cells = row_of("a b");
        for c in &mut cells {
            c.style.underline = true;
        }
        let got = runs(&cells);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "a b");
        assert_eq!(got[0].attrs, attrs::UNDERLINE);

        let mut trailing = row_of("a  ");
        trailing[2].style.underline = true;
        let got = runs(&trailing);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!((got[1].col, got[1].text.as_str()), (2, " "));
    }

    #[test]
    fn inverse_is_resolved_into_the_colors() {
        let mut cells = row_of("iv");
        for c in &mut cells {
            c.style.inverse = true;
        }
        let got = runs(&cells);
        assert_eq!((got[0].fg, got[0].bg), (rgb(BG), rgb(FG)));
        assert_eq!(got[0].attrs, attrs::EXPLICIT_BG);
    }

    #[test]
    fn invisible_text_keeps_its_cells_but_not_its_glyphs() {
        let mut cells = row_of("pw");
        for c in &mut cells {
            c.style.invisible = true;
            c.bg = Some(RED);
        }
        let got = runs(&cells);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].cells, got[0].text.as_str()), (2, "  "));
    }

    #[test]
    fn cells_past_the_grid_are_dropped() {
        let got = row_runs(&row_of("abcdef"), (FG, BG), None, 3);
        assert_eq!(got[0].text, "abc");
    }

    #[test]
    fn a_run_closes_before_its_text_outgrows_u16() {
        let cells: Vec<Cell> = (0..40_000u16).map(|col| cell(col, "é")).collect();
        let got = row_runs(&cells, (FG, BG), None, u16::MAX);
        assert!(got.len() > 1);
        assert!(got
            .iter()
            .all(|run| run.text.len() <= usize::from(u16::MAX)));
        let total: u32 = got.iter().map(|run| u32::from(run.cells)).sum();
        assert_eq!(total, 40_000);
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let frame = Frame {
            header: Header {
                flags: flags::FULL | flags::CURSOR_VISIBLE,
                cols: 20,
                rows: 5,
                default_fg: 0x00e5_e5e5,
                default_bg: 0x001c_1c1c,
                cursor_color: 0x00bb_bbbb,
                cursor_col: 3,
                cursor_row: 1,
                cursor_style: cursor_style::BAR,
                scroll_total: 9,
                scroll_offset: 4,
                scroll_len: 5,
            },
            rows: vec![
                Row {
                    index: 0,
                    runs: vec![Run {
                        col: 2,
                        cells: 2,
                        fg: 1,
                        bg: 2,
                        attrs: attrs::WIDE,
                        text: "中".into(),
                    }],
                },
                Row {
                    index: 4,
                    runs: vec![],
                },
            ],
        };
        let bytes = frame.encode();
        assert_eq!(decode(&bytes), Ok(frame));
        assert_eq!(bytes[0], VERSION);
        assert_eq!(&bytes[2..4], &20u16.to_le_bytes(), "little-endian cols");
    }

    #[test]
    fn decode_rejects_truncation_and_trailing_bytes() {
        let bytes = Frame::default().encode();
        assert!(matches!(
            decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Truncated(_))
        ));
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(decode(&long), Err(DecodeError::Trailing(1)));
    }
}
