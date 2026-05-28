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
    DrawingArea, EventControllerFocus, EventControllerKey, EventControllerMotion,
    EventControllerScroll, EventControllerScrollFlags, GestureDrag,
};
use pangocairo::functions as pango_cairo;

use roost_vt::{
    ActiveScreen, Cell, ColorRgb, CursorInfo, CursorVisualStyle, KeyEncoder, MouseEncoder,
    MouseEvent, RenderState, ScrollViewport, Terminal, TerminalOptions,
};

use crate::cell_metrics::{default_font_description, CellMetrics};
use crate::clipboard;
use crate::config::CopyOnSelect;
use crate::key_encoder;
use crate::paste_image;
use crate::sprite;
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

/// Text snapshot of a terminal viewport, produced by [`TerminalView::dump`]
/// for the `tab.dump` IPC op. `rows_text` has one trimmed line per visible
/// row; `cursor` is `None` when off-viewport.
pub struct TerminalDump {
    pub cols: u32,
    pub rows: u32,
    pub cursor: Option<DumpCursor>,
    pub rows_text: Vec<String>,
}

/// Cursor position inside a [`TerminalDump`] (0-indexed from the top-left).
#[derive(Clone, Copy)]
pub struct DumpCursor {
    pub row: u32,
    pub col: u32,
    pub visible: bool,
}

impl TerminalView {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    /// Construct with a custom theme, optional font overrides, and a
    /// `copy-on-select` mode. The App passes user-supplied
    /// `font_family` + `font_size_pt` + `copy_on_select` from
    /// `~/.config/roost/config.conf` (defaults applied when absent).
    /// The theme's palette is pushed into libghostty so SGR cells
    /// (`ls --color`, `git diff`) flip to the theme's reds / greens.
    pub fn with_theme_font_and_copy(
        theme: Theme,
        font_family: Option<&str>,
        font_size_pt: Option<f64>,
        copy_on_select: CopyOnSelect,
    ) -> Self {
        let view = Self::with_theme(theme);
        view.apply_font(font_family, font_size_pt);
        view.state.borrow_mut().copy_on_select = copy_on_select;
        view
    }

    /// Snapshot the live terminal viewport as text for `tab.dump`.
    /// Main-thread-only — touches the libghostty handle + render state.
    pub fn dump(&self) -> TerminalDump {
        self.state.borrow_mut().dump_text()
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
        let mouse_encoder = MouseEncoder::new().expect("allocate libghostty-vt mouse encoder");
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
            mouse_encoder,
            pointer: (0.0, 0.0),
            theme,
            font_desc,
            cell_metrics: metrics,
            cursor_blink_on: true,
            has_focus: true,
            scrolled_back: false,
            scroll_accum: 0.0,
            selection: None,
            copy_on_select: CopyOnSelect::default(),
            input_callback: None,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            on_resize: None,
        }));

        // Draw function: hand a Cairo context per redraw.
        widget.set_draw_func({
            let state = state.clone();
            move |widget, cr, width, height| {
                let mut s = state.borrow_mut();
                s.paint(widget, cr, width as f64, height as f64);
            }
        });

        // Reflow the cell grid whenever the DrawingArea is reallocated.
        // The widget reports logical pixels and `CellMetrics` is in the
        // same logical-pixel space, so the division is consistent (see
        // the HiDPI note in app.rs `attach`). When the grid dimensions
        // change we fire `on_resize` so the PTY's `TIOCSWINSZ` tracks
        // the window. Per the callback invariant on `TerminalViewState`,
        // clone the callback out and drop the borrow before invoking it.
        widget.connect_resize({
            let state = state.clone();
            move |widget, w, h| {
                let mut s = state.borrow_mut();
                let fire = s
                    .reflow(w as f64, h as f64, false)
                    .then(|| (s.on_resize.clone(), s.cols, s.rows));
                drop(s);
                if let Some((Some(cb), cols, rows)) = fire {
                    cb(cols, rows);
                }
                widget.queue_draw();
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

        // Pointer tracking: the scroll controller doesn't carry a
        // position, so keep the latest hover here for the wheel-as-button
        // reports (which must name the cell under the pointer).
        let motion_ctrl = EventControllerMotion::new();
        motion_ctrl.connect_motion({
            let state = state.clone();
            move |_ctrl, x, y| {
                state.borrow_mut().pointer = (x, y);
            }
        });
        widget.add_controller(motion_ctrl);

        // Scroll wheel: 3 modes per the Mac UI / Go binary. Discrete
        // notches + smooth scroll both go through the same path; the
        // smooth-scroll accumulator handles trackpad fractional rows.
        let scroll_ctrl = EventControllerScroll::new(
            EventControllerScrollFlags::VERTICAL | EventControllerScrollFlags::DISCRETE,
        );
        scroll_ctrl.connect_scroll({
            let state = state.clone();
            let widget = widget.clone();
            move |ctrl, _dx, dy| {
                // Modifiers held during the scroll, so mouse-tracking apps
                // see shift/ctrl+wheel (matches the Mac path).
                let mods = key_encoder::translate_mods(ctrl.current_event_state());
                let mut s = state.borrow_mut();
                let bytes = s.handle_scroll(dy, mods);
                let cb = s.input_callback.clone();
                drop(s);
                widget.queue_draw();
                if !bytes.is_empty() {
                    if let Some(cb) = cb {
                        cb(bytes);
                    }
                }
                glib::Propagation::Stop
            }
        });
        widget.add_controller(scroll_ctrl);

        // Drag selection. Anchor on press, update on drag, on release
        // the selection becomes "committed" until the user clicks
        // elsewhere or types (commit 7+'s `clearSelection` flow).
        // Rows are captured in screen-y (scrollback-stable) space so
        // the highlight scrolls with the content.
        let drag = GestureDrag::new();
        drag.connect_drag_begin({
            let state = state.clone();
            let widget = widget.clone();
            move |_g, x, y| {
                let mut s = state.borrow_mut();
                // Start a fresh selection, or clear any stale one if
                // the viewport → screen conversion fails (terminal
                // handle not ready, cell coords out of range).
                s.selection = s.cell_at(x, y, &widget).and_then(|(col, row)| {
                    let screen_y = s.screen_y_for_viewport_row(row)?;
                    Some(Selection {
                        anchor_col: col,
                        anchor_screen_y: screen_y,
                        cursor_col: col,
                        cursor_screen_y: screen_y,
                    })
                });
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
                    let resolved = cell_at_inner(x, y, cell_w, cell_h).and_then(|(col, row)| {
                        let screen_y = s.screen_y_for_viewport_row(row)?;
                        Some((col, screen_y))
                    });
                    match (resolved, s.selection.as_mut()) {
                        (Some((col, screen_y)), Some(sel)) => {
                            sel.cursor_col = col;
                            sel.cursor_screen_y = screen_y;
                        }
                        (None, Some(_)) => {
                            // Conversion failed mid-drag — drop the
                            // selection rather than keep updating a
                            // stale anchor.
                            s.selection = None;
                        }
                        _ => {}
                    }
                }
                drop(s);
                widget.queue_draw();
            }
        });
        // On selection commit, copy-on-select per the config. The
        // three-state model matches Ghostty (`off | true | clipboard`):
        //   * Off: no write (user has to press explicit copy).
        //   * True (default): write to PRIMARY only. Middle-click
        //     paste works; Ctrl+Shift+V keeps whatever was last
        //     Ctrl+Shift+C'd. This is the conventional X11 behavior.
        //   * Clipboard: write to BOTH PRIMARY and CLIPBOARD, so a
        //     drag-and-Ctrl+V-into-another-app flow works.
        // `Target::Primary` is a no-op off Linux, so installing the
        // handler unconditionally is harmless on macOS.
        drag.connect_drag_end({
            let state = state.clone();
            move |_g, _x, _y| {
                let mode = state.borrow().copy_on_select;
                if mode == CopyOnSelect::Off {
                    return;
                }
                if let Some(text) = selection_text(&state) {
                    clipboard::write(clipboard::Target::Primary, &text);
                    if mode == CopyOnSelect::Clipboard {
                        clipboard::write(clipboard::Target::Clipboard, &text);
                    }
                }
            }
        });
        widget.add_controller(drag);

        // Middle-click pastes the PRIMARY selection into this terminal,
        // matching Linux terminal convention. Routes through the same
        // bracketed-paste-aware path as Ctrl+Shift+V. The middle-click
        // paste gesture is a genuinely Linux-only UI convention.
        #[cfg(target_os = "linux")]
        {
            let middle_click = gtk4::GestureClick::new();
            middle_click.set_button(gtk4::gdk::BUTTON_MIDDLE);
            middle_click.connect_pressed({
                let state = state.clone();
                move |_gesture, _n_press, _x, _y| {
                    clipboard::read(clipboard::Target::Primary, {
                        let state = state.clone();
                        move |text| paste_text_into(&state, text)
                    });
                }
            });
            widget.add_controller(middle_click);
        }

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
        // The new cell pixel size changes how many cells fit; reflow so
        // libghostty + the PTY learn the new grid. `force` because the
        // cell metrics changed even when the column/row count happens to
        // land the same. Fire `on_resize` only when the grid dimensions
        // actually moved — the PTY only cares about cols/rows.
        let (w, h) = (self.widget.width(), self.widget.height());
        let fire = (w > 0 && h > 0 && s.reflow(w as f64, h as f64, true))
            .then(|| (s.on_resize.clone(), s.cols, s.rows));
        drop(s);
        if let Some((Some(cb), cols, rows)) = fire {
            cb(cols, rows);
        }
        self.widget.queue_draw();
    }

    /// Install a resize handler, fired with the new `(cols, rows)`
    /// whenever a reflow changes the grid. Wired by the session attach
    /// to push the grid through `TabSession::send_resize` →
    /// `ioctl(TIOCSWINSZ)`. If the widget is already allocated when the
    /// callback lands (the first `resize` signal can fire before the
    /// session attaches), force a reflow and push the current grid so
    /// the PTY starts at the real window size rather than 80×24.
    pub fn set_on_resize<F>(&self, callback: F)
    where
        F: Fn(u16, u16) + 'static,
    {
        let mut s = self.state.borrow_mut();
        s.on_resize = Some(Rc::new(callback));
        let (w, h) = (self.widget.width(), self.widget.height());
        let fire = if w > 0 && h > 0 {
            s.reflow(w as f64, h as f64, true);
            Some((s.on_resize.clone(), s.cols, s.rows))
        } else {
            None
        };
        drop(s);
        if let Some((Some(cb), cols, rows)) = fire {
            cb(cols, rows);
        }
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

    /// Set the selection rectangle from viewport `(col, row)` coords.
    /// Mirrors `mouseDown` + `mouseDragged` for the IPC `selection.set`
    /// op: drops any existing selection, anchors at `anchor`, and sets
    /// the cursor end at `cursor`. Returns `false` and clears the
    /// selection if either point can't be converted to a stable screen-y
    /// (out-of-range row, terminal not ready). Same coordinate semantics
    /// as the drag handlers.
    pub fn set_selection(&self, anchor: (u16, u16), cursor: (u16, u16)) -> bool {
        let mut s = self.state.borrow_mut();
        let resolved = (|| {
            let anchor_y = s.screen_y_for_viewport_row(anchor.1)?;
            let cursor_y = s.screen_y_for_viewport_row(cursor.1)?;
            Some((anchor_y, cursor_y))
        })();
        match resolved {
            Some((anchor_y, cursor_y)) => {
                s.selection = Some(Selection {
                    anchor_col: anchor.0,
                    anchor_screen_y: anchor_y,
                    cursor_col: cursor.0,
                    cursor_screen_y: cursor_y,
                });
                drop(s);
                self.widget.queue_draw();
                true
            }
            None => {
                s.selection = None;
                drop(s);
                self.widget.queue_draw();
                false
            }
        }
    }

    /// Clear any active selection on this terminal.
    pub fn clear_selection(&self) {
        let had = {
            let mut s = self.state.borrow_mut();
            s.selection.take().is_some()
        };
        if had {
            self.widget.queue_draw();
        }
    }

    /// Snapshot the current selection for the `selection.dump` IPC op:
    /// extracted text (same path the `Alt+C` copy uses) + whether each
    /// endpoint is currently visible in the viewport. Returns `None`
    /// when no selection is active.
    pub fn dump_selection(&self) -> Option<SelectionDumpData> {
        let (anchor_screen_y, cursor_screen_y) = {
            let s = self.state.borrow();
            let sel = s.selection?;
            (sel.anchor_screen_y, sel.cursor_screen_y)
        };
        let text = selection_text(&self.state);
        let s = self.state.borrow();
        let anchor_visible = s.viewport_row_for_screen_y(anchor_screen_y).is_some();
        let cursor_visible = s.viewport_row_for_screen_y(cursor_screen_y).is_some();
        Some(SelectionDumpData {
            text,
            anchor_visible,
            cursor_visible,
        })
    }

    /// Copy the current selection to the system clipboard
    /// (`gdk::Display::clipboard`). No-op if the selection is empty. On
    /// Linux the text is also published to the X11/Wayland PRIMARY
    /// selection so a middle-click paste into another app works.
    pub fn copy_selection_to_clipboard(&self) {
        let Some(text) = selection_text(&self.state) else {
            return;
        };
        clipboard::write(clipboard::Target::Clipboard, &text);
        // PRIMARY is an X11/Wayland concept; `Target::Primary` no-ops
        // off Linux.
        clipboard::write(clipboard::Target::Primary, &text);
    }

    /// Read the system clipboard and feed it into the PTY. Three
    /// shapes, in priority order: text → file URIs (image extensions
    /// only) → raw image bytes. Image bytes (PNG passthrough or any
    /// other format gdk-pixbuf can decode) are written to a temp
    /// `.png` and the path is pasted as bracketed text so agents like
    /// Claude Code and Codex recognise it and offer to attach. The
    /// three branches share `paste_text_into` so DECSET-2004 wrapping
    /// stays consistent. Each read is async; the callbacks hop back
    /// to the GTK main loop before touching `paste_text_into`.
    pub fn paste_from_clipboard(&self) {
        let state = self.state.clone();
        clipboard::read(clipboard::Target::Clipboard, move |text| {
            if !text.is_empty() {
                paste_text_into(&state, text);
                return;
            }
            Self::paste_image_or_uris(state);
        });
    }

    /// File-URI fallback (cheap, text-shaped) → image-bytes fallback.
    /// Pulled out of `paste_from_clipboard` because the closure has
    /// already consumed the captured state once on the empty-text
    /// branch, and re-using it requires a fresh clone hop.
    fn paste_image_or_uris(state: Rc<RefCell<TerminalViewState>>) {
        let for_image = state.clone();
        clipboard::read_file_uris(move |paths| {
            if !paths.is_empty() {
                paste_text_into(&state, paths.join("\n"));
                return;
            }
            clipboard::read_image(move |maybe| {
                let Some((bytes, mime)) = maybe else { return };
                match paste_image::materialize(&bytes, &mime) {
                    Ok(path) => {
                        paste_text_into(&for_image, path.to_string_lossy().into_owned());
                    }
                    Err(e) => tracing::warn!(error = %e, "clipboard image materialize"),
                }
            });
        });
    }

    // Multi-path joining lives here rather than in paste_image_or_uris
    // because `paths.join(...)` must use newline (not space): a Finder /
    // Nautilus selection that includes "Screenshot 2026.png" or paths
    // under "/Volumes/My Disk" would merge on space. Bracketed paste
    // delivers the bytes verbatim; the receiver treats each line as a
    // separate attachment candidate. Mirrors the Mac fix in
    // TerminalView.paste(_:) under PR #149.

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
        s.input_callback = Some(Rc::new(callback));
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
                let mut s = state.borrow_mut();
                // A bare modifier (incl. the modifier that begins a copy
                // chord such as Alt+C / Ctrl+Shift+C) must NOT disturb the
                // selection or the scrollback position: clearing on that
                // keypress would wipe the selection before the copy action
                // reads it (the #99 fix).
                if is_modifier_key(key) {
                    return glib::Propagation::Proceed;
                }
                // A real key: typing snaps the viewport back to the bottom
                // (matches the Go binary's `cmd/roost/input.go:67`) and
                // clears any active selection — even when the key encodes
                // to nothing (dead keys / IME composition / unmapped),
                // since typing always overrides a selection.
                let snapped = s.scrolled_back;
                if s.scrolled_back {
                    s.terminal.scroll_viewport(ScrollViewport::Bottom);
                    s.scrolled_back = false;
                    s.scroll_accum = 0.0;
                }
                let had_selection = s.selection.take().is_some();
                // Phase 7 commit 6: route through `roost_vt::KeyEncoder`
                // (the safe wrapper landed in commit 1). The encoder
                // handles modifier conventions, Kitty keyboard protocol,
                // DECCKM application-mode arrows, etc. Split-borrow so the
                // encoder can take `&mut encoder` + `&terminal` at once.
                let bytes = {
                    let s_mut: &mut TerminalViewState = &mut s;
                    key_encoder::encode_key(&mut s_mut.encoder, &s_mut.terminal, key, mods)
                };
                if snapped || had_selection {
                    widget.queue_draw();
                }
                // Clone the callback out and drop the borrow before
                // invoking, per the callback invariant on
                // `TerminalViewState`.
                let cb = s.input_callback.clone();
                drop(s);
                if bytes.is_empty() {
                    return glib::Propagation::Proceed;
                }
                if let Some(cb) = cb {
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
    /// Reused across wheel events. Encodes the scroll wheel as button-4/5
    /// reports when the focused app enables mouse tracking.
    mouse_encoder: MouseEncoder,
    /// Last known pointer position in widget pixels, tracked by a motion
    /// controller. The scroll controller doesn't carry a position, so we
    /// keep the latest hover here to report the wheel at the right cell.
    pointer: (f64, f64),
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
    /// Current drag selection, in (col, screen_y) coordinates.
    /// `None` outside an active drag.
    selection: Option<Selection>,
    /// `copy-on-select` mode from `~/.config/roost/config.conf`,
    /// resolved at startup by `App` and passed through
    /// [`TerminalView::with_theme_font_and_copy`]. Read by `drag_end`
    /// to decide which clipboard target(s) receive the selection.
    copy_on_select: CopyOnSelect,
    /// Caller-installed keystroke handler. Optional because the
    /// TerminalView can be built before its session is spawned;
    /// `set_on_input` populates it once the daemon round-trip is
    /// ready.
    ///
    /// Callback invariant: `input_callback` and `on_resize` are cloned
    /// out and invoked **after** the `state` borrow is dropped, so a
    /// callback may safely re-enter the view. An `Rc` makes the clone
    /// cheap (atomic-free, single-threaded GTK).
    input_callback: Option<Rc<dyn Fn(Vec<u8>)>>,
    /// Live cell grid. Reflowed from the widget's pixel size on every
    /// `resize` signal; seeded with the `DEFAULT_COLS`/`DEFAULT_ROWS`
    /// the libghostty terminal was allocated with.
    cols: u16,
    rows: u16,
    /// Caller-installed resize handler. Fired (with the new grid) when
    /// a reflow changes the column/row count so the PTY's window size
    /// (`TIOCSWINSZ`) tracks the widget. `set_on_resize` populates it
    /// once the session is attached. See the callback invariant on
    /// `input_callback`.
    on_resize: Option<Rc<dyn Fn(u16, u16)>>,
}

/// Selection snapshot for the `selection.dump` IPC op. `text` is the
/// extracted plain text (same path as `Alt+C` / Ctrl+Shift+C) or `None`
/// when the selection has scrolled fully out of the viewport.
pub struct SelectionDumpData {
    pub text: Option<String>,
    pub anchor_visible: bool,
    pub cursor_visible: bool,
}

/// Drag-selection state. Rows are stored as libghostty
/// `PointTag::Screen` y coordinates (the unified screen-including-
/// scrollback index) so the highlight stays anchored to the same
/// rows as the user scrolls. Cols are column indices that don't
/// change with vertical scroll.
#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor_col: u16,
    anchor_screen_y: u32,
    cursor_col: u16,
    cursor_screen_y: u32,
}

impl Selection {
    fn is_empty(&self) -> bool {
        self.anchor_col == self.cursor_col && self.anchor_screen_y == self.cursor_screen_y
    }

    /// Normalized `(start_col, start_y, end_col, end_y)` in screen-y
    /// space. Both ends are exclusive — the cell the user dragged to
    /// is included, then `+1` makes the bound exclusive for `< end`
    /// comparisons. Mirrors the Mac UI's `CellSelection.normalized`
    /// 1:1; pre-rewrite Linux omitted the `+1` on `end_col`, which
    /// silently dropped the rightmost cell from the paint rectangle
    /// and the copy text.
    fn normalized(&self) -> (u16, u32, u16, u32) {
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

/// Compute `[start_col, end_col)` for a single row of a multi-row
/// selection. Single-row selections use the literal cols; multi-row
/// selections fill the first row from `start_col` to the right edge,
/// interior rows full-width, and the last row from the left edge to
/// `end_col`. Mirrors the Mac UI's `TerminalView.colRange`.
fn selection_col_range(
    offset: usize,
    total_row_span: usize,
    start_col: u16,
    end_col: u16,
    cols: u16,
) -> (u16, u16) {
    if total_row_span == 1 {
        return (start_col, end_col);
    }
    if offset == 0 {
        return (start_col, cols);
    }
    if offset == total_row_span - 1 {
        return (0, end_col);
    }
    (0, cols)
}

impl TerminalViewState {
    /// Recompute the cell grid from the widget's pixel size and push
    /// the new dimensions into libghostty (cell px included for OSC
    /// size reports). Returns `true` when the column/row count changed.
    /// `force` re-pushes to libghostty even when the count is unchanged
    /// — used after a font-size change (cell px moved) and when a
    /// resize callback is installed late.
    fn reflow(&mut self, width_px: f64, height_px: f64, force: bool) -> bool {
        let cw = self.cell_metrics.cell_width;
        let ch = self.cell_metrics.cell_height;
        if cw <= 0.0 || ch <= 0.0 {
            return false;
        }
        let new_cols = (width_px / cw).floor().clamp(1.0, u16::MAX as f64) as u16;
        let new_rows = (height_px / ch).floor().clamp(1.0, u16::MAX as f64) as u16;
        let changed = (new_cols, new_rows) != (self.cols, self.rows);
        if !changed && !force {
            return false;
        }
        self.cols = new_cols;
        self.rows = new_rows;
        if let Err(err) =
            self.terminal
                .resize(new_cols, new_rows, cw.round() as u32, ch.round() as u32)
        {
            tracing::warn!(?err, new_cols, new_rows, "terminal resize failed");
        }
        changed
    }

    /// Snapshot the live viewport as text for the `tab.dump` IPC op:
    /// one trimmed line per row (a blank cell becomes a space so columns
    /// line up) plus the cursor. Mirrors `paint`'s update→cursor→walk
    /// but accumulates text instead of drawing. Cells arrive in column
    /// order across the full grid, so appending reconstructs each row.
    fn dump_text(&mut self) -> TerminalDump {
        if let Err(err) = self.render_state.update(&self.terminal) {
            tracing::warn!(?err, "render_state.update failed for tab.dump");
        }
        let cursor = self.render_state.cursor().map(|c| DumpCursor {
            row: c.row,
            col: c.col,
            visible: c.visible,
        });
        let mut lines: Vec<String> = vec![String::new(); self.rows as usize];
        let _ = self.render_state.walk(&self.terminal, |row, cell: Cell| {
            if let Some(line) = lines.get_mut(row as usize) {
                if cell.text.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(&cell.text);
                }
            }
        });
        for line in &mut lines {
            // Trim only ASCII spaces (the blank-cell filler above), not
            // all Unicode whitespace, so the dump matches the Mac
            // `dumpText` rstrip byte-for-byte (cross-UI parity).
            let end = line.trim_end_matches(' ').len();
            line.truncate(end);
        }
        TerminalDump {
            cols: self.cols as u32,
            rows: self.rows as u32,
            cursor,
            rows_text: lines,
        }
    }

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
        let bold_color = self.theme.bold_color;

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
                // Apply SGR inverse + bold-accent rules. Without this,
                // codex's `\e[7m`-highlighted prompt row renders against
                // the canvas-default bg and the gray prompt disappears
                // (the visible regression the PR fixes). The theme's
                // optional `bold-color` accent (Ghostty `bold-color = …`)
                // colors bold default-fg cells when present; themes
                // that don't set it keep the canvas-default fg.
                let (fg, bg, has_explicit_bg) =
                    resolve_cell_colors(&cell, default_fg, default_bg, bold_color);
                if has_explicit_bg && bg != default_bg {
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

        // Pass B: glyphs. Box-drawing (U+2500..U+257F) and block-
        // element (U+2580..U+259F) codepoints get a custom geometric
        // renderer that tiles pixel-perfectly across cells; everything
        // else falls through to Pango. Pango fonts produce visible
        // seams in TUI chrome — most obvious in the opencode wordmark
        // logo — which is what `crate::sprite` exists to fix.
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
            let x = *col as f64 * cell_w;
            let y = *row as f64 * cell_h;
            // Sprite-render single-codepoint cells whose codepoint
            // falls in one of the geometric ranges. Multi-codepoint
            // graphemes (emoji ZWJ etc.) skip this path because the
            // sprite layer is by-codepoint, not by-grapheme.
            let mut chars = text.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                if sprite::draw_cell_sprite(cr, x, y, cell_w, cell_h, *fg, c as u32) {
                    continue;
                }
            }
            set_cairo_color(cr, *fg);
            layout.set_text(text);
            cr.move_to(x, y);
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
        let (sc, sy, ec, ey) = sel.normalized();
        let (r, g, b) = self.theme.selection_background.to_f64();
        let total_row_span = (ey - sy) as usize;
        let cols = self.cols;
        // Walk each row of the selection in screen-y space, resolve
        // to a viewport row each frame, skip rows currently outside
        // the visible viewport. This makes the highlight scroll with
        // the content (the bug fix).
        for offset in 0..total_row_span {
            let screen_y = sy.saturating_add(offset as u32);
            let Some(v_row) = self.viewport_row_for_screen_y(screen_y) else {
                continue;
            };
            let (start_col, end_col) = selection_col_range(offset, total_row_span, sc, ec, cols);
            cr.set_source_rgba(r, g, b, 0.35);
            cr.rectangle(
                start_col as f64 * cell_w,
                v_row as f64 * cell_h,
                end_col.saturating_sub(start_col) as f64 * cell_w,
                cell_h,
            );
            let _ = cr.fill();
        }
    }

    /// Convert a viewport row (0 = top of visible area) to its
    /// `PointTag::Screen` y coordinate. Returns `None` when libghostty
    /// rejects the conversion (out-of-range row, no terminal set up
    /// yet). Used by drag-begin / drag-update to anchor selection in
    /// scrollback-stable coordinates.
    fn screen_y_for_viewport_row(&self, row: u16) -> Option<u32> {
        let pt = roost_vt::Point::viewport(0, row as u32);
        let screen = self
            .terminal
            .convert_point(pt, roost_vt::PointTag::Screen)?;
        Some(screen.y)
    }

    /// Convert a `PointTag::Screen` y coordinate back to its current
    /// viewport row. Returns `None` when the row is scrolled out of
    /// the visible viewport (caller should clip / skip).
    fn viewport_row_for_screen_y(&self, screen_y: u32) -> Option<u16> {
        let pt = roost_vt::Point::screen(0, screen_y);
        let viewport = self
            .terminal
            .convert_point(pt, roost_vt::PointTag::Viewport)?;
        if viewport.y >= self.rows as u32 {
            return None;
        }
        Some(viewport.y as u16)
    }

    /// Handle a single scroll-wheel `dy`. Negative = up (older
    /// history). 3 modes per the Go binary `cmd/roost/session.go`:
    ///   * Mouse-tracking (DECSET 1000/1002/1003) — encode button-4/5
    ///     reports via `encode_wheel_buttons`, checked first so a
    ///     tracking alt-screen app (htop) gets the report.
    ///   * Alt-screen — translate to ArrowUp / ArrowDown via the key
    ///     encoder. Lets vim / less consume the wheel.
    ///   * Primary screen — local viewport scroll via
    ///     `Terminal::scroll_viewport(Delta)`.
    ///
    /// Returns the bytes to feed into the PTY (the mouse / arrow-key
    /// encoding); empty for a local scrollback move. The caller
    /// dispatches them through `input_callback` after dropping the
    /// borrow, per the callback invariant on `TerminalViewState`.
    fn handle_scroll(&mut self, dy: f64, mods: roost_vt::Mods) -> Vec<u8> {
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
            return Vec::new();
        };

        // Mouse tracking: the app opted into mouse events, so forward the
        // wheel as button-4 (up) / button-5 (down) reports at the
        // pointer's cell. Checked *before* alt-screen — a mouse-tracking
        // alt-screen app (htop) wants the report, not arrow keys. The
        // encoder honors the negotiated format (X10 / SGR / pixels).
        if self.terminal.mouse_tracking() {
            return self.encode_wheel_buttons(rows_to_scroll, mods);
        }

        if self.terminal.active_screen() == ActiveScreen::Alternate {
            // Translate to arrow keys for alt-screen apps.
            let key = if rows_to_scroll < 0 {
                roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_UP
            } else {
                roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_DOWN
            };
            let mut event = match roost_vt::KeyEvent::new() {
                Ok(ev) => ev,
                Err(_) => return Vec::new(),
            };
            event.set_action(roost_vt::key_action::PRESS);
            event.set_key(key);
            event.set_mods(0);
            self.encoder.sync_from_terminal(&self.terminal);
            let mut out = Vec::new();
            for _ in 0..rows_to_scroll.unsigned_abs() {
                if let Ok(bytes) = self.encoder.encode(&event) {
                    out.extend_from_slice(&bytes);
                }
            }
            return out;
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
        Vec::new()
    }

    /// Encode one wheel button-press per scrolled row at the pointer's
    /// current cell. `rows < 0` is wheel-up (button 4); `rows > 0` is
    /// wheel-down (button 5). Returns the concatenated reports (empty if
    /// the encoder declines, e.g. the negotiated format reports nothing).
    fn encode_wheel_buttons(&mut self, rows: isize, mods: roost_vt::Mods) -> Vec<u8> {
        let button = if rows < 0 {
            roost_vt::mouse_button::FOUR // wheel up
        } else {
            roost_vt::mouse_button::FIVE // wheel down
        };
        let cw = self.cell_metrics.cell_width.max(1.0);
        let ch = self.cell_metrics.cell_height.max(1.0);
        let screen_w = (cw * self.cols as f64) as u32;
        let screen_h = (ch * self.rows as f64) as u32;
        // Clamp the pointer into the grid so a wheel event just off the
        // edge still names the last cell, not an out-of-range coordinate.
        let (px, py) = self.pointer;
        let x = px.clamp(0.0, (cw * self.cols as f64 - 1.0).max(0.0)) as f32;
        let y = py.clamp(0.0, (ch * self.rows as f64 - 1.0).max(0.0)) as f32;

        self.mouse_encoder.sync_from_terminal(&self.terminal);
        self.mouse_encoder
            .set_size(screen_w, screen_h, cw as u32, ch as u32);

        let mut event = match MouseEvent::new() {
            Ok(ev) => ev,
            Err(_) => return Vec::new(),
        };
        event.set_action(roost_vt::mouse_action::PRESS);
        event.set_button(button);
        event.set_mods(mods);
        event.set_position(x, y);

        let mut out = Vec::new();
        for _ in 0..rows.unsigned_abs() {
            if let Ok(bytes) = self.mouse_encoder.encode(&event) {
                out.extend_from_slice(&bytes);
            }
        }
        out
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
                        // Same sprite-vs-Pango dispatch as Pass B —
                        // a block-element cursor cell (e.g. a ▌ over
                        // a TUI glyph) must redraw geometrically too
                        // or it'd seam against the cursor block.
                        let mut chars = text.chars();
                        let drawn = if let (Some(c), None) = (chars.next(), chars.next()) {
                            sprite::draw_cell_sprite(cr, x, y, cell_w, cell_h, canvas_bg, c as u32)
                        } else {
                            false
                        };
                        if !drawn {
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
}

/// Set the Cairo source color from an `roost_vt::ColorRgb`.
fn set_cairo_color(cr: &cairo::Context, rgb: ColorRgb) {
    let (r, g, b) = rgb.to_f64();
    cr.set_source_rgb(r, g, b);
}

/// Resolve a cell's effective fg/bg + whether it needs a BG fill,
/// applying the same SGR inverse + bold-accent rules as the legacy
/// Go `cellColors` (`cmd/roost/render.go:206-224`). Free function so
/// it's unit-testable without a Cairo context or DrawingArea.
///
/// Rules (matching legacy 1:1):
/// * Default colors are the per-frame terminal default.
/// * Explicit SGR fg/bg overrides the default.
/// * `\e[7m` (inverse) swaps the *effective* fg/bg — done **after**
///   the explicit-color lookup and **before** the bold-accent step,
///   and forces `has_explicit_bg = true` so the renderer paints the
///   swap even when the cell had no explicit bg of its own.
/// * `bold_color` is applied only when the cell is bold, has no
///   explicit fg, and isn't inverted. (Bold red stays red; only
///   bold default-fg text gets the accent.) Pass `None` to disable.
///   The theme parser populates this from the Ghostty `bold-color`
///   line; themes that omit it keep the canvas-default fg for bold
///   text.
pub(crate) fn resolve_cell_colors(
    cell: &Cell,
    default_fg: ColorRgb,
    default_bg: ColorRgb,
    bold_color: Option<ColorRgb>,
) -> (ColorRgb, ColorRgb, bool) {
    let mut fg = cell.fg.unwrap_or(default_fg);
    let mut bg = cell.bg.unwrap_or(default_bg);
    let mut has_explicit_bg = cell.bg.is_some();
    if cell.style.inverse {
        std::mem::swap(&mut fg, &mut bg);
        has_explicit_bg = true;
    }
    if cell.style.bold && cell.fg.is_none() && !cell.style.inverse {
        if let Some(bc) = bold_color {
            fg = bc;
        }
    }
    (fg, bg, has_explicit_bg)
}

/// Extract the active selection as plain text (trailing whitespace
/// trimmed per line), or `None` when there is no non-empty selection.
/// A free function — shared by the explicit copy path and (on Linux)
/// the drag-end PRIMARY publish, neither of which can hold an
/// `&TerminalView`.
///
/// Selection rows are stored in screen-y space; we resolve each to its
/// current viewport row before walking. Rows currently outside the
/// viewport are skipped — copy returns only the visible portion of the
/// selection. A fuller scroll-walk-restore implementation is a
/// follow-up; mirrors the Mac UI's limitation in `selectedPlainText`.
fn selection_text(state: &Rc<RefCell<TerminalViewState>>) -> Option<String> {
    let s = state.borrow();
    let sel = s.selection?;
    if sel.is_empty() {
        return None;
    }
    let (sc, sy, ec, ey) = sel.normalized();
    let total_row_span = (ey - sy) as usize;
    if total_row_span == 0 {
        return None;
    }
    let cols = s.cols;

    // Map viewport rows currently representing this selection back to
    // their selection offset. Off-viewport rows are silently skipped.
    let mut offset_for_viewport_row: std::collections::HashMap<u32, usize> =
        std::collections::HashMap::with_capacity(total_row_span);
    for offset in 0..total_row_span {
        let screen_y = sy.saturating_add(offset as u32);
        if let Some(v_row) = s.viewport_row_for_screen_y(screen_y) {
            offset_for_viewport_row.insert(v_row as u32, offset);
        }
    }
    drop(s);
    if offset_for_viewport_row.is_empty() {
        return None;
    }

    let mut rows: Vec<String> = vec![String::new(); total_row_span];
    let mut s = state.borrow_mut();
    let TerminalViewState {
        terminal,
        render_state,
        ..
    } = &mut *s;
    let _ = render_state.update(terminal);
    let _ = render_state.walk(terminal, |row, cell| {
        let Some(&offset) = offset_for_viewport_row.get(&row) else {
            return;
        };
        let (start_col, end_col) = selection_col_range(offset, total_row_span, sc, ec, cols);
        if cell.col < start_col || cell.col >= end_col {
            return;
        }
        if cell.text.is_empty() {
            rows[offset].push(' ');
        } else {
            rows[offset].push_str(&cell.text);
        }
    });
    drop(s);

    // Trim trailing whitespace per row, then drop empty leading and
    // trailing rows so a partial copy (where the first or last
    // selection rows scrolled off-screen and are blank in `rows`)
    // doesn't carry stray newlines into the clipboard.
    let mut trimmed: Vec<String> = rows.iter().map(|r| r.trim_end().to_string()).collect();
    while matches!(trimmed.first(), Some(line) if line.is_empty()) {
        trimmed.remove(0);
    }
    while matches!(trimmed.last(), Some(line) if line.is_empty()) {
        trimmed.pop();
    }
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.join("\n"))
    }
}

/// Feed pasted `text` into the PTY, wrapping in bracketed-paste escapes
/// (`ESC[200~` … `ESC[201~`) when DECSET 2004 is active. Shared by
/// Ctrl+Shift+V (CLIPBOARD) and, on Linux, middle-click (PRIMARY); the
/// async clipboard read lives in `clipboard::read`. Reads the callback
/// out of the borrow before invoking, per the callback invariant on
/// `TerminalViewState`. Empty `text` is a no-op so middle-clicking with
/// an empty PRIMARY selection doesn't send a stray `ESC[200~ESC[201~`,
/// and so the image / URI cascade in `paste_from_clipboard` (which
/// relies on `clipboard::read` firing with `""` when no text is
/// available) doesn't double-paste.
fn paste_text_into(state: &Rc<RefCell<TerminalViewState>>, text: String) {
    if text.is_empty() {
        return;
    }
    let (bracketed, cb) = {
        let s = state.borrow();
        (s.terminal.mode_get(2004), s.input_callback.clone())
    };
    let Some(cb) = cb else { return };
    let bytes = if bracketed {
        let mut buf = Vec::with_capacity(text.len() + 8);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~");
        buf
    } else {
        text.into_bytes()
    };
    cb(bytes);
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

/// True for bare modifier keypresses (Shift/Ctrl/Alt/Super/etc.) that
/// should never disturb an active selection or the scrollback position.
/// Used by the key handler so a real key (including one that encodes to
/// nothing — dead keys / IME) clears the selection while the modifier
/// that begins a copy chord does not.
fn is_modifier_key(key: gtk4::gdk::Key) -> bool {
    use gtk4::gdk::Key as K;
    matches!(
        key,
        K::Shift_L
            | K::Shift_R
            | K::Control_L
            | K::Control_R
            | K::Alt_L
            | K::Alt_R
            | K::Meta_L
            | K::Meta_R
            | K::Super_L
            | K::Super_R
            | K::Hyper_L
            | K::Hyper_R
            | K::ISO_Level3_Shift
            | K::ISO_Level5_Shift
            | K::Caps_Lock
            | K::Num_Lock
            | K::Shift_Lock
    )
}

#[cfg(test)]
mod tests {
    //! Inverse + bold-accent resolver tests. The Pass A walk feeds
    //! `resolve_cell_colors` per cell; getting this wrong produces a
    //! visible regression (e.g. codex's gray prompt vanishing into
    //! the canvas bg). All cases below mirror legacy
    //! `cmd/roost/render.go::cellColors` behavior.
    use super::*;
    use roost_vt::{Cell, ColorRgb, Style};

    const DEFAULT_FG: ColorRgb = ColorRgb::new(0xe5, 0xe5, 0xe5);
    const DEFAULT_BG: ColorRgb = ColorRgb::new(0x1c, 0x1c, 0x1c);
    const EXPLICIT_FG: ColorRgb = ColorRgb::new(0x80, 0xc0, 0x40);
    const EXPLICIT_BG: ColorRgb = ColorRgb::new(0x3a, 0x3a, 0x3a);
    const BOLD: ColorRgb = ColorRgb::new(0xff, 0xff, 0xff);

    fn cell(fg: Option<ColorRgb>, bg: Option<ColorRgb>, style: Style) -> Cell {
        Cell {
            col: 0,
            fg,
            bg,
            text: String::new(),
            style,
        }
    }

    // Selection column-range helper tests. Mirrors
    // `mac/Tests/RoostTests/SelectionColRangeTests.swift` 1:1 so both
    // UIs agree on how a multi-row selection's first/middle/last rows
    // map to per-row [start_col, end_col) extents.
    const COLS: u16 = 80;

    #[test]
    fn selection_col_range_single_row_uses_literal_cols() {
        let (s, e) = selection_col_range(0, 1, 3, 17, COLS);
        assert_eq!((s, e), (3, 17));
    }

    #[test]
    fn selection_col_range_first_row_of_multi_fills_to_right_edge() {
        let (s, e) = selection_col_range(0, 4, 12, 5, COLS);
        assert_eq!((s, e), (12, COLS));
    }

    #[test]
    fn selection_col_range_interior_row_spans_full_width() {
        let (s, e) = selection_col_range(1, 4, 12, 5, COLS);
        assert_eq!((s, e), (0, COLS));
    }

    #[test]
    fn selection_col_range_last_row_ends_at_end_col() {
        let (s, e) = selection_col_range(3, 4, 12, 5, COLS);
        assert_eq!((s, e), (0, 5));
    }

    // Selection-rectangle normalization tests — ensure that dragging
    // in any direction (anchor below cursor, anchor above cursor,
    // anchor and cursor on same row, same cell) yields a
    // well-formed normalized rectangle. Catches off-by-ones in the
    // screen-y vs viewport-row migration.
    #[test]
    fn selection_normalized_same_cell_is_empty_zero_span() {
        let sel = Selection {
            anchor_col: 5,
            anchor_screen_y: 100,
            cursor_col: 5,
            cursor_screen_y: 100,
        };
        assert!(sel.is_empty());
        let (sc, sy, ec, ey) = sel.normalized();
        // Empty selection still produces a 1-cell normalized rect.
        assert_eq!((sc, sy, ec, ey), (5, 100, 6, 101));
    }

    #[test]
    fn selection_normalized_anchor_above_cursor_keeps_order() {
        let sel = Selection {
            anchor_col: 5,
            anchor_screen_y: 10,
            cursor_col: 8,
            cursor_screen_y: 12,
        };
        let (sc, sy, ec, ey) = sel.normalized();
        assert_eq!((sc, sy, ec, ey), (5, 10, 9, 13));
    }

    #[test]
    fn selection_normalized_cursor_above_anchor_swaps() {
        let sel = Selection {
            anchor_col: 8,
            anchor_screen_y: 12,
            cursor_col: 5,
            cursor_screen_y: 10,
        };
        let (sc, sy, ec, ey) = sel.normalized();
        // Same rectangle as the previous test — direction-independent.
        assert_eq!((sc, sy, ec, ey), (5, 10, 9, 13));
    }

    #[test]
    fn plain_default_cell_inherits_defaults_and_skips_bg_fill() {
        let c = cell(None, None, Style::default());
        let (fg, bg, has_bg) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, None);
        assert_eq!(fg, DEFAULT_FG);
        assert_eq!(bg, DEFAULT_BG);
        assert!(!has_bg, "default cell must not trigger a per-cell bg fill");
    }

    #[test]
    fn explicit_bg_is_reported_as_fillable() {
        let c = cell(None, Some(EXPLICIT_BG), Style::default());
        let (fg, bg, has_bg) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, None);
        assert_eq!(fg, DEFAULT_FG);
        assert_eq!(bg, EXPLICIT_BG);
        assert!(has_bg);
    }

    /// The codex regression: `\e[7m` on an otherwise-default cell.
    /// Pre-fix the resolver simply returned `(default_fg, default_bg,
    /// false)`, so the renderer skipped the bg fill and the gray
    /// prompt row stayed black.
    #[test]
    fn inverse_default_cell_swaps_colors_and_forces_bg_fill() {
        let c = cell(
            None,
            None,
            Style {
                inverse: true,
                ..Style::default()
            },
        );
        let (fg, bg, has_bg) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, None);
        assert_eq!(fg, DEFAULT_BG, "inverse swap: fg becomes default bg");
        assert_eq!(bg, DEFAULT_FG, "inverse swap: bg becomes default fg");
        assert!(
            has_bg,
            "inverse must force has_explicit_bg=true so the swap is painted"
        );
    }

    #[test]
    fn inverse_with_explicit_colors_swaps_them() {
        let c = cell(
            Some(EXPLICIT_FG),
            Some(EXPLICIT_BG),
            Style {
                inverse: true,
                ..Style::default()
            },
        );
        let (fg, bg, has_bg) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, None);
        assert_eq!(fg, EXPLICIT_BG);
        assert_eq!(bg, EXPLICIT_FG);
        assert!(has_bg);
    }

    /// Boundary: inverse on a cell that has only an explicit fg (no
    /// explicit bg). The default bg sits in the bg slot before the
    /// swap, so after inverse the effective fg should be `default_bg`
    /// and the effective bg should be the originally-explicit fg.
    #[test]
    fn inverse_with_only_explicit_fg_swaps_default_bg_into_fg() {
        let c = cell(
            Some(EXPLICIT_FG),
            None,
            Style {
                inverse: true,
                ..Style::default()
            },
        );
        let (fg, bg, has_bg) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, None);
        assert_eq!(fg, DEFAULT_BG);
        assert_eq!(bg, EXPLICIT_FG);
        assert!(has_bg);
    }

    /// Mirror of the above: explicit bg only, no explicit fg. After
    /// the inverse swap the effective fg is the explicit bg and the
    /// effective bg is the default fg.
    #[test]
    fn inverse_with_only_explicit_bg_swaps_default_fg_into_bg() {
        let c = cell(
            None,
            Some(EXPLICIT_BG),
            Style {
                inverse: true,
                ..Style::default()
            },
        );
        let (fg, bg, has_bg) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, None);
        assert_eq!(fg, EXPLICIT_BG);
        assert_eq!(bg, DEFAULT_FG);
        assert!(has_bg);
    }

    #[test]
    fn bold_default_fg_uses_bold_accent_when_provided() {
        let c = cell(
            None,
            None,
            Style {
                bold: true,
                ..Style::default()
            },
        );
        let (fg, _, _) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, Some(BOLD));
        assert_eq!(fg, BOLD);
    }

    #[test]
    fn bold_with_explicit_fg_keeps_the_explicit_fg() {
        let c = cell(
            Some(EXPLICIT_FG),
            None,
            Style {
                bold: true,
                ..Style::default()
            },
        );
        let (fg, _, _) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, Some(BOLD));
        assert_eq!(
            fg, EXPLICIT_FG,
            "bold accent must not override explicit SGR fg (e.g. bold red stays red)"
        );
    }

    #[test]
    fn bold_with_inverse_does_not_apply_bold_accent_to_swapped_bg() {
        // After inverse, fg = default_bg. Applying bold_color here
        // would land it in the bg position and produce the wrong
        // visual. The legacy guard `!cell.Inverse` prevents this.
        let c = cell(
            None,
            None,
            Style {
                bold: true,
                inverse: true,
                ..Style::default()
            },
        );
        let (fg, bg, _) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, Some(BOLD));
        assert_eq!(fg, DEFAULT_BG, "post-inverse fg must remain default_bg");
        assert_eq!(bg, DEFAULT_FG, "post-inverse bg must remain default_fg");
    }

    #[test]
    fn bold_color_none_disables_the_accent() {
        let c = cell(
            None,
            None,
            Style {
                bold: true,
                ..Style::default()
            },
        );
        let (fg, _, _) = resolve_cell_colors(&c, DEFAULT_FG, DEFAULT_BG, None);
        assert_eq!(
            fg, DEFAULT_FG,
            "bold_color=None must leave default fg unchanged"
        );
    }

    /// End-to-end: feed `\e[1mX` through libghostty, walk via
    /// `RenderState`, and confirm the resolver picks up the theme's
    /// `bold-color` accent for the bold default-fg `X` cell. Pins the
    /// full chain that the renderer relies on, so a regression
    /// anywhere (parser, plumbing, resolver) fails this test instead
    /// of a screenshot eyeball.
    #[test]
    fn bold_default_fg_through_libghostty_uses_theme_bold_color() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 0,
        })
        .expect("Terminal::new");
        terminal.vt_write(b"\x1b[1mX");

        let mut render_state = RenderState::new().expect("RenderState::new");
        render_state.update(&terminal).expect("update");

        let bold_accent = ColorRgb::new(0xaa, 0xbb, 0xcc);

        let mut effective_fg: Option<ColorRgb> = None;
        render_state
            .walk(&terminal, |row, cell| {
                if row == 0 && cell.text == "X" {
                    let (fg, _, _) =
                        resolve_cell_colors(&cell, DEFAULT_FG, DEFAULT_BG, Some(bold_accent));
                    effective_fg = Some(fg);
                }
            })
            .expect("walk");

        assert_eq!(
            effective_fg,
            Some(bold_accent),
            "bold default-fg X must resolve to the theme bold-color accent"
        );
    }
}
