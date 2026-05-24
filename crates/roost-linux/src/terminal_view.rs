//! Cell renderer over libghostty-vt — Phase 7 commit 4.
//!
//! Owns a [`roost_vt::Terminal`] + a [`roost_vt::RenderState`] and
//! paints into a [`gtk::DrawingArea`]'s Cairo context. Two-pass walk
//! mirroring the Go binary's `cmd/roost/render.go`:
//!   1. Pass A: background fills (canvas + per-cell bg).
//!   2. Pass B: glyphs via Pango layout (one layout reused per frame,
//!      `set_text` per cell).
//!   3. Pass C: cursor draw — 4 styles (block / bar / underline /
//!      hollow), focus-aware. Per `feature/rust-port` commit
//!      `266dea7` we deliberately keep the cursor visible whenever
//!      the view has focus, regardless of libghostty's DECTCEM
//!      `visible` flag. This is the cmux-style UX the user
//!      requested; matches the Mac UI's TerminalView.draw decision.
//!   4. Pass D (later): selection overlay. Lands in commit 7.
//!
//! Subsequent commits add: PTY round-trip (5), key input (6),
//! scrollback + selection + clipboard (7), full theme + config (11).
//! This commit hard-codes a roost-dark palette + JetBrains Mono fall-
//! through to Monospace + a static `vt_write` "hello" payload so we
//! can eyeball the renderer side-by-side with the Go binary on Mac
//! Homebrew GTK.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::cairo;
use gtk4::glib;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{
    DrawingArea, EventControllerFocus, EventControllerKey, EventControllerScroll,
    EventControllerScrollFlags, GestureDrag,
};
use pangocairo::functions as pango_cairo;

use roost_vt::{
    ActiveScreen, Cell, ColorRgb, CursorInfo, CursorVisualStyle, KeyEncoder, RenderState,
    ScrollViewport, Terminal, TerminalOptions,
};

use crate::cell_metrics::{default_font_description, CellMetrics};
use crate::key_encoder;
use crate::theme::Theme;

/// Default cell grid the terminal allocates with. Cell pixels are
/// reported to libghostty so its OSC 14 / size-report responses are
/// accurate; the grid is reflowed per-resize in commit 5 onwards.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Cursor blink half-period. 530ms matches the Mac UI + Go binary.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// Light-weight handle into a [`TerminalView`] that can be cloned
/// into closures. Useful for keystroke handlers that need to invoke
/// the view's copy / paste public methods without re-borrowing the
/// outer Rc.
#[derive(Clone)]
struct TerminalViewHandle {
    state: Rc<RefCell<TerminalViewState>>,
    widget: DrawingArea,
}

impl TerminalViewHandle {
    fn copy(&self) {
        // Inline copy_selection_to_clipboard to avoid an Rc<TerminalView>
        // captured in the closure (TerminalView itself is not Clone).
        let view = TerminalView {
            widget: self.widget.clone(),
            state: self.state.clone(),
        };
        view.copy_selection_to_clipboard();
        // Forget the shallow `TerminalView` we just constructed —
        // the underlying widget + state are reference-counted.
        std::mem::forget(view);
    }

    fn paste(&self) {
        let view = TerminalView {
            widget: self.widget.clone(),
            state: self.state.clone(),
        };
        view.paste_from_clipboard();
        std::mem::forget(view);
    }
}

/// Owned widget + state for one terminal view. Wraps a
/// [`gtk::DrawingArea`] so callers can drop it into any GTK container.
pub struct TerminalView {
    widget: DrawingArea,
    /// Shared with the draw closure + the blink timer. RefCell-borrow
    /// is fine because every access happens on the GTK main thread
    /// (the !Sync invariant of `roost_vt::Terminal` enforces this at
    /// the type level).
    state: Rc<RefCell<TerminalViewState>>,
}

impl TerminalView {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    /// Construct with a custom theme + optional font overrides.
    /// Phase 7 commit 11: the App passes user-supplied
    /// `font_family` + `font_size_pt` from `~/.config/roost/config.conf`
    /// when present, falling back to the JetBrains Mono / 13pt
    /// defaults otherwise. The theme's palette is pushed into
    /// libghostty so SGR cells (`ls --color`, `git diff`) flip to the
    /// theme's reds / greens / etc.
    pub fn with_theme_and_font(
        theme: Theme,
        font_family: Option<&str>,
        font_size_pt: Option<f64>,
    ) -> Self {
        let view = Self::with_theme(theme);
        view.apply_font(font_family, font_size_pt);
        view
    }

    pub fn with_theme(theme: Theme) -> Self {
        let widget = DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .can_focus(true)
            .build();

        let terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            // 2000 rows of off-screen history matches both the Go
            // binary (`cmd/roost/session.go`) and the Mac UI's M6
            // scrollback. Enough for a `seq 1 5000 | less` session
            // without growing memory.
            max_scrollback: 2000,
        })
        .expect("allocate libghostty-vt terminal");

        let render_state = RenderState::new().expect("allocate libghostty-vt render state");
        let encoder = KeyEncoder::new().expect("allocate libghostty-vt key encoder");
        // Push the theme's palette + chrome colors into libghostty so
        // SGR cells (`ls --color`, `git diff`, htop, etc.) flip to
        // the theme's reds / greens / etc. Failures are non-fatal:
        // the renderer falls back to libghostty's compiled-in palette
        // plus the theme.background canvas fill in the draw pass.
        let mut terminal = terminal;
        let _ = terminal.set_color_background(theme.background);
        let _ = terminal.set_color_foreground(theme.foreground);
        let _ = terminal.set_color_cursor(theme.cursor);
        let _ = terminal.set_color_palette(&theme.palette);

        let pango_ctx = widget.pango_context();
        let font_desc = default_font_description();
        // Hinting + antialias on so monospace cells align to whole
        // pixels. gtk4-rs's `pangocairo` binding exposes
        // `context_set_font_options` directly via raw FFI, so the
        // gotk4 `pangoextra` workaround (DL-6) doesn't apply.
        let mut font_options = cairo::FontOptions::new().expect("alloc cairo::FontOptions");
        font_options.set_antialias(cairo::Antialias::Gray);
        font_options.set_hint_metrics(cairo::HintMetrics::On);
        font_options.set_hint_style(cairo::HintStyle::Slight);
        pango_cairo::context_set_font_options(&pango_ctx, Some(&font_options));
        let metrics = CellMetrics::measure(&pango_ctx, &font_desc);

        let state = Rc::new(RefCell::new(TerminalViewState {
            terminal,
            render_state,
            encoder,
            theme,
            font_desc,
            cell_metrics: metrics,
            cursor_blink_on: true,
            has_focus: true,
            scrolled_back: false,
            scroll_accum: 0.0,
            selection: None,
            input_callback: None,
        }));

        // Draw function: hand a Cairo context per redraw.
        widget.set_draw_func({
            let state = state.clone();
            move |widget, cr, width, height| {
                let mut s = state.borrow_mut();
                s.paint(widget, cr, width as f64, height as f64);
            }
        });

        // Focus tracking: drives the cursor "hollow vs filled"
        // distinction in `paint_cursor`.
        let focus_ctrl = EventControllerFocus::new();
        focus_ctrl.connect_enter({
            let state = state.clone();
            let widget = widget.clone();
            move |_| {
                state.borrow_mut().has_focus = true;
                state.borrow_mut().cursor_blink_on = true;
                widget.queue_draw();
            }
        });
        focus_ctrl.connect_leave({
            let state = state.clone();
            let widget = widget.clone();
            move |_| {
                state.borrow_mut().has_focus = false;
                widget.queue_draw();
            }
        });
        widget.add_controller(focus_ctrl);

        // Scroll wheel: 3 modes per the Mac UI / Go binary. Discrete
        // notches + smooth scroll both go through the same path; the
        // smooth-scroll accumulator handles trackpad fractional rows.
        let scroll_ctrl = EventControllerScroll::new(
            EventControllerScrollFlags::VERTICAL | EventControllerScrollFlags::DISCRETE,
        );
        scroll_ctrl.connect_scroll({
            let state = state.clone();
            let widget = widget.clone();
            move |_ctrl, _dx, dy| {
                let mut s = state.borrow_mut();
                s.handle_scroll(dy);
                drop(s);
                widget.queue_draw();
                glib::Propagation::Stop
            }
        });
        widget.add_controller(scroll_ctrl);

        // Drag selection. Anchor on press, update on drag, on release
        // the selection becomes "committed" until the user clicks
        // elsewhere or types (commit 7+'s `clearSelection` flow).
        let drag = GestureDrag::new();
        drag.connect_drag_begin({
            let state = state.clone();
            let widget = widget.clone();
            move |_g, x, y| {
                let mut s = state.borrow_mut();
                if let Some((col, row)) = s.cell_at(x, y, &widget) {
                    s.selection = Some(Selection {
                        anchor_col: col,
                        anchor_row: row,
                        cursor_col: col,
                        cursor_row: row,
                    });
                }
                drop(s);
                widget.queue_draw();
            }
        });
        drag.connect_drag_update({
            let state = state.clone();
            let widget = widget.clone();
            move |g, dx, dy| {
                let mut s = state.borrow_mut();
                if let Some((start_x, start_y)) = g.start_point() {
                    let x = start_x + dx;
                    let y = start_y + dy;
                    let cell_w = s.cell_metrics.cell_width;
                    let cell_h = s.cell_metrics.cell_height;
                    if let Some(sel) = s.selection.as_mut() {
                        if let Some((col, row)) = cell_at_inner(x, y, cell_w, cell_h) {
                            sel.cursor_col = col;
                            sel.cursor_row = row;
                        }
                    }
                }
                drop(s);
                widget.queue_draw();
            }
        });
        widget.add_controller(drag);

        // Cursor blink: toggle every 530ms while the widget exists.
        // Pausing the timer on focus loss is a polish nit deferred to
        // commit 7 — for now we just stop redrawing the cursor when
        // unfocused (the hollow outline doesn't blink).
        glib::timeout_add_local(CURSOR_BLINK_INTERVAL, {
            let state = state.clone();
            let widget = widget.clone();
            move || {
                let mut s = state.borrow_mut();
                if s.has_focus {
                    s.cursor_blink_on = !s.cursor_blink_on;
                    drop(s);
                    widget.queue_draw();
                }
                glib::ControlFlow::Continue
            }
        });

        Self { widget, state }
    }

    /// The underlying widget — drop into any GTK container.
    pub fn widget(&self) -> &DrawingArea {
        &self.widget
    }

    /// Replace the font description + remeasure cell metrics. Used
    /// by `with_theme_and_font` to honor `~/.config/roost/config.conf`
    /// `font-family` / `font-size` settings. Triggers a redraw.
    pub fn apply_font(&self, family: Option<&str>, size_pt: Option<f64>) {
        if family.is_none() && size_pt.is_none() {
            return;
        }
        let mut s = self.state.borrow_mut();
        let mut desc = s.font_desc.clone();
        if let Some(family) = family {
            desc.set_family(family);
        }
        if let Some(pt) = size_pt {
            desc.set_absolute_size(pt * gtk4::pango::SCALE as f64 * 96.0 / 72.0);
        }
        s.font_desc = desc.clone();
        let pango_ctx = self.widget.pango_context();
        s.cell_metrics = CellMetrics::measure(&pango_ctx, &desc);
        drop(s);
        self.widget.queue_draw();
    }

    /// Swap the live theme on this terminal. Re-pushes the palette +
    /// chrome colors into libghostty (the same calls `with_theme` makes
    /// at creation, safe to re-call at runtime) so SGR-indexed cells
    /// (`ls --color`, htop) recolor, stores the new theme so the paint
    /// pass picks up the default fg/bg/cursor/selection, and queues a
    /// redraw. Drives the command palette's live theme preview.
    pub fn set_theme(&self, theme: &Theme) {
        {
            let mut s = self.state.borrow_mut();
            let _ = s.terminal.set_color_background(theme.background);
            let _ = s.terminal.set_color_foreground(theme.foreground);
            let _ = s.terminal.set_color_cursor(theme.cursor);
            let _ = s.terminal.set_color_palette(&theme.palette);
            s.theme = theme.clone();
        }
        self.widget.queue_draw();
    }

    /// Feed VT bytes into the terminal. Triggers a redraw so the new
    /// state is on screen by the next frame.
    pub fn vt_write(&self, bytes: &[u8]) {
        {
            let mut s = self.state.borrow_mut();
            s.terminal.vt_write(bytes);
        }
        self.widget.queue_draw();
    }

    /// Copy the current selection to the system clipboard. Walks the
    /// libghostty render state, extracts plain text for the selected
    /// cell range, and pushes to `gdk::Display::clipboard`. No-op if
    /// the selection is empty.
    pub fn copy_selection_to_clipboard(&self) {
        let s = self.state.borrow();
        let Some(sel) = s.selection else {
            return;
        };
        if sel.is_empty() {
            return;
        }
        let (sc, sr, ec, er) = sel.normalized();
        drop(s);

        let mut text = String::new();
        let mut s = self.state.borrow_mut();
        // Split-borrow so the walk can hold `&mut render_state` and
        // `&terminal` simultaneously — both fields of `s` but
        // Rust's borrow checker can't see through the outer
        // `&mut TerminalViewState` without destructuring.
        let TerminalViewState {
            terminal,
            render_state,
            ..
        } = &mut *s;
        let _ = render_state.update(terminal);
        let _ = render_state.walk(terminal, |row, cell| {
            if row < sr as u32 || row >= er as u32 {
                return;
            }
            let col = cell.col;
            let in_range = if row == sr as u32 && row == er.saturating_sub(1) as u32 {
                col >= sc && col < ec
            } else if row == sr as u32 {
                col >= sc
            } else if row == er.saturating_sub(1) as u32 {
                col < ec
            } else {
                true
            };
            if !in_range {
                return;
            }
            // Row boundary: when col rolls back to 0 on a new row,
            // append a newline. (`row` advances per-row, so we just
            // detect row breaks by tracking the prior row.)
            if col == 0 && !text.is_empty() {
                text.push('\n');
            }
            if cell.text.is_empty() {
                text.push(' ');
            } else {
                text.push_str(&cell.text);
            }
        });
        drop(s);
        // Trim trailing whitespace on each line for a cleaner paste.
        let trimmed = text
            .lines()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        if !trimmed.is_empty() {
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&trimmed);
            }
        }
    }

    /// Read text from the system clipboard and feed it into the PTY.
    /// Wraps the payload in bracketed-paste escapes (`ESC[200~` …
    /// `ESC[201~`) when the terminal has DECSET 2004 active (zsh,
    /// bash with the bracketed-paste plugin, vim insert mode, etc.).
    /// Async because the clipboard read is async; spawn on the GTK
    /// main loop via `glib::spawn_future_local`.
    pub fn paste_from_clipboard(&self) {
        let Some(display) = gtk4::gdk::Display::default() else {
            return;
        };
        let clipboard = display.clipboard();
        let state = self.state.clone();
        glib::spawn_future_local(async move {
            let Ok(text) = clipboard.read_text_future().await else {
                return;
            };
            let Some(text) = text else { return };
            let s = state.borrow();
            let bracketed = s.terminal.mode_get(2004);
            let bytes = if bracketed {
                let mut buf = Vec::with_capacity(text.len() + 8);
                buf.extend_from_slice(b"\x1b[200~");
                buf.extend_from_slice(text.as_bytes());
                buf.extend_from_slice(b"\x1b[201~");
                buf
            } else {
                text.as_bytes().to_vec()
            };
            if let Some(cb) = s.input_callback.as_ref() {
                cb(bytes);
            }
        });
    }

    /// Install a keystroke handler. Called with raw UTF-8 bytes when
    /// the view has focus and the user types a printable character.
    /// Bare-minimum bridge for commit 5 — the full key encoder (arrow
    /// keys, function keys, Shift+Tab, IME, etc.) lands in commit 6.
    pub fn set_on_input<F>(&self, callback: F)
    where
        F: Fn(Vec<u8>) + 'static,
    {
        // Lazily attach the EventControllerKey on first set. We rebind
        // the held callback to support replacing the handler when a
        // tab is closed + reopened.
        let mut s = self.state.borrow_mut();
        let already_attached = s.input_callback.is_some();
        s.input_callback = Some(Box::new(callback));
        drop(s);
        if already_attached {
            return;
        }

        let key_ctrl = EventControllerKey::new();
        key_ctrl.connect_key_pressed({
            let state = self.state.clone();
            let widget = self.widget.clone();
            let view_handle = TerminalViewHandle {
                state: self.state.clone(),
                widget: self.widget.clone(),
            };
            move |_ctrl, key, _keycode, mods| {
                // Commit 7 stop-gap: Ctrl+Shift+C / Ctrl+Shift+V invoke
                // copy/paste. Full keybind table (with config-file
                // overrides + Mac-style ⌘C / ⌘V on Mac) lands in
                // commit 9.
                if mods.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
                    && mods.contains(gtk4::gdk::ModifierType::SHIFT_MASK)
                {
                    use gtk4::gdk::Key as K;
                    match key {
                        K::c | K::C => {
                            view_handle.copy();
                            return glib::Propagation::Stop;
                        }
                        K::v | K::V => {
                            view_handle.paste();
                            return glib::Propagation::Stop;
                        }
                        _ => {}
                    }
                }
                // Phase 7 commit 6: route through `roost_vt::KeyEncoder`
                // (the safe wrapper landed in commit 1). The encoder
                // handles modifier conventions, Kitty keyboard
                // protocol, DECCKM application-mode arrows, etc.
                let mut s = state.borrow_mut();
                // Commit 7: snap viewport to bottom on any keystroke
                // (matches the Go binary's `cmd/roost/input.go:67`).
                // Clear any active selection — typing intent overrides.
                if s.scrolled_back {
                    s.terminal.scroll_viewport(ScrollViewport::Bottom);
                    s.scrolled_back = false;
                    s.scroll_accum = 0.0;
                    widget.queue_draw();
                }
                if s.selection.is_some() {
                    s.selection = None;
                    widget.queue_draw();
                }
                let bytes = {
                    let s_mut: &mut TerminalViewState = &mut s;
                    key_encoder::encode_key(&mut s_mut.encoder, &s_mut.terminal, key, mods)
                };
                if bytes.is_empty() {
                    return glib::Propagation::Proceed;
                }
                if let Some(cb) = s.input_callback.as_ref() {
                    cb(bytes);
                }
                glib::Propagation::Stop
            }
        });
        self.widget.add_controller(key_ctrl);
    }
}

impl Default for TerminalView {
    fn default() -> Self {
        Self::new()
    }
}

struct TerminalViewState {
    terminal: Terminal,
    render_state: RenderState,
    /// Reused across keystrokes; an internal scratch buffer keeps
    /// per-keystroke allocation amortized to zero in the steady state.
    encoder: KeyEncoder,
    theme: Theme,
    font_desc: pango::FontDescription,
    cell_metrics: CellMetrics,
    cursor_blink_on: bool,
    has_focus: bool,
    /// True while the viewport has been scrolled back into history.
    /// Cleared the moment we scroll back to bottom (either via a
    /// keystroke snap or by the wheel reaching the active region).
    /// The Go binary tracks this in `cmd/roost/session.go` to decide
    /// whether to snap before encoding a key.
    scrolled_back: bool,
    /// Smooth-scroll accumulator. Trackpad / Magic Mouse deltas are
    /// fractional rows; we accumulate until we have a whole row,
    /// then dispatch. Discrete wheels usually report 1.0+ per notch
    /// so the accumulator passes through.
    scroll_accum: f64,
    /// Current drag selection, in (col, row) viewport coordinates.
    /// `None` outside an active drag.
    selection: Option<Selection>,
    /// Caller-installed keystroke handler. Optional because the
    /// TerminalView can be built before its session is spawned;
    /// `set_on_input` populates it once the daemon round-trip is
    /// ready.
    input_callback: Option<Box<dyn Fn(Vec<u8>)>>,
}

/// Drag-selection state. Anchor = where the mouse-down landed,
/// cursor = the current pointer cell.
#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor_col: u16,
    anchor_row: u16,
    cursor_col: u16,
    cursor_row: u16,
}

impl Selection {
    fn is_empty(&self) -> bool {
        self.anchor_col == self.cursor_col && self.anchor_row == self.cursor_row
    }

    /// Normalized (start_col, start_row, end_col, end_row) with
    /// start <= end in row-major order. Inclusive on start, exclusive
    /// on end, mirroring the Mac UI's `CellSelection.normalized`.
    fn normalized(&self) -> (u16, u16, u16, u16) {
        let (sc, sr, ec, er) =
            if (self.anchor_row, self.anchor_col) <= (self.cursor_row, self.cursor_col) {
                (
                    self.anchor_col,
                    self.anchor_row,
                    self.cursor_col,
                    self.cursor_row,
                )
            } else {
                (
                    self.cursor_col,
                    self.cursor_row,
                    self.anchor_col,
                    self.anchor_row,
                )
            };
        (sc, sr, ec, er.saturating_add(1))
    }
}

impl TerminalViewState {
    fn paint(&mut self, widget: &DrawingArea, cr: &cairo::Context, width: f64, height: f64) {
        // Snapshot terminal state for this frame.
        if let Err(err) = self.render_state.update(&self.terminal) {
            tracing::warn!(?err, "render_state.update failed; skipping frame");
            return;
        }
        let colors = self.render_state.colors().unwrap_or(roost_vt::Colors {
            foreground: self.theme.foreground,
            background: self.theme.background,
            cursor: None,
        });
        // Theme wins over libghostty's compiled-in default when no
        // SGR override is set on the cell. M6 P3 on the Mac side
        // pushes the theme into libghostty so `colors.foreground/
        // background` already carries the theme — until commit 11
        // does the same here, fall back to the theme directly.
        let default_fg = self.theme.foreground;
        let default_bg = self.theme.background;

        // Pass A: canvas + per-cell backgrounds.
        set_cairo_color(cr, default_bg);
        let _ = cr.paint();

        let cell_w = self.cell_metrics.cell_width;
        let cell_h = self.cell_metrics.cell_height;

        // Pass A — per-cell bg fills only for cells that override
        // the default. (A cell carrying the default bg is already
        // painted by the canvas fill above.)
        let mut bg_pass: Vec<(u32, u16, ColorRgb)> = Vec::new();
        // Pass B — per-cell glyphs.
        let mut glyph_pass: Vec<(u32, u16, ColorRgb, String)> = Vec::new();
        // Cursor cell glyph + override fg — captured during the walk
        // so the block cursor can re-draw the underlying glyph in
        // inverted color in pass C.
        let cursor = self.render_state.cursor();
        let mut cursor_cell_text: Option<(String, ColorRgb)> = None;

        self.render_state
            .walk(&self.terminal, |row, cell: Cell| {
                let bg = cell.bg.unwrap_or(default_bg);
                let fg = cell.fg.unwrap_or(default_fg);
                if cell.bg.is_some() && bg != default_bg {
                    bg_pass.push((row, cell.col, bg));
                }
                if !cell.text.is_empty() && cell.text != " " {
                    glyph_pass.push((row, cell.col, fg, cell.text.clone()));
                }
                if let Some(c) = cursor.as_ref() {
                    if c.row == row && c.col == cell.col as u32 {
                        cursor_cell_text = Some((cell.text.clone(), fg));
                    }
                }
            })
            .ok();

        for (row, col, bg) in &bg_pass {
            set_cairo_color(cr, *bg);
            cr.rectangle(*col as f64 * cell_w, *row as f64 * cell_h, cell_w, cell_h);
            let _ = cr.fill();
        }

        // Pass B: glyphs via Pango.
        let pango_ctx = widget.pango_context();
        let layout = pango::Layout::new(&pango_ctx);
        layout.set_font_description(Some(&self.font_desc));
        for (row, col, fg, text) in &glyph_pass {
            // Skip drawing the glyph at the cursor cell when the
            // cursor's about to redraw it inverted (block cursor).
            if let Some(c) = cursor.as_ref() {
                if c.row == *row && c.col == *col as u32 && self.should_invert_cursor_glyph() {
                    continue;
                }
            }
            set_cairo_color(cr, *fg);
            layout.set_text(text);
            cr.move_to(*col as f64 * cell_w, *row as f64 * cell_h);
            pango_cairo::show_layout(cr, &layout);
        }

        // Pass C: cursor.
        if let Some(c) = cursor.as_ref() {
            self.paint_cursor(
                cr,
                &layout,
                c,
                cursor_cell_text.as_ref(),
                cell_w,
                cell_h,
                colors.cursor.unwrap_or(self.theme.cursor),
                default_bg,
            );
        }

        // Pass D: selection overlay. Translucent fill so cell glyphs
        // and the cursor stay legible underneath. Same shape as the
        // Mac UI's `TerminalView.draw` selection draw.
        if let Some(sel) = self.selection {
            if !sel.is_empty() {
                self.paint_selection(cr, sel, cell_w, cell_h);
            }
        }

        let _ = (width, height);
    }

    fn paint_selection(&self, cr: &cairo::Context, sel: Selection, cell_w: f64, cell_h: f64) {
        let (sc, sr, ec, er) = sel.normalized();
        let (r, g, b) = self.theme.selection_background.to_f64();
        cr.set_source_rgba(r, g, b, 0.35);
        if sr == er.saturating_sub(1) {
            // Single-row selection: one rect from sc..ec.
            cr.rectangle(
                sc as f64 * cell_w,
                sr as f64 * cell_h,
                (ec.saturating_sub(sc)) as f64 * cell_w,
                cell_h,
            );
            let _ = cr.fill();
            return;
        }
        // Multi-row: head from sc → end-of-row, middle full rows,
        // tail 0 → ec. Matches the Mac UI's `ribbonRects()`.
        cr.rectangle(
            sc as f64 * cell_w,
            sr as f64 * cell_h,
            ((DEFAULT_COLS as f64) - sc as f64) * cell_w,
            cell_h,
        );
        let _ = cr.fill();
        if er.saturating_sub(sr) > 1 {
            cr.set_source_rgba(r, g, b, 0.35);
            cr.rectangle(
                0.0,
                (sr + 1) as f64 * cell_h,
                DEFAULT_COLS as f64 * cell_w,
                (er.saturating_sub(sr).saturating_sub(1)) as f64 * cell_h,
            );
            let _ = cr.fill();
        }
        cr.set_source_rgba(r, g, b, 0.35);
        cr.rectangle(
            0.0,
            (er.saturating_sub(1)) as f64 * cell_h,
            ec as f64 * cell_w,
            cell_h,
        );
        let _ = cr.fill();
    }

    /// Handle a single scroll-wheel `dy`. Negative = up (older
    /// history). 3 modes per the Go binary `cmd/roost/session.go`:
    ///   * Mouse-tracking (DECSET 1000/1002/1003) — defer; commit 7
    ///     of this plan doesn't enable mouse-tracking encode yet.
    ///   * Alt-screen — translate to ArrowUp / ArrowDown via the key
    ///     encoder. Lets vim / less consume the wheel.
    ///   * Primary screen — local viewport scroll via
    ///     `Terminal::scroll_viewport(Delta)`.
    fn handle_scroll(&mut self, dy: f64) {
        // Smooth-scroll accumulator. Trackpad deltas are typically
        // fractional rows; discrete wheels are integers.
        self.scroll_accum += dy;
        // 3 rows per discrete notch matches the Mac UI; for smooth
        // scroll we step one row at a time so the animation isn't
        // jumpy.
        let rows_to_scroll = if self.scroll_accum.abs() >= 1.0 {
            let rows = self.scroll_accum.trunc() as isize;
            self.scroll_accum -= rows as f64;
            rows
        } else {
            return;
        };

        if self.terminal.active_screen() == ActiveScreen::Alternate {
            // Translate to arrow keys for alt-screen apps.
            let key = if rows_to_scroll < 0 {
                roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_UP
            } else {
                roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_DOWN
            };
            let mut event = match roost_vt::KeyEvent::new() {
                Ok(ev) => ev,
                Err(_) => return,
            };
            event.set_action(roost_vt::key_action::PRESS);
            event.set_key(key);
            event.set_mods(0);
            self.encoder.sync_from_terminal(&self.terminal);
            for _ in 0..rows_to_scroll.unsigned_abs() {
                if let Ok(bytes) = self.encoder.encode(&event) {
                    if let Some(cb) = self.input_callback.as_ref() {
                        cb(bytes);
                    }
                }
            }
            return;
        }

        // Primary screen: local scrollback. Negative dy = scroll up
        // (older history). libghostty's Delta semantics use negative
        // for up.
        self.terminal
            .scroll_viewport(ScrollViewport::Delta(-rows_to_scroll));
        // Track whether we're scrolled back. A positive scroll-down
        // that lands us at the active region clears the flag.
        if rows_to_scroll < 0 {
            self.scrolled_back = true;
        } else if rows_to_scroll > 0 && self.scrolled_back {
            // Heuristic: if we scrolled down by the request, assume
            // we're back at bottom. A more precise check would call
            // a render-state getter for the viewport offset; deferred
            // since the keystroke snap is the primary "back-to-bottom"
            // path.
            self.scrolled_back = false;
        }
    }

    /// Convert widget-pixel `(x, y)` to a (col, row) cell pair,
    /// clamping to the visible viewport. Returns None for points
    /// outside the rendered region.
    fn cell_at(&self, x: f64, y: f64, widget: &DrawingArea) -> Option<(u16, u16)> {
        let w = widget.width() as f64;
        let h = widget.height() as f64;
        if x < 0.0 || y < 0.0 || x > w || y > h {
            return None;
        }
        cell_at_inner(
            x,
            y,
            self.cell_metrics.cell_width,
            self.cell_metrics.cell_height,
        )
    }

    /// Decide whether to skip the per-cell glyph at the cursor and
    /// let the cursor's own re-draw paint it inverted. True only for
    /// the focused block cursor in the "on" phase of the blink.
    fn should_invert_cursor_glyph(&self) -> bool {
        if !self.has_focus || !self.cursor_blink_on {
            return false;
        }
        match self.render_state.cursor() {
            Some(c) => matches!(c.visual_style, CursorVisualStyle::Block),
            None => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_cursor(
        &self,
        cr: &cairo::Context,
        layout: &pango::Layout,
        cursor: &CursorInfo,
        cursor_cell_text: Option<&(String, ColorRgb)>,
        cell_w: f64,
        cell_h: f64,
        cursor_color: ColorRgb,
        canvas_bg: ColorRgb,
    ) {
        // cmux-style: keep the cursor visible whenever the view has
        // focus, regardless of libghostty's DECTCEM `visible` flag.
        // Matches the Mac UI's TerminalView.draw decision merged from
        // feature/rust-port commit 266dea7.
        let should_draw = self.has_focus || cursor.visible;
        if !should_draw || cursor.wide_tail {
            return;
        }

        let x = cursor.col as f64 * cell_w;
        let y = cursor.row as f64 * cell_h;

        if !self.has_focus {
            // Hollow outline — 1pt stroke inset 0.5pt so the stroke
            // sits inside the cell rect.
            set_cairo_color(cr, cursor_color);
            cr.set_line_width(1.0);
            cr.rectangle(x + 0.5, y + 0.5, cell_w - 1.0, cell_h - 1.0);
            let _ = cr.stroke();
            return;
        }

        // Blink "off" phase: skip the cursor draw so the underlying
        // glyph shows through. The next blink tick toggles it back.
        if !self.cursor_blink_on {
            return;
        }

        match cursor.visual_style {
            CursorVisualStyle::Bar => {
                // 2pt vertical line at the cell's leading edge.
                set_cairo_color(cr, cursor_color);
                cr.rectangle(x, y, 2.0, cell_h);
                let _ = cr.fill();
            }
            CursorVisualStyle::Underline => {
                // 2pt horizontal line at the cell's bottom edge.
                set_cairo_color(cr, cursor_color);
                cr.rectangle(x, y + cell_h - 2.0, cell_w, 2.0);
                let _ = cr.fill();
            }
            CursorVisualStyle::BlockHollow => {
                set_cairo_color(cr, cursor_color);
                cr.set_line_width(1.0);
                cr.rectangle(x + 0.5, y + 0.5, cell_w - 1.0, cell_h - 1.0);
                let _ = cr.stroke();
            }
            CursorVisualStyle::Block => {
                // Fill the cell with cursor color, then redraw the
                // underlying glyph in canvas-bg color so it's
                // legible against the filled block.
                set_cairo_color(cr, cursor_color);
                cr.rectangle(x, y, cell_w, cell_h);
                let _ = cr.fill();

                if let Some((text, _fg)) = cursor_cell_text {
                    if !text.is_empty() && text != " " {
                        set_cairo_color(cr, canvas_bg);
                        layout.set_text(text);
                        cr.move_to(x, y);
                        pango_cairo::show_layout(cr, layout);
                    }
                }
            }
        }
    }
}

/// Set the Cairo source color from an `roost_vt::ColorRgb`.
fn set_cairo_color(cr: &cairo::Context, rgb: ColorRgb) {
    let (r, g, b) = rgb.to_f64();
    cr.set_source_rgb(r, g, b);
}

/// Pure-function variant of `TerminalViewState::cell_at` for use from
/// closures that don't have a Borrow-able reference to the widget.
fn cell_at_inner(x: f64, y: f64, cell_w: f64, cell_h: f64) -> Option<(u16, u16)> {
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }
    let col = (x / cell_w).floor().max(0.0) as u16;
    let row = (y / cell_h).floor().max(0.0) as u16;
    Some((col, row))
}
