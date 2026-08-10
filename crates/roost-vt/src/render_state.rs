//! Safe wrapper around `ghostty_render_state_*`.
//!
//! `RenderState` is the per-UI snapshot the renderer walks. Lifecycle:
//!   1. `RenderState::new()` allocates the render-state, row-iterator,
//!      and row-cells handles once. They're reused across frames.
//!   2. `update(&terminal)` snapshots the current screen.
//!   3. `walk(|cell| ...)` iterates rows × cells, calling the closure
//!      once per cell; `walk_dirty` iterates only the rows that
//!      changed and consumes the frame's damage.
//!   4. `cursor()` / `colors()` extract additional per-frame data.
//!
//! `mac/Sources/Roost/RenderState.swift` mirrors the constructor,
//! walk, and cursor-info shape — but the two have **intentionally
//! diverged** as of the dirty-tracking API below (`Dirty`, `dirty`,
//! `mark_full`, `dirty_rows`, `walk_dirty`). Swift deliberately does
//! not get dirty tracking: it is the daily driver, its full-grid
//! renderer is adequate, and the macOS-Iced evaluation may retire it,
//! so investing in its render path is potentially wasted work. Do not
//! "restore parity" here without that decision changing.

use std::ptr;

use crate::sys;
use crate::{ColorRgb, Error, Result, Terminal};

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

/// SGR style bits the renderer needs to resolve effective fg/bg.
/// `Bold / Italic / Inverse` are the three bits the cell-color
/// resolver consumes to swap fg↔bg for `\e[7m` cells and apply the
/// bold-accent rule. Underline / faint / blink / strikethrough /
/// overline ride along the C struct but no caller uses them yet, so
/// we drop them on the way in to keep `Cell` small.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub inverse: bool,
}

/// How a cell participates in double-width text. Mirrors
/// `GhosttyCellWide`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CellWide {
    #[default]
    Narrow,
    /// Carries a double-width grapheme; the next column is its
    /// [`CellWide::SpacerTail`].
    Wide,
    /// Placeholder occupying the second column of a wide grapheme.
    SpacerTail,
    /// Placeholder at the end of a row where a wide grapheme did not
    /// fit and wrapped to the next row.
    SpacerHead,
}

impl CellWide {
    fn from_raw(raw: sys::GhosttyCellWide) -> Self {
        match raw {
            sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_WIDE => Self::Wide,
            sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL => Self::SpacerTail,
            sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD => Self::SpacerHead,
            _ => Self::Narrow,
        }
    }

    /// True for the placeholder cells that carry no text of their own.
    /// Text extraction skips them so a wide grapheme contributes one
    /// grapheme, not a grapheme plus a phantom space.
    pub fn is_spacer(self) -> bool {
        matches!(self, Self::SpacerTail | Self::SpacerHead)
    }
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
    /// SGR style bits (bold / italic / inverse). Default-style cells
    /// carry `Style::default()` — all bits clear.
    pub style: Style,
    /// Double-width role. Spacers are textless placeholders, not blanks.
    pub wide: CellWide,
}

/// A row's soft-wrap flags, from `GHOSTTY_ROW_DATA_WRAP` and
/// `GHOSTTY_ROW_DATA_WRAP_CONTINUATION`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowWrap {
    /// The row soft-wraps into the next one: there is no line break
    /// between them, only a screen edge.
    pub wrap: bool,
    /// The row is itself the continuation of the row above.
    pub wrap_continuation: bool,
}

/// Global dirty state after `update`. Maps `GhosttyRenderStateDirty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    Clean,
    Partial,
    Full,
}

impl Dirty {
    fn from_raw(v: sys::GhosttyRenderStateDirty) -> Self {
        match v {
            sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FALSE => Self::Clean,
            sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_PARTIAL => Self::Partial,
            // Anything we don't recognize — including a variant a future
            // Ghostty adds — maps to `Full`, not `Clean`. Guessing "clean"
            // for an unknown state would skip the rebuild and freeze the
            // screen; guessing "full" only costs a redraw.
            _ => Self::Full,
        }
    }
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
        self.bind_row_iterator()?;
        // Keep `terminal` alive across the walk so libghostty doesn't
        // drop allocations that back the iterators. Borrowing & makes
        // the lifetime explicit; the variable itself is intentionally
        // unused.
        let _ = terminal;

        for row_idx in 0u32.. {
            // SAFETY: iter handle non-null.
            if !unsafe { sys::ghostty_render_state_row_iterator_next(self.row_iter) } {
                break;
            }
            if self.bind_row_cells().is_err() {
                continue;
            }

            let mut col: u16 = 0;
            // SAFETY: row_cells handle non-null.
            while unsafe { sys::ghostty_render_state_row_cells_next(self.row_cells) } {
                let cell = self.read_current_cell(col);
                f(row_idx, cell);
                col = col.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Soft-wrap flags for every viewport row, in row order.
    ///
    /// A separate pass rather than a field on [`Cell`]: the flags are
    /// row-level, so hanging them off every cell would repeat them
    /// `cols` times for the one consumer that reads them. Like
    /// [`Self::dirty_rows`] it rebinds the cached row iterator, so it
    /// must not be called from inside a `walk` / `walk_dirty` callback.
    pub fn row_wraps(&mut self, terminal: &Terminal) -> Result<Vec<RowWrap>> {
        self.bind_row_iterator()?;
        // Keep `terminal` alive across the walk (see `walk`).
        let _ = terminal;

        let mut wraps = Vec::new();
        // SAFETY: iter handle non-null.
        while unsafe { sys::ghostty_render_state_row_iterator_next(self.row_iter) } {
            wraps.push(self.read_row_wrap());
        }
        Ok(wraps)
    }

    /// Global dirty state. Pure read — clears nothing.
    pub fn dirty(&self) -> Result<Dirty> {
        let raw = self.read_u32(sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_DIRTY)?;
        Ok(Dirty::from_raw(raw))
    }

    /// Raise the global dirty state to `Full`, forcing the next
    /// `walk_dirty` to visit every row. Monotonic: this is the only
    /// public way to move the dirty state at all, and it only ever
    /// raises. Lowering lives solely inside `walk_dirty`, where both
    /// layers are cleared together — which is what keeps libghostty's
    /// two-layer footgun (clearing one layer does not clear the other)
    /// structurally unreachable from safe code.
    pub fn mark_full(&mut self) -> Result<()> {
        self.set_global_dirty(sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FULL)
    }

    /// Viewport row indices libghostty currently marks dirty. Pure with
    /// respect to dirty state — clears nothing, on either layer. It does
    /// rebind the cached row-iterator handle (`self.row_iter`) to walk the
    /// rows, so it must not be called from inside a `walk` / `walk_dirty`
    /// callback: that would re-anchor the iterator mid-iteration and
    /// corrupt the caller's in-flight walk. Diagnostic / test accessor;
    /// renderers want `walk_dirty`.
    pub fn dirty_rows(&mut self, terminal: &Terminal) -> Result<Vec<u32>> {
        self.bind_row_iterator()?;
        // Keep `terminal` alive across the walk (see `walk`).
        let _ = terminal;

        let mut rows = Vec::new();
        for row_idx in 0u32.. {
            // SAFETY: iter handle non-null.
            if !unsafe { sys::ghostty_render_state_row_iterator_next(self.row_iter) } {
                break;
            }
            if self.read_row_dirty() {
                rows.push(row_idx);
            }
        }
        Ok(rows)
    }

    /// Walk only the rows that changed, handing each row's complete cell
    /// list to `f`. Visits every row when the global state is `Full`.
    ///
    /// **Consumes the frame's damage: clears BOTH dirty layers.** A
    /// `Clean` frame visits nothing but still clears both, so the
    /// contract is uniform regardless of what came back.
    ///
    /// Contract: `f` is called exactly once per visited row, with that
    /// row's COMPLETE cell slice. It is never called mid-row, and never
    /// for a row whose read failed — so a caller may replace its cached
    /// row wholesale on each callback without a half-built row ever
    /// landing in the cache. A row whose cells could not be read keeps
    /// its dirty flag set, so the next frame retries it.
    ///
    /// Returns the global state as it was on ENTRY.
    ///
    /// On an early `Err` return some rows may already be cleared while
    /// the global layer is not. That is safe by construction: residual
    /// damage means the next frame redraws MORE than necessary, never
    /// less.
    pub fn walk_dirty(
        &mut self,
        terminal: &Terminal,
        mut f: impl FnMut(u32, &[Cell]),
    ) -> Result<Dirty> {
        let global = self.dirty()?;
        self.bind_row_iterator()?;
        // Keep `terminal` alive across the walk so libghostty doesn't
        // drop allocations that back the iterators (see `walk`).
        let _ = terminal;

        let mut cells: Vec<Cell> = Vec::new();
        let mut retry_pending = false;
        for row_idx in 0u32.. {
            // SAFETY: iter handle non-null.
            if !unsafe { sys::ghostty_render_state_row_iterator_next(self.row_iter) } {
                break;
            }

            let row_dirty = self.read_row_dirty();
            let visit = match global {
                Dirty::Clean => false,
                // `render.h` does not promise that a `Full` frame also
                // flags every row, so don't depend on it.
                Dirty::Full => true,
                Dirty::Partial => row_dirty,
            };

            if visit {
                if self.bind_row_cells().is_err() {
                    // Leave this row's flag SET so the next walk retries
                    // it, and don't call `f` with a partial row.
                    retry_pending = true;
                    continue;
                }

                cells.clear();
                let mut col: u16 = 0;
                // SAFETY: row_cells handle non-null.
                while unsafe { sys::ghostty_render_state_row_cells_next(self.row_cells) } {
                    cells.push(self.read_current_cell(col));
                    col = col.saturating_add(1);
                }
                f(row_idx, &cells);
            }

            if row_dirty {
                self.clear_row_dirty()?;
            }
        }

        // A row we could not read kept its flag, but clearing the global
        // layer to FALSE would strand it: the next `walk_dirty` reads
        // `Clean` and visits nothing at all, so the retry the contract
        // promises would never happen. Leave the frame `Partial` instead,
        // which is exactly what the surviving row flags mean.
        self.set_global_dirty(if retry_pending {
            sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_PARTIAL
        } else {
            sys::GhosttyRenderStateDirty_GHOSTTY_RENDER_STATE_DIRTY_FALSE
        })?;
        Ok(global)
    }

    /// Rebind `self.row_iter` to the current frame's state.
    ///
    /// The C signature expects `GhosttyRenderStateRowIterator*` (a
    /// pointer-to-handle slot), not the handle's value — the call writes
    /// into the slot to re-anchor the pre-allocated iterator at the new
    /// frame. Passing `self.row_iter as *mut _` would point at the
    /// iterator's IMPL and corrupt its internal state, leaving
    /// `..._next` returning false on every row (silent: no error, just
    /// zero cells walked). Mirrors
    /// `mac/Sources/Roost/RenderState.swift::walk`'s
    /// `withUnsafeMutablePointer(to: &self.rowIter)` pattern.
    fn bind_row_iterator(&mut self) -> Result<()> {
        // SAFETY: state + iter handles non-null per constructor.
        let rc = unsafe {
            sys::ghostty_render_state_get(
                self.handle,
                sys::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                (&mut self.row_iter) as *mut _ as *mut _,
            )
        };
        Error::from_result(rc)
    }

    /// Rebind `self.row_cells` to the row the iterator currently sits
    /// on. Same pointer-to-slot semantics as `bind_row_iterator`.
    fn bind_row_cells(&mut self) -> Result<()> {
        // SAFETY: iter + cells handles non-null per constructor.
        let rc = unsafe {
            sys::ghostty_render_state_row_get(
                self.row_iter,
                sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                (&mut self.row_cells) as *mut _ as *mut _,
            )
        };
        Error::from_result(rc)
    }

    /// Dirty flag of the row the iterator currently sits on. A failed
    /// read reports `false`, which leaves the flag alone rather than
    /// clearing damage we could not confirm.
    fn read_row_dirty(&self) -> bool {
        let mut out: bool = false;
        // SAFETY: iter handle non-null; out is a real local.
        let rc = unsafe {
            sys::ghostty_render_state_row_get(
                self.row_iter,
                sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                (&mut out) as *mut bool as *mut _,
            )
        };
        Error::from_result(rc).is_ok() && out
    }

    fn clear_row_dirty(&mut self) -> Result<()> {
        let value = false;
        // SAFETY: iter handle non-null; value is a real local.
        let rc = unsafe {
            sys::ghostty_render_state_row_set(
                self.row_iter,
                sys::GhosttyRenderStateRowOption_GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY,
                (&value) as *const bool as *const _,
            )
        };
        Error::from_result(rc)
    }

    fn set_global_dirty(&mut self, value: sys::GhosttyRenderStateDirty) -> Result<()> {
        // SAFETY: handle non-null; value is a real local.
        let rc = unsafe {
            sys::ghostty_render_state_set(
                self.handle,
                sys::GhosttyRenderStateOption_GHOSTTY_RENDER_STATE_OPTION_DIRTY,
                (&value) as *const _ as *const _,
            )
        };
        Error::from_result(rc)
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

        let style = self.read_cells_style();

        Cell {
            col,
            bg,
            fg,
            text,
            style,
            wide: self.read_cells_wide(),
        }
    }

    /// Double-width role of the cell the row-cells iterator is on.
    /// Read from the raw cell value rather than inferred from the
    /// grapheme's display width, so it can never disagree with the
    /// engine's own width table. Anything unreadable falls back to
    /// `Narrow`, which keeps the cell's text.
    fn read_cells_wide(&self) -> CellWide {
        let mut raw: sys::GhosttyCell = 0;
        // SAFETY: row_cells handle non-null; `raw` is a real local
        // matching the RAW datum's type.
        let rc = unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.row_cells,
                sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                (&mut raw) as *mut sys::GhosttyCell as *mut _,
            )
        };
        if Error::from_result(rc).is_err() {
            return CellWide::Narrow;
        }
        let mut wide: sys::GhosttyCellWide = 0;
        // SAFETY: `raw` is the cell value libghostty just handed back;
        // `wide` is a real local matching the WIDE datum's type.
        let rc = unsafe {
            sys::ghostty_cell_get(
                raw,
                sys::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE,
                (&mut wide) as *mut sys::GhosttyCellWide as *mut _,
            )
        };
        if Error::from_result(rc).is_err() {
            return CellWide::Narrow;
        }
        CellWide::from_raw(wide)
    }

    /// Soft-wrap flags of the row the iterator sits on. Anything
    /// unreadable reports "not wrapped", which leaves the row on its own
    /// line — copy loses a join it should have made, never joins two
    /// lines that were really separate.
    fn read_row_wrap(&self) -> RowWrap {
        let mut raw: sys::GhosttyRow = 0;
        // SAFETY: iter handle non-null; `raw` is a real local matching
        // the RAW datum's type.
        let rc = unsafe {
            sys::ghostty_render_state_row_get(
                self.row_iter,
                sys::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_RAW,
                (&mut raw) as *mut sys::GhosttyRow as *mut _,
            )
        };
        if Error::from_result(rc).is_err() {
            return RowWrap::default();
        }
        RowWrap {
            wrap: read_row_flag(raw, sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP),
            wrap_continuation: read_row_flag(
                raw,
                sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP_CONTINUATION,
            ),
        }
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

    fn read_cells_style(&self) -> Style {
        // `GhosttyStyle` is a sized C struct — `.size` MUST be initialized
        // to `sizeof(GhosttyStyle)` before the call so libghostty knows
        // which fields this caller is prepared to receive (forward-compat
        // contract per `ghostty/include/ghostty/vt/style.h`).
        //
        // The call returns `success` for every cell — even default-style
        // cells get a zeroed `GhosttyStyle` back — so we treat any
        // non-success as "no style data, use defaults" rather than
        // propagating an error: rendering a cell without styles is more
        // useful than failing the whole frame.
        let mut s = sys::GhosttyStyle {
            size: std::mem::size_of::<sys::GhosttyStyle>(),
            ..Default::default()
        };
        let rc = unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.row_cells,
                sys::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                (&mut s) as *mut _ as *mut _,
            )
        };
        if Error::from_result(rc).is_err() {
            return Style::default();
        }
        Style {
            bold: s.bold,
            italic: s.italic,
            inverse: s.inverse,
        }
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

/// Read one boolean row flag out of a raw `GhosttyRow` value.
fn read_row_flag(row: sys::GhosttyRow, data: sys::GhosttyRowData) -> bool {
    let mut out: bool = false;
    // SAFETY: `row` is the value libghostty just handed back; `out` is a
    // real local matching every boolean row datum's type.
    let rc = unsafe { sys::ghostty_row_get(row, data, (&mut out) as *mut bool as *mut _) };
    Error::from_result(rc).is_ok() && out
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

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn is_light_classifies_theme_backgrounds() {
        // roost-dark (#1e1e1e) and every other bundled theme background
        // are dark; white and near-white are light. The DEC 2031 report
        // picks its parameter off exactly this predicate.
        assert!(!ColorRgb::new(0x1e, 0x1e, 0x1e).is_light());
        assert!(!ColorRgb::new(0x00, 0x00, 0x00).is_light());
        assert!(ColorRgb::new(0xff, 0xff, 0xff).is_light());
        assert!(ColorRgb::new(0xfa, 0xfa, 0xfa).is_light());
        // Green weighs heaviest (0.7152) — a saturated green reads light,
        // a saturated blue (0.0722) reads dark.
        assert!(ColorRgb::new(0x00, 0xff, 0x00).is_light());
        assert!(!ColorRgb::new(0x00, 0x00, 0xff).is_light());
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

    /// Inverse-bit readback regression. Pre-fix, `Cell` had no `style`
    /// field at all, so the renderer dropped `\e[7m` entirely and the
    /// gray prompt row of TUI agents (codex, others) rendered against
    /// the default canvas background instead of the inverted swap.
    /// Verifies the bit survives the round trip libghostty → walk
    /// callback for the inverse-marked cell *and* is clear on the
    /// post-reset cell that follows.
    #[test]
    fn walk_reads_style_bits_for_inverse_cells() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .expect("Terminal::new");
        // CSI 7m = inverse on, CSI 1m = bold on (combined), reset, then Y.
        terminal.vt_write(b"\x1b[1;7mX\x1b[0mY");

        let mut render_state = RenderState::new().expect("RenderState::new");
        render_state.update(&terminal).expect("update");

        let mut row0: Vec<(u16, String, Style)> = Vec::new();
        render_state
            .walk(&terminal, |row, cell| {
                if row == 0 && !cell.text.is_empty() && cell.text != " " {
                    row0.push((cell.col, cell.text.clone(), cell.style));
                }
            })
            .expect("walk");

        let x_cell = row0
            .iter()
            .find(|(_, t, _)| t == "X")
            .expect("X cell missing");
        assert!(
            x_cell.2.inverse,
            "X cell should carry inverse=true after \\e[1;7m, got {:?}",
            x_cell.2
        );
        assert!(
            x_cell.2.bold,
            "X cell should carry bold=true after \\e[1;7m, got {:?}",
            x_cell.2
        );

        let y_cell = row0
            .iter()
            .find(|(_, t, _)| t == "Y")
            .expect("Y cell missing");
        assert!(
            !y_cell.2.inverse,
            "Y cell (post-reset) must not carry inverse, got {:?}",
            y_cell.2
        );
        assert!(
            !y_cell.2.bold,
            "Y cell (post-reset) must not carry bold, got {:?}",
            y_cell.2
        );
    }
}
