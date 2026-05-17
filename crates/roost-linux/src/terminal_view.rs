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
use gtk4::gdk;
use gtk4::glib;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{DrawingArea, EventControllerFocus, EventControllerKey};
use pangocairo::functions as pango_cairo;

use roost_vt::{
    Cell, ColorRgb, CursorInfo, CursorVisualStyle, RenderState, Terminal, TerminalOptions,
};

use crate::cell_metrics::{default_font_description, CellMetrics};
use crate::theme::Theme;

/// Default cell grid the terminal allocates with. Cell pixels are
/// reported to libghostty so its OSC 14 / size-report responses are
/// accurate; the grid is reflowed per-resize in commit 5 onwards.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Cursor blink half-period. 530ms matches the Mac UI + Go binary.
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

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
            // Phase 7 commit 7 will bump this to 2000 with the
            // scrollback wheel handler.
            max_scrollback: 0,
        })
        .expect("allocate libghostty-vt terminal");

        let render_state = RenderState::new().expect("allocate libghostty-vt render state");

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
            theme,
            font_desc,
            cell_metrics: metrics,
            cursor_blink_on: true,
            has_focus: true,
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

    /// Feed VT bytes into the terminal. Triggers a redraw so the new
    /// state is on screen by the next frame.
    pub fn vt_write(&self, bytes: &[u8]) {
        {
            let mut s = self.state.borrow_mut();
            s.terminal.vt_write(bytes);
        }
        self.widget.queue_draw();
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
            move |_ctrl, key, _keycode, mods| {
                // Phase 7 commit 5 bare-minimum input path: forward
                // printable Unicode keys + Enter / Tab / Backspace +
                // basic Ctrl chords for ASCII letters. Anything more
                // (arrows, function keys, Kitty modifyOtherKeys, etc.)
                // routes through `roost_vt::KeyEncoder` in commit 6.
                let bytes = encode_minimal_keystroke(key, mods);
                if bytes.is_empty() {
                    return glib::Propagation::Proceed;
                }
                let s = state.borrow();
                if let Some(cb) = s.input_callback.as_ref() {
                    cb(bytes);
                }
                glib::Propagation::Stop
            }
        });
        self.widget.add_controller(key_ctrl);
    }
}

/// Stop-gap key encoder for commit 5. Handles printable ASCII +
/// Enter / Tab / Backspace + Ctrl+letter chords. Everything else
/// returns empty (drops the keystroke); commit 6 replaces this with
/// `roost_vt::KeyEncoder` and the full Kitty protocol surface.
fn encode_minimal_keystroke(key: gdk::Key, mods: gdk::ModifierType) -> Vec<u8> {
    use gdk::Key as K;
    let is_ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);

    // Named keys with fixed C0 mappings.
    match key {
        K::Return | K::ISO_Enter | K::KP_Enter => return b"\r".to_vec(),
        K::Tab => return b"\t".to_vec(),
        K::BackSpace => return b"\x7f".to_vec(),
        K::Escape => return b"\x1b".to_vec(),
        _ => {}
    }

    // Ctrl+letter → C0 control byte.
    if is_ctrl {
        if let Some(c) = key.to_unicode() {
            if ('a'..='z').contains(&c) {
                return vec![(c as u8) - b'a' + 1];
            }
            if ('A'..='Z').contains(&c) {
                return vec![(c as u8) - b'A' + 1];
            }
        }
    }

    // Printable Unicode → UTF-8.
    if let Some(c) = key.to_unicode() {
        if !c.is_control() {
            return c.to_string().into_bytes();
        }
    }
    Vec::new()
}

impl Default for TerminalView {
    fn default() -> Self {
        Self::new()
    }
}

struct TerminalViewState {
    terminal: Terminal,
    render_state: RenderState,
    theme: Theme,
    font_desc: pango::FontDescription,
    cell_metrics: CellMetrics,
    cursor_blink_on: bool,
    has_focus: bool,
    /// Caller-installed keystroke handler. Optional because the
    /// TerminalView can be built before its session is spawned;
    /// `set_on_input` populates it once the daemon round-trip is
    /// ready.
    input_callback: Option<Box<dyn Fn(Vec<u8>)>>,
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

        let _ = (width, height);
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
