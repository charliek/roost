//! Safe wrapper around `ghostty_render_state_*`.
//!
//! `RenderState` is the per-UI snapshot the renderer walks. Lifecycle:
//!   1. `RenderState::new()` allocates the render-state, row-iterator,
//!      and row-cells handles once. They're reused across frames.
//!   2. `update(&terminal)` snapshots the current screen.
//!   3. `walk(|cell| ...)` iterates rows × cells, calling the closure
//!      once per cell.
//!   4. `cursor()` / `colors()` extract additional per-frame data.
//!
//! Matches `mac/Sources/Roost/RenderState.swift` 1:1 in shape — same
//! constructor pattern, same walk surface, same cursor info layout.

use std::ptr;

use crate::sys;
use crate::{Error, Result, Terminal};

/// sRGB triple, layout-compatible with libghostty's `GhosttyColorRgb`.
/// Repr-C so palette arrays can be passed through `set_color_palette`
/// without copying.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl ColorRgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Convert to f64 triple normalized to [0.0, 1.0] for Cairo /
    /// Pango color sinks.
    pub fn to_f64(self) -> (f64, f64, f64) {
        (
            self.r as f64 / 255.0,
            self.g as f64 / 255.0,
            self.b as f64 / 255.0,
        )
    }
}

impl From<sys::GhosttyColorRgb> for ColorRgb {
    fn from(c: sys::GhosttyColorRgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// Cursor visual style from `GhosttyRenderStateCursorVisualStyle`.
/// `BlockHollow` is libghostty's hint that the cursor block should be
/// rendered hollow (e.g. unfocused window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorVisualStyle {
    Block,
    Bar,
    Underline,
    BlockHollow,
}

impl CursorVisualStyle {
    fn from_u32(v: u32) -> Self {
        // The bindgen-generated constant names are long; we depend on
        // the underlying integer codes (Ghostty's header file fixes
        // these). 0=block, 1=bar, 2=underline, 3=block-hollow.
        match v {
            1 => Self::Bar,
            2 => Self::Underline,
            3 => Self::BlockHollow,
            _ => Self::Block,
        }
    }
}

/// Cursor data extracted from the render state.
#[derive(Debug, Clone, Copy)]
pub struct CursorInfo {
    /// Column inside the viewport (0-indexed, left edge = 0).
    pub col: u32,
    /// Row inside the viewport (0-indexed, top edge = 0).
    pub row: u32,
    /// True if the cursor sits on the second column of a wide-character
    /// (CJK / emoji) — the renderer should skip its own glyph draw and
    /// let the wide-char cell carry the cursor.
    pub wide_tail: bool,
    /// DECTCEM mode 25 — whether the cursor should be drawn at all.
    pub visible: bool,
    /// DECSCUSR blink-request bit. The UI's blink timer drives the
    /// visual on/off cycle; this just says whether the cursor *wants*
    /// to blink.
    pub blinking: bool,
    /// `CursorVisualStyle::*` from libghostty.
    pub visual_style: CursorVisualStyle,
    /// OSC 12 cursor color override, if set.
    pub color: Option<ColorRgb>,
}

/// Snapshot of the default fg/bg/cursor colors at frame time. Maps to
/// `GhosttyRenderStateColors` but only exposes the fields the renderer
/// uses today.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub foreground: ColorRgb,
    pub background: ColorRgb,
    pub cursor: Option<ColorRgb>,
}

/// Per-cell data the renderer needs. Background / foreground are
/// `Option` because cells often inherit the terminal default (None →
/// renderer paints with `Colors::foreground` / `Colors::background`).
#[derive(Debug, Clone)]
pub struct Cell {
    /// Column inside the row (0-indexed).
    pub col: u16,
    /// Cell background color, if set explicitly via SGR.
    pub bg: Option<ColorRgb>,
    /// Cell foreground color, if set explicitly via SGR.
    pub fg: Option<ColorRgb>,
    /// Grapheme cluster text, UTF-8. Empty for blank cells.
    pub text: String,
}

pub struct RenderState {
    handle: sys::GhosttyRenderState,
    row_iter: sys::GhosttyRenderStateRowIterator,
    row_cells: sys::GhosttyRenderStateRowCells,
}

unsafe impl Send for RenderState {}

impl RenderState {
    pub fn new() -> Result<Self> {
        let mut handle: sys::GhosttyRenderState = ptr::null_mut();
        // SAFETY: null allocator + out-pointer we own.
        let rc = unsafe { sys::ghostty_render_state_new(ptr::null_mut(), &mut handle) };
        Error::from_result(rc)?;
        if handle.is_null() {
            return Err(Error::NullHandle);
        }

        let mut row_iter: sys::GhosttyRenderStateRowIterator = ptr::null_mut();
        // SAFETY: see above.
        let rc =
            unsafe { sys::ghostty_render_state_row_iterator_new(ptr::null_mut(), &mut row_iter) };
        if let Err(e) = Error::from_result(rc) {
            // SAFETY: handle non-null, just-allocated.
            unsafe { sys::ghostty_render_state_free(handle) };
            return Err(e);
        }
        if row_iter.is_null() {
            // SAFETY: handle non-null.
            unsafe { sys::ghostty_render_state_free(handle) };
            return Err(Error::NullHandle);
        }

        let mut row_cells: sys::GhosttyRenderStateRowCells = ptr::null_mut();
        // SAFETY: see above.
        let rc =
            unsafe { sys::ghostty_render_state_row_cells_new(ptr::null_mut(), &mut row_cells) };
        if let Err(e) = Error::from_result(rc) {
            // SAFETY: both prior handles non-null.
            unsafe { sys::ghostty_render_state_row_iterator_free(row_iter) };
            unsafe { sys::ghostty_render_state_free(handle) };
            return Err(e);
        }
        if row_cells.is_null() {
            // SAFETY: see above.
            unsafe { sys::ghostty_render_state_row_iterator_free(row_iter) };
            unsafe { sys::ghostty_render_state_free(handle) };
            return Err(Error::NullHandle);
        }

        Ok(Self {
            handle,
            row_iter,
            row_cells,
        })
    }

    /// Snapshot the terminal's current state into this render state.
    /// Call once per frame; subsequent `walk` / `cursor` / `colors`
    /// reads see the snapshot, not the live terminal.
    pub fn update(&mut self, terminal: &Terminal) -> Result<()> {
        // SAFETY: both handles non-null per constructors.
        let rc = unsafe { sys::ghostty_render_state_update(self.handle, terminal.handle()) };
        Error::from_result(rc)
    }

    /// Raw FFI handle. Internal use for crates that need to call a
    /// not-yet-wrapped getter.
    pub fn as_ffi(&self) -> sys::GhosttyRenderState {
        self.handle
    }

    /// Default fg/bg/cursor colors at frame time. The renderer paints
    /// the canvas with `background` before walking cells.
    pub fn colors(&self) -> Result<Colors> {
        let mut raw = sys::GhosttyRenderStateColors {
            size: std::mem::size_of::<sys::GhosttyRenderStateColors>(),
            background: sys::GhosttyColorRgb::default(),
            foreground: sys::GhosttyColorRgb::default(),
            cursor: sys::GhosttyColorRgb::default(),
            cursor_has_value: false,
            palette: [sys::GhosttyColorRgb::default(); 256],
        };
        // SAFETY: handle non-null; raw is a real local.
        let rc = unsafe { sys::ghostty_render_state_colors_get(self.handle, &mut raw) };
        Error::from_result(rc)?;
        Ok(Colors {
            foreground: raw.foreground.into(),
            background: raw.background.into(),
            cursor: raw.cursor_has_value.then(|| raw.cursor.into()),
        })
    }

    /// Cursor info if the cursor is in the visible viewport.
    pub fn cursor(&self) -> Option<CursorInfo> {
        let mut has_value: bool = false;
        // SAFETY: handle non-null.
        let rc = unsafe {
            sys::ghostty_render_state_get(
                self.handle,
                sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE,
                (&mut has_value) as *mut bool as *mut _,
            )
        };
        if Error::from_result(rc).is_err() || !has_value {
            return None;
        }

        let col = self
            .read_u32(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X)
            .unwrap_or(0);
        let row = self
            .read_u32(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y)
            .unwrap_or(0);
        let wide_tail = self
            .read_bool(
                sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_WIDE_TAIL,
            )
            .unwrap_or(false);
        let visible = self
            .read_bool(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE)
            .unwrap_or(true);
        let blinking = self
            .read_bool(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_BLINKING)
            .unwrap_or(false);
        let style = self
            .read_u32(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE)
            .unwrap_or(0);

        let cursor_has_color = self
            .read_bool(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLOR_CURSOR_HAS_VALUE)
            .unwrap_or(false);
        let color = if cursor_has_color {
            self.read_color(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLOR_CURSOR)
        } else {
            None
        };

        Some(CursorInfo {
            col,
            row,
            wide_tail,
            visible,
            blinking,
            visual_style: CursorVisualStyle::from_u32(style),
            color,
        })
    }

    /// Iterate rows × cells, calling `f(row, cell)` once per cell.
    /// Reuses the same row-iterator + row-cells handles across frames;
    /// libghostty's contract says they're safe to re-bind via the next
    /// `_next` call without reallocation.
    pub fn walk(&mut self, terminal: &Terminal, mut f: impl FnMut(u32, Cell)) -> Result<()> {
        // Rebind the row iterator to this frame's state. The C signature
        // expects `GhosttyRenderStateRowIterator*` (pointer-to-handle slot),
        // not the handle's value — the function writes into the slot to
        // re-anchor the pre-allocated iterator at the new frame. Passing
        // `self.row_iter as *mut _` would point at the iterator's IMPL
        // and corrupt its internal state, leaving `..._next` returning
        // false on every row (silent: no error, just zero cells walked).
        // Mirrors `mac/Sources/Roost/RenderState.swift::walk`'s
        // `withUnsafeMutablePointer(to: &self.rowIter)` pattern.
        // SAFETY: state + iter handles non-null per constructor.
        let rc = unsafe {
            sys::ghostty_render_state_get(
                self.handle,
                sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                (&mut self.row_iter) as *mut _ as *mut _,
            )
        };
        Error::from_result(rc)?;
        // Keep `terminal` alive across the walk so libghostty doesn't
        // drop allocations that back the iterators. Borrowing & makes
        // the lifetime explicit; the variable itself is intentionally
        // unused.
        let _ = terminal;

        let mut row_idx: u32 = 0;
        // SAFETY: iter handle non-null.
        while unsafe { sys::ghostty_render_state_row_iterator_next(self.row_iter) } {
            // Bind this row's cells to row_cells. Same pointer-to-slot
            // semantics as the row iterator above — pass `&mut`, not the
            // handle value.
            // SAFETY: iter + cells handles non-null.
            let rc = unsafe {
                sys::ghostty_render_state_row_get(
                    self.row_iter,
                    sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    (&mut self.row_cells) as *mut _ as *mut _,
                )
            };
            if Error::from_result(rc).is_err() {
                row_idx += 1;
                continue;
            }

            let mut col: u16 = 0;
            // SAFETY: row_cells handle non-null.
            while unsafe { sys::ghostty_render_state_row_cells_next(self.row_cells) } {
                let cell = self.read_current_cell(col);
                f(row_idx, cell);
                col = col.saturating_add(1);
            }
            row_idx += 1;
        }
        Ok(())
    }

    fn read_current_cell(&self, col: u16) -> Cell {
        let bg = self.read_cells_color(
            sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
        );
        let fg = self.read_cells_color(
            sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
        );

        // Graphemes: read length first, then the buffer if non-zero.
        let mut len: u32 = 0;
        let rc = unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.row_cells,
                sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                (&mut len) as *mut u32 as *mut _,
            )
        };
        let text = if Error::from_result(rc).is_ok() && len > 0 {
            // libghostty exposes the grapheme buffer as a `*const u32`
            // array of codepoints. We allocate enough capacity once
            // per cell.
            let mut buf: Vec<u32> = vec![0; len as usize];
            let rc = unsafe {
                sys::ghostty_render_state_row_cells_get(
                    self.row_cells,
                    sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                    buf.as_mut_ptr() as *mut _,
                )
            };
            if Error::from_result(rc).is_ok() {
                buf.into_iter()
                    .filter_map(char::from_u32)
                    .collect::<String>()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        Cell { col, bg, fg, text }
    }

    fn read_u32(&self, data: sys::GhosttyRenderStateData) -> Result<u32> {
        let mut out: u32 = 0;
        // SAFETY: handle non-null; out is local.
        let rc = unsafe {
            sys::ghostty_render_state_get(self.handle, data, (&mut out) as *mut u32 as *mut _)
        };
        Error::from_result(rc)?;
        Ok(out)
    }

    fn read_bool(&self, data: sys::GhosttyRenderStateData) -> Result<bool> {
        let mut out: bool = false;
        // SAFETY: handle non-null; out is local.
        let rc = unsafe {
            sys::ghostty_render_state_get(self.handle, data, (&mut out) as *mut bool as *mut _)
        };
        Error::from_result(rc)?;
        Ok(out)
    }

    fn read_color(&self, data: sys::GhosttyRenderStateData) -> Option<ColorRgb> {
        let mut out = sys::GhosttyColorRgb::default();
        // SAFETY: handle non-null; out is local.
        let rc = unsafe {
            sys::ghostty_render_state_get(self.handle, data, (&mut out) as *mut _ as *mut _)
        };
        Error::from_result(rc).ok()?;
        Some(out.into())
    }

    fn read_cells_color(&self, data: sys::GhosttyRenderStateRowCellsData) -> Option<ColorRgb> {
        // libghostty returns NO_VALUE when the cell uses the default
        // color — that's not an error, it just means "fall back to
        // Colors::foreground / background".
        let mut out = sys::GhosttyColorRgb::default();
        let rc = unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.row_cells,
                data,
                (&mut out) as *mut _ as *mut _,
            )
        };
        match Error::from_result(rc) {
            Ok(()) => Some(out.into()),
            Err(Error::NoValue) => None,
            Err(_) => None,
        }
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        // SAFETY: all three handles allocated by constructor; we own
        // them exclusively. Free in reverse construction order.
        unsafe {
            sys::ghostty_render_state_row_cells_free(self.row_cells);
            sys::ghostty_render_state_row_iterator_free(self.row_iter);
            sys::ghostty_render_state_free(self.handle);
        }
    }
}

#[cfg(all(test, feature = "ffi"))]
mod tests {
    use super::*;
    use crate::{Terminal, TerminalOptions};

    /// Regression test for the row-iterator binding bug fixed in M1 of
    /// `polish/gtk-parity`. Pre-fix, `walk` passed the iterator handle's
    /// VALUE as the `out` pointer to `ghostty_render_state_get`, which
    /// corrupted the iterator's internal state and caused every
    /// subsequent `..._row_iterator_next` to return `false` — silently
    /// yielding zero cells walked even after `vt_write` fed bytes in.
    /// Symptom in the GTK Linux UI: terminal area blank with the cursor
    /// visible but no glyphs. Cross-check against the Mac UI, where
    /// `RenderState.walk` correctly passes `&self.rowIter`.
    #[test]
    fn walk_yields_cells_after_vt_write() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .expect("Terminal::new");
        // ASCII "hello" — exactly 5 visible cells at columns 0..5 on row 0.
        terminal.vt_write(b"hello");

        let mut render_state = RenderState::new().expect("RenderState::new");
        render_state.update(&terminal).expect("update");

        let mut total_cells = 0u32;
        let mut visible: Vec<(u32, u16, String)> = Vec::new();
        render_state
            .walk(&terminal, |row, cell| {
                total_cells += 1;
                if !cell.text.is_empty() && cell.text != " " {
                    visible.push((row, cell.col, cell.text.clone()));
                }
            })
            .expect("walk");

        // 80 cols × 24 rows = 1920 cells walked.
        assert_eq!(
            total_cells, 1920,
            "walk visited {} cells but expected 1920 (80×24); \
             pre-fix this was 0 due to the row-iterator pointer-indirection bug",
            total_cells
        );

        // "hello" should land at (0, 0)..(0, 4).
        let glyphs: String = visible
            .iter()
            .filter(|(row, _, _)| *row == 0)
            .map(|(_, _, t)| t.as_str())
            .collect();
        assert_eq!(
            glyphs, "hello",
            "row 0 visible glyphs should be \"hello\", got {:?}",
            visible
        );
    }
}
