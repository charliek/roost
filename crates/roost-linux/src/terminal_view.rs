//! Cell renderer over libghostty-vt — Phase 7 commit 4.
//!
//! Owns a [`roost_vt::Terminal`] + a [`roost_vt::RenderState`] and
//! paints into a [`gtk::DrawingArea`]'s Cairo context. Multi-pass
//! walk:
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
//! can eyeball the renderer on Mac Homebrew GTK.

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
use crate::focus::safe_grab_focus;
use crate::key_encoder;
use crate::keybind::{self, AccelMods};
use crate::paste_image;
use crate::shell_escape;
use crate::sprite;
use crate::theme::Theme;

/// Default cell grid the terminal allocates with. Cell pixels are
/// reported to libghostty so its OSC 14 / size-report responses are
/// accurate; the grid is reflowed per-resize in commit 5 onwards.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Cursor blink half-period. 530ms matches the Mac UI.
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

    /// Construct with a custom theme, optional font overrides, a
    /// `copy-on-select` mode, and the `word-break-chars` extra
    /// word-char set. The App passes user-supplied `font_family` +
    /// `font_size_pt` + `copy_on_select` + `word_break_chars` from
    /// `~/.config/roost/config.conf` (defaults applied when absent).
    /// The theme's palette is pushed into libghostty so SGR cells
    /// (`ls --color`, `git diff`) flip to the theme's reds / greens.
    pub fn with_theme_font_and_copy(
        theme: Theme,
        font_family: Option<&str>,
        font_size_pt: Option<f64>,
        copy_on_select: CopyOnSelect,
        word_break_chars: String,
        link_modifier: AccelMods,
    ) -> Self {
        let view = Self::with_theme(theme);
        view.apply_font(font_family, font_size_pt);
        {
            let mut s = view.state.borrow_mut();
            s.copy_on_select = copy_on_select;
            s.word_break_chars = word_break_chars;
            s.link_modifier = link_modifier;
        }
        view
    }

    /// Snapshot the live terminal viewport as text for `tab.dump`.
    /// Main-thread-only — touches the libghostty handle + render state.
    pub fn dump(&self) -> TerminalDump {
        self.state.borrow_mut().dump_text()
    }

    /// Snapshot the live viewport through the same color resolver the
    /// real `paint` path runs (including `theme.bold_color`), for
    /// the `tab.dump_resolved` IPC op. Closes #142's call-site
    /// gap: a test can assert that a bold cell ends up colored by
    /// `bold_color`, which only holds if the production resolver
    /// call site is plumbed correctly. Main-thread-only — same
    /// libghostty + render-state requirements as `dump`.
    pub fn dump_resolved_cells(&self) -> roost_linux::ipc::ResolvedCellsData {
        self.state.borrow_mut().dump_resolved_cells()
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
            // 2000 rows of off-screen history matches the Mac UI's M6
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
        // `context_set_font_options` directly, so no extra FFI shim is
        // needed to set them.
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
            // Initialized to false so the first real
            // `focus_ctrl.connect_enter` is an edge transition that
            // emits CSI I under mode 1004. With `has_focus = true`
            // at init, the dedup in connect_enter would swallow the
            // first focus-in event the user generates after the
            // widget is realized.
            has_focus: false,
            scrolled_back: false,
            scroll_accum: 0.0,
            selection: None,
            copy_on_select: CopyOnSelect::default(),
            input_callback: None,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            on_resize: None,
            hover_url: None,
            link_modifier: keybind::default_link_modifier(),
            link_mod_held: false,
            url_opener: Rc::new(|uri| {
                if let Err(err) = crate::url_launcher::open_uri(uri) {
                    tracing::warn!(uri, ?err, "url launcher failed");
                }
            }),
            link_click_consumed_this_gesture: false,
            pointer_inside: false,
            last_applied_cursor_name: None,
            multi_click_consumed_this_gesture: false,
            word_break_chars: roost_linux::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
            tracking_press_consumed_this_gesture: false,
            right_tracking_press_consumed_this_gesture: false,
            motion_emitter: roost_linux::mouse_routing::MotionEmitter::new(),
            current_osc_shape: String::new(),
            was_alt_screen_active: false,
            osc_shape_set_in_this_chunk: false,
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
                let mut s = state.borrow_mut();
                // Dedup: GTK can fire focus-in twice in a row during a
                // window-manager activate cycle (compositor grab +
                // key-focus). Mode 1004 emit must only fire on the
                // edge — emitting CSI I twice would land junk at a
                // shell prompt and confuse mode-1004 TUIs.
                if s.has_focus {
                    return;
                }
                s.has_focus = true;
                s.cursor_blink_on = true;
                let bytes = s.encode_focus_bytes_if_active(true);
                let cb = s.input_callback.clone();
                drop(s);
                if !bytes.is_empty() {
                    if let Some(cb) = cb {
                        cb(bytes);
                    }
                }
                widget.queue_draw();
            }
        });
        focus_ctrl.connect_leave({
            let state = state.clone();
            let widget = widget.clone();
            move |_| {
                let mut s = state.borrow_mut();
                if !s.has_focus {
                    // Dedup symmetric to connect_enter; spurious
                    // focus-leave during a WM activate cycle.
                    return;
                }
                s.has_focus = false;
                // Drop the cursor-name cache: some compositors
                // reset the OS cursor on focus-loss, so a cached
                // `Some("pointer")` would skip re-applying on focus
                // return — leaving the cursor visually stuck on
                // GTK's default even though we believe we already
                // pushed `pointer`. Symmetric to the leave-on-
                // pointer-exit cache drop.
                s.last_applied_cursor_name = None;
                // Mac's `handleWindowDidResignKey` clears the URL
                // hover so the underline + Ctrl-hand cursor don't
                // survive off-window. GTK parity.
                let hover_was_set = s.hover_url.is_some();
                if hover_was_set {
                    s.hover_url = None;
                    s.apply_link_cursor(&widget);
                }
                let bytes = s.encode_focus_bytes_if_active(false);
                let cb = s.input_callback.clone();
                drop(s);
                if !bytes.is_empty() {
                    if let Some(cb) = cb {
                        cb(bytes);
                    }
                }
                widget.queue_draw();
            }
        });
        widget.add_controller(focus_ctrl);

        // Pointer tracking: the scroll controller doesn't carry a
        // position, so keep the latest hover here for the wheel-as-button
        // reports (which must name the cell under the pointer).
        //
        // PR C: the same motion controller also drives clickable-link
        // hover state — recompute the URL under the pointer on every
        // motion event, so the underline + hand cursor track the
        // pointer even while Ctrl stays held.
        let motion_ctrl = EventControllerMotion::new();
        motion_ctrl.connect_enter({
            let state = state.clone();
            let widget = widget.clone();
            move |_, _x, _y| {
                let mut s = state.borrow_mut();
                s.pointer_inside = true;
                // A background-tab OSC 22 drain may have set
                // `current_osc_shape` while we were unfocused; apply
                // now that the pointer is over us so the shape lands
                // without waiting for the next motion event.
                s.apply_current_cursor_shape(&widget);
            }
        });
        motion_ctrl.connect_motion({
            let state = state.clone();
            let widget = widget.clone();
            move |ctrl, x, y| {
                let raw_mods = ctrl.current_event_state();
                let mut s = state.borrow_mut();
                s.pointer = (x, y);
                s.pointer_inside = true;
                let link_held = s.link_modifier_held(raw_mods);
                s.link_mod_held = link_held;
                let cell_w = s.cell_metrics.cell_width;
                let cell_h = s.cell_metrics.cell_height;
                let next = if link_held {
                    cell_at_inner(x, y, cell_w, cell_h)
                        .and_then(|(col, row)| s.compute_hover_url(col, row))
                } else {
                    None
                };
                let hover_changed = next != s.hover_url;
                if hover_changed {
                    s.hover_url = next;
                    s.apply_link_cursor(&widget);
                }
                // Mode 1003 (any-event) motion: report movement to
                // the PTY with no button held. Throttled to 60 Hz +
                // per-cell-dedup inside `emit_mouse_tracking`. URL
                // hover detection still runs above regardless of
                // mode (Ctrl-hover is a peek gesture even under a
                // mouse-tracking TUI).
                if s.terminal.mouse_tracking() && s.terminal.mode_get(1003) {
                    let mods = key_encoder::translate_mods(raw_mods);
                    let bytes = s.emit_mouse_tracking(
                        roost_linux::mouse_routing::MouseRoutingAction::Motion,
                        None,
                        mods,
                        x,
                        y,
                    );
                    if !bytes.is_empty() {
                        let cb = s.input_callback.clone();
                        drop(s);
                        if hover_changed {
                            widget.queue_draw();
                        }
                        if let Some(cb) = cb {
                            cb(bytes);
                        }
                        return;
                    }
                }
                drop(s);
                if hover_changed {
                    widget.queue_draw();
                }
            }
        });
        motion_ctrl.connect_leave({
            let state = state.clone();
            let widget = widget.clone();
            move |_| {
                let mut s = state.borrow_mut();
                s.pointer_inside = false;
                // Drop the cached cursor-name so a re-enter pushes a
                // fresh `set_cursor_from_name` (the OS may have
                // changed the cursor under us while the pointer was
                // away).
                s.last_applied_cursor_name = None;
                if s.hover_url.is_some() {
                    s.hover_url = None;
                    s.apply_link_cursor(&widget);
                    drop(s);
                    widget.queue_draw();
                }
            }
        });
        widget.add_controller(motion_ctrl);

        // Track link-modifier press/release so the underline + cursor
        // appear even without pointer movement (user presses the
        // modifier while the pointer is already over a URL). GTK4's
        // `EventControllerKey` exposes a `modifiers` signal for exactly
        // this.
        let modifier_ctrl = EventControllerKey::new();
        modifier_ctrl.connect_modifiers({
            let state = state.clone();
            let widget = widget.clone();
            move |_ctrl, mods| {
                let mut s = state.borrow_mut();
                let link_held = s.link_modifier_held(mods);
                if s.link_mod_held == link_held {
                    return glib::Propagation::Proceed;
                }
                s.link_mod_held = link_held;
                let (px, py) = s.pointer;
                let cell_w = s.cell_metrics.cell_width;
                let cell_h = s.cell_metrics.cell_height;
                // Only recompute hover if the pointer is currently
                // inside the widget — a modifier press with the pointer
                // outside (the user Tabbed back to the window with the
                // modifier already held but the pointer elsewhere) must
                // not resurrect a stale underline at the last-known
                // in-bounds cell.
                let next = if link_held && s.pointer_inside {
                    cell_at_inner(px, py, cell_w, cell_h)
                        .and_then(|(col, row)| s.compute_hover_url(col, row))
                } else {
                    None
                };
                if next != s.hover_url {
                    s.hover_url = next;
                    s.apply_link_cursor(&widget);
                    drop(s);
                    widget.queue_draw();
                }
                glib::Propagation::Proceed
            }
        });
        widget.add_controller(modifier_ctrl);

        // Scroll wheel: 3 modes, matching the Mac UI. Discrete
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

        // Accept files / text dropped onto the terminal: insert the
        // (shell-escaped) path through the same bracketed-paste path as
        // Ctrl+Shift+V, so a dragged screenshot resolves as an image
        // attachment in Claude Code / Codex. See `install_file_drop`.
        install_file_drop(&widget, &state);

        // Drag selection. Anchor on press, update on drag, on release
        // the selection becomes "committed" until the user clicks
        // elsewhere or types (commit 7+'s `clearSelection` flow).
        // Rows are captured in screen-y (scrollback-stable) space so
        // the highlight scrolls with the content.
        let drag = GestureDrag::new();
        drag.connect_drag_begin({
            let state = state.clone();
            let widget = widget.clone();
            move |g, x, y| {
                let mut s = state.borrow_mut();
                s.link_click_consumed_this_gesture = false;
                s.tracking_press_consumed_this_gesture = false;
                // PR #176: the parallel GestureClick controller (added
                // below) may have already claimed this press for a
                // double/triple-click word/line expansion. If so, skip
                // the selection-mutation path so the word selection it
                // set survives the gesture. The flag is cleared in
                // `drag_end` so the next gesture starts fresh.
                if s.multi_click_consumed_this_gesture {
                    return;
                }
                // PR C: link-modifier-click on a URL opens it and skips
                // selection setup. Preserves any pre-existing
                // selection — the user gets the new browser tab
                // without losing the text they just copied.
                let link_held = s.link_modifier_held(g.current_event_state());
                if link_held {
                    if let Some((col, row)) = s.cell_at(x, y, &widget) {
                        if let Some(hov) = s.compute_hover_url(col, row) {
                            let url = hov.url.clone();
                            s.hover_url = Some(hov);
                            s.link_click_consumed_this_gesture = true;
                            let opener = s.url_opener.clone();
                            drop(s);
                            opener(&url);
                            widget.queue_draw();
                            return;
                        }
                    }
                }
                // Mouse-tracking apps (TUIs with `\x1b[?1000h` /
                // `?1002h` enabled) get press reports at the pointer's
                // cell. libvt's encoder gates internally on the
                // negotiated mode/format. URL precedence above wins
                // over tracking; matches the Mac UI's mouseDown order.
                if s.terminal.mouse_tracking() {
                    let mods = key_encoder::translate_mods(g.current_event_state());
                    let bytes = s.emit_mouse_tracking(
                        roost_linux::mouse_routing::MouseRoutingAction::Press,
                        Some(roost_linux::mouse_routing::MouseRoutingButton::Left),
                        mods,
                        x,
                        y,
                    );
                    if !bytes.is_empty() {
                        s.tracking_press_consumed_this_gesture = true;
                        let cb = s.input_callback.clone();
                        drop(s);
                        if let Some(cb) = cb {
                            cb(bytes);
                        }
                        return;
                    } else {
                        // Encoder declined but tracking was on — still
                        // mark the press consumed so a subsequent
                        // release / drag doesn't leak into selection.
                        // Mirrors the Mac UI's
                        // `trackingPressConsumedThisGesture` X10 fix.
                        s.tracking_press_consumed_this_gesture = true;
                        drop(s);
                        return;
                    }
                }
                // Start a fresh selection, or clear any stale one if
                // the viewport → screen conversion fails (terminal
                // handle not ready, cell coords out of range).
                // `committed = false` — a drag that never extends is
                // a "click without drag" and shouldn't render a
                // selection rect. drag_update flips it to true on the
                // first movement.
                s.selection = s.cell_at(x, y, &widget).and_then(|(col, row)| {
                    let screen_y = s.screen_y_for_viewport_row(row)?;
                    Some(Selection {
                        anchor_col: col,
                        anchor_screen_y: screen_y,
                        cursor_col: col,
                        cursor_screen_y: screen_y,
                        committed: false,
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
                // PR C: don't mutate selection while a Ctrl-click
                // gesture is being consumed for URL opening.
                if s.link_click_consumed_this_gesture {
                    return;
                }
                // PR #176: don't shrink a double/triple-click word
                // selection back to a single cell when the pointer
                // wobbles after the press. Drag-after-multi-click is
                // intentionally a no-op (the "expand by word" gesture
                // ghostty + iTerm2 ship is deferred — see plan).
                if s.multi_click_consumed_this_gesture {
                    return;
                }
                // Tracking owns this gesture (press was forwarded):
                // forward drag motion under mode 1002, but never
                // touch the selection. Encoder declines (mode 1000
                // only) still skip selection — that's the X10 fix.
                if s.tracking_press_consumed_this_gesture {
                    if let Some((start_x, start_y)) = g.start_point() {
                        let x = start_x + dx;
                        let y = start_y + dy;
                        let mods = key_encoder::translate_mods(g.current_event_state());
                        let bytes = s.emit_mouse_tracking(
                            roost_linux::mouse_routing::MouseRoutingAction::Motion,
                            Some(roost_linux::mouse_routing::MouseRoutingButton::Left),
                            mods,
                            x,
                            y,
                        );
                        if !bytes.is_empty() {
                            let cb = s.input_callback.clone();
                            drop(s);
                            if let Some(cb) = cb {
                                cb(bytes);
                            }
                            return;
                        }
                    }
                    return;
                }
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
            move |g, _x, _y| {
                let mut s = state.borrow_mut();
                // PR C: a Ctrl-click consumed by URL launch must not
                // run copy-on-select against any prior selection.
                // Clear the gesture flag for the next gesture.
                if s.link_click_consumed_this_gesture {
                    s.link_click_consumed_this_gesture = false;
                    return;
                }
                // PR #176: the parallel GestureClick already wrote the
                // word/line selection to PRIMARY. Skip the redundant
                // write here (matches the Mac port's mouseUp short-
                // circuit) and clear the flag for the next gesture.
                if s.multi_click_consumed_this_gesture {
                    s.multi_click_consumed_this_gesture = false;
                    return;
                }
                // Tracking owns this gesture: emit a release report
                // and skip copy-on-select. If the encoder declines
                // (X10 / mode 9 doesn't report releases) the
                // gesture STILL must not run copy-on-select — the
                // tracking-press-consumed flag covers that. Clear
                // the flag for the next gesture either way.
                if s.tracking_press_consumed_this_gesture {
                    s.tracking_press_consumed_this_gesture = false;
                    // Use the gesture's start_point + accumulated
                    // offset for the release coord — GestureDrag's
                    // drag_end callback doesn't carry a fresh
                    // (x, y), so reconstruct from start+offset.
                    let (rx, ry) = g.offset().unwrap_or((0.0, 0.0));
                    let (sx, sy) = g.start_point().unwrap_or((0.0, 0.0));
                    let mods = key_encoder::translate_mods(g.current_event_state());
                    let bytes = s.emit_mouse_tracking(
                        roost_linux::mouse_routing::MouseRoutingAction::Release,
                        Some(roost_linux::mouse_routing::MouseRoutingButton::Left),
                        mods,
                        sx + rx,
                        sy + ry,
                    );
                    if !bytes.is_empty() {
                        let cb = s.input_callback.clone();
                        drop(s);
                        if let Some(cb) = cb {
                            cb(bytes);
                        }
                    }
                    return;
                }
                let mode = s.copy_on_select;
                drop(s);
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

        // PR #176: double-/triple-click word/line selection. A
        // separate `GestureClick` controller is the cleanest gtk4-rs
        // surface for n_press dispatch — `GestureDrag` doesn't carry
        // press-count info. The two controllers coexist; GTK's
        // built-in resolution lets `GestureDrag` keep handling drags
        // while the click gesture catches multi-presses on the same
        // primary button. The handler claims the event sequence for
        // n_press >= 2 so the drag gesture's begin sees the
        // `multi_click_consumed_this_gesture` flag and skips the
        // single-cell anchor.
        let multi_click = gtk4::GestureClick::new();
        multi_click.set_button(gtk4::gdk::BUTTON_PRIMARY);
        multi_click.connect_pressed({
            let state = state.clone();
            let widget = widget.clone();
            move |gesture, n_press, x, y| {
                if n_press < 2 {
                    return;
                }
                let mut s = state.borrow_mut();
                // Don't fight the link-modifier-double-click → URL-open
                // path: drag_begin will see the modifier held and route
                // to the URL launcher (PR #175 behavior preserved).
                // Leaving the multi-click flag CLEAR lets drag_begin's
                // regular branches run.
                if s.link_modifier_held(gesture.current_event_state()) {
                    return;
                }
                let Some((col, row)) = s.cell_at(x, y, &widget) else {
                    return;
                };
                let word_break_chars = s.word_break_chars.clone();
                let row_text = s.text_for_viewport_row(row);
                let click_count = u8::try_from(n_press).unwrap_or(u8::MAX);
                let Some(span) = click_count_span(&row_text, col, click_count, &word_break_chars)
                else {
                    return;
                };
                let Some(screen_y) = s.screen_y_for_viewport_row(row) else {
                    return;
                };
                s.selection = Some(Selection {
                    anchor_col: span.col0,
                    anchor_screen_y: screen_y,
                    cursor_col: span.col1,
                    cursor_screen_y: screen_y,
                    // `committed = true` so a single-letter word
                    // (col0 == col1) still renders + copies; Codex
                    // flagged on PR #177 review.
                    committed: true,
                });
                s.multi_click_consumed_this_gesture = true;
                let mode = s.copy_on_select;
                drop(s);
                // Copy-on-select right here. The Mac port handles it
                // inside `handleClickCount`; the GTK side mirrors
                // that so a click that never drags still writes the
                // word/line to PRIMARY. The drag_end short-circuit
                // above prevents the redundant write.
                if mode != CopyOnSelect::Off {
                    if let Some(text) = selection_text(&state) {
                        clipboard::write(clipboard::Target::Primary, &text);
                        if mode == CopyOnSelect::Clipboard {
                            clipboard::write(clipboard::Target::Clipboard, &text);
                        }
                    }
                }
                // Deliberately NOT calling `gesture.set_state(Claimed)`
                // — Codex flagged on PR #177 that claiming this
                // sequence denies the parallel `GestureDrag`, which
                // can then skip emitting `drag_end` and leave
                // `multi_click_consumed_this_gesture` stuck on. The
                // flag alone is enough to gate `drag_update` and
                // `drag_end` for this gesture; the drag controller's
                // `drag_begin` either already ran (single-cell
                // selection now overwritten by the word span above)
                // or runs after this handler and returns early on the
                // flag.
                let _ = gesture;
                widget.queue_draw();
            }
        });
        widget.add_controller(multi_click);

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

        // Right-button forwarding under mouse tracking (Tier 2). No
        // legacy fallback path on GTK (no context menu yet), so this
        // controller is a no-op outside tracking. Same encoder path
        // as the left button via the routing helper.
        let right_click = gtk4::GestureClick::new();
        right_click.set_button(gtk4::gdk::BUTTON_SECONDARY);
        right_click.connect_pressed({
            let state = state.clone();
            move |g, _n_press, x, y| {
                forward_right_button(
                    &state,
                    roost_linux::mouse_routing::MouseRoutingAction::Press,
                    g.current_event_state(),
                    x,
                    y,
                );
            }
        });
        right_click.connect_released({
            let state = state.clone();
            move |g, _n_press, x, y| {
                forward_right_button(
                    &state,
                    roost_linux::mouse_routing::MouseRoutingAction::Release,
                    g.current_event_state(),
                    x,
                    y,
                );
            }
        });
        widget.add_controller(right_click);

        // Click-to-focus: a pointer press grabs keyboard focus, so a
        // click starts typing — and is the recovery path when focus
        // landed elsewhere (a sidebar row, the tab bar). AppKit gives
        // this free via `acceptsFirstResponder`; GTK4 does not focus a
        // custom `DrawingArea` on click, so we wire it explicitly. A
        // dedicated all-button (button 0) gesture — not folded into
        // `GestureDrag` — keeps focus off the selection/paste paths and
        // never claims the sequence, so it can't deny the parallel drag
        // (the PR #177 hazard).
        let focus_click = gtk4::GestureClick::new();
        focus_click.set_button(0);
        focus_click.connect_pressed({
            let widget = widget.clone();
            move |_gesture, _n_press, _x, _y| {
                safe_grab_focus(&widget);
            }
        });
        widget.add_controller(focus_click);

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
        let mut s = self.state.borrow_mut();
        // Snapshot the prior alt-screen state so we can detect an
        // alt→primary transition and reset the OSC 22 cursor shape
        // after vt_write applies the bytes. Note the cross-file
        // ordering: app.rs drains the OSC scanner FIRST (calling
        // `apply_mouse_shape` for each MouseShape event in this
        // chunk, which sets `osc_shape_set_in_this_chunk`), THEN
        // calls vt_write. So when we read the flag below, it
        // reflects the chunk's OSC 22 activity; we clear it at the
        // end to prepare for the next chunk's drain.
        s.was_alt_screen_active = s.terminal.active_screen() == ActiveScreen::Alternate;
        s.terminal.vt_write(bytes);
        let alt_now = s.terminal.active_screen() == ActiveScreen::Alternate;
        // Alt→primary transition resets OSC 22 unless the same chunk
        // also processed an OSC 22 (the TUI explicitly chose its
        // own shape on the way out — e.g. strix sends `default`
        // immediately before the alt-exit CSI).
        if s.was_alt_screen_active
            && !alt_now
            && !s.current_osc_shape.is_empty()
            && !s.osc_shape_set_in_this_chunk
        {
            s.current_osc_shape.clear();
            s.apply_current_cursor_shape(&self.widget);
        }
        s.osc_shape_set_in_this_chunk = false;
        drop(s);
        self.widget.queue_draw();
    }

    /// Apply an OSC 22 W3C cursor name. Called from `app.rs`'s OSC
    /// drain when a `MouseShape` event arrives for the matching
    /// tab. Stores the name + tries to apply via
    /// `set_cursor_from_name` (the apply is gated on
    /// pointer-is-over-this-view; a background tab's drain stores
    /// but doesn't change the visible cursor).
    pub fn apply_mouse_shape(&self, name: &str) {
        let mut s = self.state.borrow_mut();
        s.current_osc_shape = name.to_string();
        s.osc_shape_set_in_this_chunk = true;
        s.apply_current_cursor_shape(&self.widget);
    }

    /// Read the active OSC 22 W3C cursor name (canonicalised:
    /// empty body and `"default"` both return `"default"`). Used by
    /// the `app.cursor_shape` IPC op.
    pub fn current_cursor_shape_name(&self) -> String {
        let s = self.state.borrow();
        roost_linux::mouse_routing::canonical_cursor_shape(&s.current_osc_shape)
    }

    /// Drive a synthetic mouse event into the production
    /// `emit_mouse_tracking` path at cell-grid coords. Backs the
    /// test-mode `tab.dispatch_mouse_event` IPC op. Returns the
    /// encoded bytes (forwarded to the PTY input by the caller).
    pub fn ipc_dispatch_mouse_event(
        &self,
        kind: roost_linux::mouse_routing::MouseRoutingAction,
        button: Option<roost_linux::mouse_routing::MouseRoutingButton>,
        cell_col: u32,
        cell_row: u32,
        mods: roost_vt::Mods,
    ) -> Vec<u8> {
        let mut s = self.state.borrow_mut();
        let cw = s.cell_metrics.cell_width.max(1.0);
        let ch = s.cell_metrics.cell_height.max(1.0);
        // Place the point at the cell's center so the encoder's
        // floor-division round-trips back to the requested cell.
        let x = (cell_col as f64 + 0.5) * cw;
        let y = (cell_row as f64 + 0.5) * ch;
        s.emit_mouse_tracking(kind, button, mods, x, y)
    }

    /// Drive a synthetic focus state change. Backs the test-mode
    /// `app.set_window_focus` IPC op. Returns the encoded bytes for
    /// the caller to push to the PTY input.
    pub fn ipc_set_window_focus(&self, focused: bool) -> Vec<u8> {
        let s = self.state.borrow();
        s.encode_focus_bytes_if_active(focused)
    }

    /// Snapshot libghostty's currently-effective default colors. The
    /// OSC drain task answers `OSC 10/11/12;?` queries from this so
    /// any mid-session `OSC 11;rgb:…` set is reflected by the next
    /// query reply.
    pub fn live_colors(&self) -> roost_vt::Result<roost_vt::Colors> {
        self.state.borrow().terminal.live_colors()
    }

    /// Live 256-entry palette — the OSC drain answers `OSC 4;Ps;?`
    /// queries from this, reflecting any mid-session `OSC 4;Ps;rgb:…`
    /// set. See [`Terminal::live_palette`].
    pub fn live_palette(&self) -> roost_vt::Result<[roost_vt::ColorRgb; 256]> {
        self.state.borrow().terminal.live_palette()
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
                    // IPC-driven selections are deliberate commits;
                    // single-cell sets must render + copy too.
                    committed: true,
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

    /// Drive the production word-/line-expansion dispatch (the same
    /// `click_count_span` the GestureClick handler runs) from explicit
    /// coords, then commit the resulting span as the selection.
    /// `click_count` is 2 (word) or 3+ (line); anything below 2 is
    /// rejected upstream. Returns `Some((col0, col1, text))` on success
    /// and `None` when the dispatch falls through (whitespace
    /// double-click, out-of-range row, terminal not ready). The
    /// `tab.expand_selection_at` test-mode IPC op exposes this on
    /// both UIs so the e2e suite can pin word/line expansion without
    /// synthetic mouse events.
    pub fn expand_selection_at(
        &self,
        col: u16,
        row: u16,
        click_count: u8,
    ) -> Option<(u16, u16, Option<String>)> {
        let span = {
            let mut s = self.state.borrow_mut();
            let row_text = s.text_for_viewport_row(row);
            let word_break_chars = s.word_break_chars.clone();
            click_count_span(&row_text, col, click_count, &word_break_chars)?
        };
        if !self.set_selection((span.col0, row), (span.col1, row)) {
            return None;
        }
        let text = selection_text(&self.state);
        Some((span.col0, span.col1, text))
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
                // and clears any active selection — even when the key
                // encodes to nothing (dead keys / IME composition /
                // unmapped), since typing always overrides a selection.
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
    /// Consulted before encoding a key to decide whether to snap.
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
    /// Active link-hover. `Some` when the pointer is over a URL **and**
    /// the link modifier is held; otherwise `None`. Populated by the
    /// motion controller from either OSC 8 explicit hyperlinks or a
    /// regex match on the row text. Consumed by `paint` (underline) and
    /// the click controller (open URL).
    hover_url: Option<HoverUrl>,
    /// Which held modifier reveals + opens a URL (Cmd on macOS, Alt on
    /// Linux by default; overridable via `link-modifier` config). The
    /// motion / key / click controllers test the live event state
    /// against this through [`Self::link_modifier_held`].
    link_modifier: AccelMods,
    /// Last-known link-modifier state. The motion + key controllers
    /// each refresh this; the click controller reads it to decide
    /// whether the press should open a URL or fall through to
    /// drag-selection.
    link_mod_held: bool,
    /// Pluggable launcher seam. Production routes to
    /// `url_launcher::open_uri`; tests substitute a stub. `Rc` so the
    /// gesture closure can clone cheaply.
    url_opener: Rc<dyn Fn(&str)>,
    /// Set by the click controller when a Ctrl-click opens a URL. The
    /// drag controller's `drag_begin` sees it on the next press and
    /// skips selection setup so a Ctrl-click doesn't drop a stray
    /// single-cell selection at the URL's anchor.
    link_click_consumed_this_gesture: bool,
    /// True while the pointer is inside the widget bounds. Updated
    /// by motion enter/leave. Read by the modifier-change handler so
    /// pressing Ctrl with the pointer outside the widget doesn't
    /// resurrect a stale underline at the last-known cell.
    pointer_inside: bool,
    /// Set when a double/triple-click commits a word/line selection
    /// through the n_press dispatch. Read by `drag_end` so the
    /// gesture skips the copy-on-select branch (n_press dispatch
    /// already wrote the selection to PRIMARY) and the drag-update
    /// branch (so a tiny pointer wobble after the multi-click
    /// doesn't collapse the word selection to a single cell).
    multi_click_consumed_this_gesture: bool,
    /// Extra word-char set used by double-click word expansion in the
    /// n_press dispatch. Loaded from `RoostConfig.word_break_chars`
    /// at view construction; default matches Ghostty's `_-.+~/:@%`
    /// (file paths + URLs stay whole on double-click).
    word_break_chars: String,

    // ---- Mouse-tracking + OSC 22 (PR B mirror of Mac PR A) ----
    /// Set on a successful press-forward through the encoder. Read
    /// by drag-update / drag-end so the selection / copy-on-select
    /// paths skip when the press was consumed by tracking — even if
    /// the matching release encoder returns empty (X10 / mode 9
    /// doesn't report releases). Cleared on drag-end.
    tracking_press_consumed_this_gesture: bool,
    /// Right-button equivalent of the left's
    /// `tracking_press_consumed_this_gesture`. Set on a successful
    /// right-press forward; read by right-release so a TUI that
    /// disables mouse tracking between press and release still gets
    /// the matching release report (or, when the TUI ENABLES
    /// tracking between press and release, the release is suppressed
    /// to avoid an orphan event). Cleared on right-release.
    right_tracking_press_consumed_this_gesture: bool,
    /// 60 Hz cap + per-cell dedup for mode 1003 motion-no-button
    /// reports. Sibling of the Mac UI's MotionEmitter.
    motion_emitter: roost_linux::mouse_routing::MotionEmitter,
    /// Last applied W3C cursor name from OSC 22. Empty string (or
    /// `"default"`) means the platform default arrow. Reset to empty
    /// on alt-screen exit so a hung TUI's stale `pointer` doesn't
    /// survive shell reset.
    current_osc_shape: String,
    /// Prior alt-screen state so the OSC drain can detect an alt→
    /// primary transition and reset `current_osc_shape`.
    was_alt_screen_active: bool,
    /// Set when the OSC drain processed a MouseShape event in the
    /// current chunk. Skips the alt-exit reset when the TUI
    /// explicitly set its own shape on the way out (e.g. strix
    /// sends `default` immediately before the alt-exit CSI).
    osc_shape_set_in_this_chunk: bool,
    /// Last GTK cursor name pushed via `set_cursor_from_name`.
    /// `apply_current_cursor_shape` short-circuits when the same
    /// name would be pushed again — under steady-state strix hover
    /// a TUI may re-assert OSC 22 `pointer` on every motion event,
    /// and re-pushing the identical name is wasted FFI.
    last_applied_cursor_name: Option<&'static str>,
}

/// Active URL hover. `col0` is the URL's first column (inclusive);
/// `col1` is the last column (inclusive); `row` is the viewport row.
/// Same shape as the Mac UI's `HoverURL` so future refactors that
/// share more logic land symmetrically.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HoverUrl {
    col0: u16,
    col1: u16,
    row: u16,
    url: String,
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
    /// True when the selection was set as a deliberate commit (the
    /// multi-click n_press dispatch, `set_selection` from IPC) rather
    /// than as a single-cell `drag_begin` anchor that the user hasn't
    /// extended yet. Codex flagged on PR #177 that a double-click on
    /// a single-letter word (e.g. `i`) returns a `(col, col)` span —
    /// geometrically equal to a click-without-drag, but the user
    /// expects to see + copy it. `committed` is the bit that
    /// distinguishes the two so paint + `selection_text` render the
    /// single-cell case.
    committed: bool,
}

impl Selection {
    fn is_empty(&self) -> bool {
        self.anchor_col == self.cursor_col && self.anchor_screen_y == self.cursor_screen_y
    }

    /// Should the renderer paint this selection / copy-on-select emit
    /// text for it? A committed single-cell span (e.g. double-click
    /// on `i`) renders even though `is_empty` is geometrically true;
    /// an in-progress drag at the anchor cell does not.
    fn is_visible(&self) -> bool {
        self.committed || !self.is_empty()
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

    /// Walk the viewport through the same `resolve_cell_colors` call
    /// `paint` runs, emitting a `ResolvedCellsData` for the
    /// `tab.dump_resolved` IPC op. Pulls the theme's `bold_color`
    /// out of `self.theme` exactly the way `paint` does — that's
    /// the call-site invariant #142's tests need to pin.
    fn dump_resolved_cells(&mut self) -> roost_linux::ipc::ResolvedCellsData {
        if let Err(err) = self.render_state.update(&self.terminal) {
            tracing::warn!(?err, "render_state.update failed for tab.dump_resolved");
        }
        let default_fg = self.theme.foreground;
        let default_bg = self.theme.background;
        let bold_color = self.theme.bold_color;
        let mut cells: Vec<roost_linux::ipc::ResolvedCellData> = Vec::new();
        let _ = self.render_state.walk(&self.terminal, |row, cell: Cell| {
            let (fg, bg, has_explicit_bg) =
                resolve_cell_colors(&cell, default_fg, default_bg, bold_color);
            let text = if cell.text.is_empty() {
                " ".to_string()
            } else {
                cell.text.clone()
            };
            cells.push(roost_linux::ipc::ResolvedCellData {
                row,
                col: cell.col,
                text,
                fg: (fg.r, fg.g, fg.b),
                bg: (bg.r, bg.g, bg.b),
                has_explicit_bg,
                bold: cell.style.bold,
                italic: cell.style.italic,
                inverse: cell.style.inverse,
            });
        });
        roost_linux::ipc::ResolvedCellsData {
            cols: self.cols,
            rows: self.rows,
            cells,
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

        // Pass C.5 — clickable-link underline. Draw a single-pixel
        // rule across the bottom of the hovered URL's cells when the
        // link modifier is held + the pointer is over a URL. Color is
        // `theme.foreground` for v1 — a future `link-color` theme
        // key (Tier 2 punch-list) would route in here.
        if let Some(hov) = self.hover_url.as_ref() {
            if self.link_mod_held {
                let (r, g, b) = default_fg.to_f64();
                cr.set_source_rgb(r, g, b);
                let (x, y, w, h) = link_underline_rect(hov.col0, hov.col1, hov.row, cell_w, cell_h);
                cr.rectangle(x, y, w, h);
                let _ = cr.fill();
            }
        }

        // Pass D: selection overlay. Translucent fill so cell glyphs
        // and the cursor stay legible underneath. Same shape as the
        // Mac UI's `TerminalView.draw` selection draw.
        if let Some(sel) = self.selection {
            if sel.is_visible() {
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
    /// history). 3 modes:
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

    /// Drive the mouse encoder for a button or motion event at the
    /// given widget-pixel point. Returns the encoded bytes (may be
    /// empty when libghostty's encoder declines the report under
    /// the negotiated mode/format). For motion-no-button (mode 1003)
    /// the `MotionEmitter` throttle peeks first; we only `commit`
    /// after a successful encode so a declined event doesn't lock
    /// out the next emit.
    fn emit_mouse_tracking(
        &mut self,
        action: roost_linux::mouse_routing::MouseRoutingAction,
        button: Option<roost_linux::mouse_routing::MouseRoutingButton>,
        mods: roost_vt::Mods,
        x: f64,
        y: f64,
    ) -> Vec<u8> {
        let cw = self.cell_metrics.cell_width.max(1.0);
        let ch = self.cell_metrics.cell_height.max(1.0);
        let screen_w = (cw * self.cols as f64) as u32;
        let screen_h = (ch * self.rows as f64) as u32;
        // Clamp into the grid so an event just off the edge still
        // names the last cell. Mirrors `encode_wheel_buttons`.
        let cx = x.clamp(0.0, (cw * self.cols as f64 - 1.0).max(0.0));
        let cy = y.clamp(0.0, (ch * self.rows as f64 - 1.0).max(0.0));

        let is_motion_no_button = matches!(
            action,
            roost_linux::mouse_routing::MouseRoutingAction::Motion
        ) && button.is_none();
        let throttle_cell_col = (cx / cw) as u32;
        let throttle_cell_row = (cy / ch) as u32;
        let throttle_now = monotonic_seconds();
        if is_motion_no_button
            && !self
                .motion_emitter
                .would_emit(throttle_cell_col, throttle_cell_row, throttle_now)
        {
            return Vec::new();
        }

        self.mouse_encoder.sync_from_terminal(&self.terminal);
        self.mouse_encoder
            .set_size(screen_w, screen_h, cw as u32, ch as u32);

        let mut event = match MouseEvent::new() {
            Ok(ev) => ev,
            Err(_) => return Vec::new(),
        };
        event.set_action(routing_action_to_c(action));
        if let Some(b) = button {
            event.set_button(routing_button_to_c(b));
        } else {
            event.clear_button();
        }
        event.set_mods(mods);
        event.set_position(cx as f32, cy as f32);

        let bytes = match self.mouse_encoder.encode(&event) {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };
        if bytes.is_empty() {
            // Encoder declined — let the caller fall through and
            // don't commit the throttle so the next event retries.
            return Vec::new();
        }
        if is_motion_no_button {
            self.motion_emitter
                .commit(throttle_cell_col, throttle_cell_row, throttle_now);
        }
        bytes
    }

    /// Apply the OSC 22 W3C cursor name to the underlying widget.
    /// Gated on `pointer_inside` so a background tab's OSC drain
    /// doesn't change the visible cursor. URL hover (Ctrl over a
    /// URL) wins precedence over OSC 22 > GTK default text cursor
    /// (matches the Mac UI's iBeam fallback in `nsCursorForW3CName`).
    ///
    /// Skips the FFI `set_cursor_from_name` call when the same name
    /// would be pushed — under steady-state strix hover the same
    /// `pointer` lands on every motion event and re-pushing is
    /// wasted work.
    fn apply_current_cursor_shape(&mut self, widget: &DrawingArea) {
        if !self.pointer_inside {
            return;
        }
        let next: &'static str = if self.hover_url.is_some() && self.link_mod_held {
            "pointer"
        } else if !self.current_osc_shape.is_empty() {
            roost_linux::mouse_routing::gtk_cursor_name_for_w3c(&self.current_osc_shape)
        } else {
            "text"
        };
        if self.last_applied_cursor_name == Some(next) {
            return;
        }
        widget.set_cursor_from_name(Some(next));
        self.last_applied_cursor_name = Some(next);
    }

    /// True when the configured link modifier is held in `raw`. Lives
    /// here (not at the call sites) so the macOS Command-key quirk is
    /// handled once: GTK's macOS backend delivers Cmd as `META_MASK`
    /// (the keybind layer maps `super` → `<Meta>`), while X11/Wayland
    /// report a bound Super key as `SUPER_MASK` — for `super`/Cmd we
    /// accept either. See [`link_modifier_mask`].
    fn link_modifier_held(&self, raw: gtk4::gdk::ModifierType) -> bool {
        raw.intersects(link_modifier_mask(self.link_modifier))
    }

    /// Write the xterm focus-tracking sequence onto the PTY input
    /// channel when mode 1004 is enabled. CSI I for focus-gained,
    /// CSI O for focus-lost — both produced by libghostty-vt's
    /// `ghostty_focus_encode`. Returns the bytes to emit (empty
    /// when mode 1004 is off, so the caller can no-op without an
    /// extra check).
    fn encode_focus_bytes_if_active(&self, focused: bool) -> Vec<u8> {
        if !self.terminal.mode_get(1004) {
            return Vec::new();
        }
        roost_linux::mouse_routing::encode_focus_bytes(focused)
    }

    /// Resolve the URL (if any) covering `(col, row)`. OSC 8 wins
    /// over regex: if the cell carries an explicit hyperlink, the
    /// span is the contiguous run of cells sharing that URI; the
    /// regex pass is skipped. Otherwise we build the row's text by
    /// walking the render state and let `roost_url::find_url_at`
    /// answer. Mirrors the Mac UI's `computeHoverURL`.
    fn compute_hover_url(&mut self, col: u16, row: u16) -> Option<HoverUrl> {
        // OSC 8 first.
        if let Some(uri) = self.terminal.hyperlink_at(col, row as u32) {
            let (c0, c1) = self.osc8_span_at(col, row, &uri);
            return Some(HoverUrl {
                col0: c0,
                col1: c1,
                row,
                url: uri,
            });
        }
        let row_text = self.text_for_viewport_row(row);
        let span = roost_url::find_url_at(&row_text, col)?;
        Some(HoverUrl {
            col0: span.col0,
            col1: span.col1,
            row,
            url: span.url,
        })
    }

    /// Walk an OSC 8 span outward from `(col, row)`. libghostty only
    /// answers per-cell; the contiguous-span walk is the renderer's
    /// job so the underline + click target cover every cell that
    /// shares the URI. Stops at the row edge — line-wrap is a TODO.
    fn osc8_span_at(&self, col: u16, row: u16, uri: &str) -> (u16, u16) {
        let row_y = row as u32;
        osc8_span_walk(col, self.cols.saturating_sub(1), uri, |c| {
            self.terminal.hyperlink_at(c, row_y)
        })
    }

    /// Build the visible text of one viewport row by walking the
    /// render state. Each cell contributes exactly **one Unicode
    /// codepoint** so the click column (cell units) lines up with
    /// `row.chars().nth(col)` in `word_selection` / `roost_url` —
    /// codex flagged this on PR #176 after noticing that a row
    /// starting with `e\u{0301}` would otherwise shift `chars()`
    /// indices past cell columns. We emit each grapheme's first
    /// char and drop any trailing combining marks; the terminal
    /// cell is one display column regardless, so the lossy
    /// reduction only affects what the algorithms see (no glyph is
    /// painted from this string). Empty cells fall through as a
    /// single space.
    ///
    /// Same shape as `dump_text`, narrowed to one row.
    fn text_for_viewport_row(&mut self, target_row: u16) -> String {
        if let Err(err) = self.render_state.update(&self.terminal) {
            tracing::warn!(?err, "render_state.update failed for hover URL");
            return String::new();
        }
        let mut line = String::new();
        let _ = self.render_state.walk(&self.terminal, |row, cell: Cell| {
            if row != target_row as u32 {
                return;
            }
            match cell.text.chars().next() {
                Some(c) => line.push(c),
                None => line.push(' '),
            }
        });
        line
    }

    /// Swap the widget's cursor between `pointer` (URL hover with
    /// Update the cursor when URL hover state changes. Routes
    /// through `apply_current_cursor_shape` so the precedence ladder
    /// (URL > OSC 22 > default text) stays in one place. The earlier
    /// inline `set_cursor_from_name("text")` unconditionally on
    /// no-URL-hover silently clobbered any OSC 22 shape the TUI had
    /// asked for, breaking strix's `pointer` cursor every time the
    /// pointer moved off a URL.
    fn apply_link_cursor(&mut self, widget: &DrawingArea) {
        self.apply_current_cursor_shape(widget);
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
/// applying the SGR inverse + bold-accent rules below. Free function
/// so it's unit-testable without a Cairo context or DrawingArea.
///
/// Rules:
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
    if !sel.is_visible() {
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

/// Install a destination-only file/text drop handler on the terminal.
///
/// `clippy.toml` forbids `gtk4::DropTarget` because GtkDnD's drag-icon surface
/// aborts on Wayland (#236) — but that crash is on the drag *source* side (the
/// icon surface created when *initiating* a drag, which is why tab/project
/// reorder use `GtkGestureDrag`). A destination-only `DropTarget` that merely
/// *receives* external drops creates no drag-icon surface, so the #236 crash
/// doesn't apply here. The global lint stays in force for `DragSource` and
/// every other site; we opt out only on this receiver.
#[allow(clippy::disallowed_types)]
fn install_file_drop(widget: &DrawingArea, state: &Rc<RefCell<TerminalViewState>>) {
    let drop_target = gtk4::DropTarget::new(
        gtk4::gdk::FileList::static_type(),
        gtk4::gdk::DragAction::COPY,
    );
    // Order matters: GTK picks the first matching type the source offers. A
    // Nautilus/Files multi-select arrives as a FileList, a single file as a
    // gio::File, dragged text/links as a String. Mirrors Ghostty's GTK
    // surface drop-target types.
    drop_target.set_types(&[
        gtk4::gdk::FileList::static_type(),
        gtk4::gio::File::static_type(),
        glib::types::Type::STRING,
    ]);
    drop_target.connect_drop({
        let state = state.clone();
        move |_target, value, _x, _y| match drop_value_to_text(value) {
            Some(text) => {
                // The `drop` signal fires on the GTK main thread, so the PTY
                // write can go through `paste_text_into` directly (no idle hop,
                // unlike the async clipboard reads in `paste_from_clipboard`).
                paste_text_into(&state, text);
                true
            }
            None => false,
        }
    });
    widget.add_controller(drop_target);
}

/// Pull the inserted text out of a drop's `glib::Value` — a `gdk::FileList`
/// (multi-select), a single `gio::File`, or a `String` — and route it through
/// the pure `drop_text` resolver. Local filesystem paths only; a URI that
/// yields no local path is ignored.
fn drop_value_to_text(value: &glib::Value) -> Option<String> {
    if let Ok(list) = value.get::<gtk4::gdk::FileList>() {
        let paths: Vec<String> = list
            .files()
            .iter()
            .filter_map(|f| f.path())
            // Skip non-UTF-8 paths rather than lossily mangling them: a
            // U+FFFD-substituted path would silently name a different (or
            // nonexistent) file. A rejected path is a clean no-op. (macOS
            // enforces Unicode filenames, so the Mac side has no equivalent.)
            .filter_map(|p| p.to_str().map(String::from))
            .collect();
        return drop_text(&paths, None);
    }
    if let Ok(file) = value.get::<gtk4::gio::File>() {
        let paths: Vec<String> = file
            .path()
            .iter()
            .filter_map(|p| p.to_str().map(String::from))
            .collect();
        return drop_text(&paths, None);
    }
    if let Ok(s) = value.get::<String>() {
        return drop_text(&[], Some(&s));
    }
    None
}

/// Pure resolver from a drop payload to the text inserted into the PTY: file
/// paths take priority (shell-escaped, de-duplicated, newline-joined, with any
/// newline-bearing path dropped — a `\n` would split the join and, at a raw
/// shell, execute everything after it), else a plain string verbatim (it may be
/// a command the user wants to run). Returns `None` for an empty payload so the
/// caller emits no stray `ESC[200~ESC[201~`. Mirrors
/// `TerminalView.dropContentString` on Mac.
fn drop_text(file_paths: &[String], string: Option<&str>) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let escaped: Vec<String> = file_paths
        .iter()
        .filter(|p| !p.contains('\n') && !p.contains('\r'))
        .filter(|p| seen.insert(p.as_str()))
        .map(|p| shell_escape::escape(p))
        .collect();
    if !escaped.is_empty() {
        return Some(escaped.join("\n"));
    }
    match string {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

/// Decide which span (word vs line) a double/triple-click should
/// select for a row of text. Pure helper so the GTK n_press dispatch
/// stays testable without standing up a `DrawingArea` — same shape
/// as `osc8_span_walk`. Returns `None` for `click_count < 2` (caller
/// falls through to the single-cell drag path) and for `click_count
/// == 2` where the clicked cell itself is whitespace. `click_count`
/// values above 3 fall through to the triple-click line span, matching
/// what ghostty/iTerm2 do (and what the Mac port's `handleClickCount`
/// does).
fn click_count_span(
    row_text: &str,
    col: u16,
    click_count: u8,
    word_break_chars: &str,
) -> Option<roost_linux::word_selection::WordSpan> {
    match click_count {
        0 | 1 => None,
        2 => roost_linux::word_selection::expand_word(row_text, col, word_break_chars),
        _ => Some(roost_linux::word_selection::expand_line(row_text)),
    }
}

/// Walk an OSC 8 hyperlink span outward from `col` while the
/// per-cell URI lookup `hyperlink_at` keeps returning `Some(uri)`.
/// Pure function so the `osc8_span_at` method on `TerminalViewState`
/// can be unit-tested against a stubbed lookup without standing up
/// a full GTK widget. `max_col` is the rightmost valid column on
/// the row (inclusive) — typically `cols - 1`.
fn osc8_span_walk<F>(col: u16, max_col: u16, uri: &str, mut hyperlink_at: F) -> (u16, u16)
where
    F: FnMut(u16) -> Option<String>,
{
    let mut c0 = col;
    while c0 > 0 && hyperlink_at(c0 - 1).as_deref() == Some(uri) {
        c0 -= 1;
    }
    let mut c1 = col;
    while c1 < max_col && hyperlink_at(c1 + 1).as_deref() == Some(uri) {
        c1 += 1;
    }
    (c0, c1)
}

/// Map an [`AccelMods`] link modifier to the GDK mask(s) that count as
/// "held". `super`/Cmd maps to BOTH `SUPER_MASK` and `META_MASK`: GTK's
/// macOS backend reports the Command key as Meta (the keybind layer maps
/// `super` → `<Meta>`), while X11/Wayland report a bound Super key as
/// Super — accept either. Callers only ever pass a single-flag modifier
/// (`parse_link_modifier` / `default_link_modifier`). Free fn (not a
/// method) so it unit-tests without a `TerminalViewState`.
fn link_modifier_mask(m: AccelMods) -> gtk4::gdk::ModifierType {
    use gtk4::gdk::ModifierType as M;
    let mut set = M::empty();
    if m.contains(AccelMods::CTRL) {
        set |= M::CONTROL_MASK;
    }
    if m.contains(AccelMods::ALT) {
        set |= M::ALT_MASK;
    }
    if m.contains(AccelMods::SUPER) {
        set |= M::SUPER_MASK | M::META_MASK;
    }
    set
}

/// Pixel rectangle (x, y, w, h) for the underline overlay drawn on
/// a URL's cells. Extracted from the paint pass so tests can pin the
/// math without standing up a Cairo surface. `cell_w` / `cell_h` are
/// the live cell metrics; `col0` / `col1` are inclusive column
/// bounds (mirrors `roost_url::UrlSpan`); `row` is the viewport row.
/// Returns `(x, y, width, height)` in widget-pixel coordinates.
fn link_underline_rect(
    col0: u16,
    col1: u16,
    row: u16,
    cell_w: f64,
    cell_h: f64,
) -> (f64, f64, f64, f64) {
    let span_cells = (col1 - col0 + 1) as f64;
    (
        col0 as f64 * cell_w,
        (row as f64 + 1.0) * cell_h - 1.0,
        span_cells * cell_w,
        1.0,
    )
}

/// Shared right-button forward path used by both `right_click`
/// gesture-controller signals (pressed + released). Both arms
/// differ only in the `MouseRoutingAction` — extracted to dedup
/// ~26 lines per arm and to give a single place for the
/// press-consumed flag that handles mid-gesture mode toggles.
///
/// Mid-gesture handling:
/// * Press fires while `mouse_tracking()` is on → forward AND mark
///   `right_tracking_press_consumed_this_gesture = true`.
/// * Release fires: if the flag is set, ALWAYS forward (the
///   matching release belongs to the tracking gesture even if the
///   TUI disabled tracking between press and release — leaving a
///   press without a release would otherwise wedge the TUI's
///   button-state machine). If the flag is NOT set and tracking is
///   on now, ignore the release: it would be an orphan (the TUI
///   enabled tracking mid-gesture and the press wasn't reported).
fn forward_right_button(
    state: &Rc<RefCell<TerminalViewState>>,
    action: roost_linux::mouse_routing::MouseRoutingAction,
    raw_mods: gtk4::gdk::ModifierType,
    x: f64,
    y: f64,
) {
    let mut s = state.borrow_mut();
    match action {
        roost_linux::mouse_routing::MouseRoutingAction::Press => {
            if !s.terminal.mouse_tracking() {
                s.right_tracking_press_consumed_this_gesture = false;
                return;
            }
        }
        roost_linux::mouse_routing::MouseRoutingAction::Release => {
            if !s.right_tracking_press_consumed_this_gesture {
                // Press wasn't forwarded (tracking was off then, or
                // never enabled). Drop the release so the TUI's
                // event queue stays balanced.
                return;
            }
        }
        _ => {}
    }
    let mods = key_encoder::translate_mods(raw_mods);
    let bytes = s.emit_mouse_tracking(
        action,
        Some(roost_linux::mouse_routing::MouseRoutingButton::Right),
        mods,
        x,
        y,
    );
    if matches!(
        action,
        roost_linux::mouse_routing::MouseRoutingAction::Press
    ) {
        // Mark even on a declined encode so a release after a
        // tracking-on press never gets dropped as "orphan". Mirror
        // of the left-drag X10 fix.
        s.right_tracking_press_consumed_this_gesture = true;
    } else if matches!(
        action,
        roost_linux::mouse_routing::MouseRoutingAction::Release
    ) {
        // Clear after the release fires through so the next press
        // gesture starts fresh.
        s.right_tracking_press_consumed_this_gesture = false;
    }
    let cb = s.input_callback.clone();
    drop(s);
    if !bytes.is_empty() {
        if let Some(cb) = cb {
            cb(bytes);
        }
    }
}

/// Monotonic-clock seconds since process start. Sibling of
/// `CACurrentMediaTime` on the Mac side; feeds `MotionEmitter`'s
/// 60 Hz throttle.
fn monotonic_seconds() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}

fn routing_action_to_c(
    action: roost_linux::mouse_routing::MouseRoutingAction,
) -> roost_vt::MouseAction {
    match action {
        roost_linux::mouse_routing::MouseRoutingAction::Press => roost_vt::mouse_action::PRESS,
        roost_linux::mouse_routing::MouseRoutingAction::Release => roost_vt::mouse_action::RELEASE,
        roost_linux::mouse_routing::MouseRoutingAction::Motion => roost_vt::mouse_action::MOTION,
    }
}

fn routing_button_to_c(
    button: roost_linux::mouse_routing::MouseRoutingButton,
) -> roost_vt::MouseButton {
    match button {
        roost_linux::mouse_routing::MouseRoutingButton::Left => roost_vt::mouse_button::LEFT,
        roost_linux::mouse_routing::MouseRoutingButton::Right => roost_vt::mouse_button::RIGHT,
        roost_linux::mouse_routing::MouseRoutingButton::Middle => roost_vt::mouse_button::MIDDLE,
        roost_linux::mouse_routing::MouseRoutingButton::Four => roost_vt::mouse_button::FOUR,
        roost_linux::mouse_routing::MouseRoutingButton::Five => roost_vt::mouse_button::FIVE,
    }
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
    //! the canvas bg). All cases below exercise `resolve_cell_colors`.
    use super::*;
    use roost_vt::{Cell, ColorRgb, Style};

    #[test]
    fn link_modifier_mask_super_accepts_meta_and_super() {
        use gtk4::gdk::ModifierType as M;
        let mask = link_modifier_mask(AccelMods::SUPER);
        // GTK's macOS backend delivers Cmd as Meta; X11/Wayland Super
        // keys as Super. Both must register as "link modifier held".
        assert!(mask.intersects(M::META_MASK), "Cmd-as-Meta must match");
        assert!(mask.intersects(M::SUPER_MASK), "Super must match");
        // A plain Ctrl press must NOT trip the super link modifier.
        assert!(!M::CONTROL_MASK.intersects(mask));
    }

    #[test]
    fn link_modifier_mask_ctrl_and_alt_are_exact() {
        use gtk4::gdk::ModifierType as M;
        assert_eq!(link_modifier_mask(AccelMods::CTRL), M::CONTROL_MASK);
        assert_eq!(link_modifier_mask(AccelMods::ALT), M::ALT_MASK);
        // Ctrl link modifier must not be tripped by Cmd/Meta.
        assert!(!M::META_MASK.intersects(link_modifier_mask(AccelMods::CTRL)));
    }

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
            committed: false,
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
            committed: false,
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
            committed: false,
        };
        let (sc, sy, ec, ey) = sel.normalized();
        // Same rectangle as the previous test — direction-independent.
        assert_eq!((sc, sy, ec, ey), (5, 10, 9, 13));
    }

    #[test]
    fn selection_is_visible_single_cell_uncommitted_hides() {
        // Drag_begin at a single cell that the user never extended
        // — anchor == cursor, committed = false. Should NOT render
        // or copy.
        let sel = Selection {
            anchor_col: 5,
            anchor_screen_y: 10,
            cursor_col: 5,
            cursor_screen_y: 10,
            committed: false,
        };
        assert!(sel.is_empty());
        assert!(!sel.is_visible());
    }

    #[test]
    fn selection_is_visible_single_cell_committed_renders() {
        // Multi-click selected a single-letter word like `i` — the
        // n_press dispatch sets committed = true so the paint pass
        // and selection_text still emit the cell, even though it's
        // geometrically empty. Pins the Codex PR #177 regression.
        let sel = Selection {
            anchor_col: 5,
            anchor_screen_y: 10,
            cursor_col: 5,
            cursor_screen_y: 10,
            committed: true,
        };
        assert!(sel.is_empty());
        assert!(sel.is_visible());
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

    // ============================================================
    // Clickable-link helpers (PR C)
    // ============================================================
    //
    // Click-path coverage: the OSC 8 span walk + the underline
    // rectangle math are tested as pure functions; full click
    // gesture wiring (modifier tracking, GestureDrag interaction)
    // is exercised in CI's e2e-gtk pytest run against a live UI.
    use std::collections::HashMap;

    #[test]
    fn osc8_span_walk_finds_contiguous_run() {
        // Cells 5..9 all carry the same URI; everything else does not.
        let cells: HashMap<u16, &'static str> = HashMap::from_iter([
            (5, "https://x.test"),
            (6, "https://x.test"),
            (7, "https://x.test"),
            (8, "https://x.test"),
            (9, "https://x.test"),
        ]);
        let lookup = |c: u16| cells.get(&c).map(|s| s.to_string());

        // Probing from the middle of the span walks both ways.
        assert_eq!(osc8_span_walk(7, 79, "https://x.test", lookup), (5, 9));
        // Probing from the left edge walks only rightward.
        assert_eq!(osc8_span_walk(5, 79, "https://x.test", lookup), (5, 9));
        // Probing from the right edge walks only leftward.
        assert_eq!(osc8_span_walk(9, 79, "https://x.test", lookup), (5, 9));
    }

    #[test]
    fn osc8_span_walk_handles_single_cell_span() {
        // A 1-cell URI must report `(col, col)` — no over-walk past the
        // span into adjacent non-OSC-8 cells.
        let cells: HashMap<u16, &'static str> = HashMap::from_iter([(12, "https://only-one.test")]);
        let lookup = |c: u16| cells.get(&c).map(|s| s.to_string());

        assert_eq!(
            osc8_span_walk(12, 79, "https://only-one.test", lookup),
            (12, 12)
        );
    }

    #[test]
    fn osc8_span_walk_stops_at_right_edge() {
        // The span runs all the way to the row's last cell — the walk
        // must clamp at `max_col`, not over-run into the next row's
        // first cell.
        let cells: HashMap<u16, &'static str> =
            HashMap::from_iter([(77, "https://x"), (78, "https://x"), (79, "https://x")]);
        let lookup = |c: u16| cells.get(&c).map(|s| s.to_string());

        assert_eq!(osc8_span_walk(78, 79, "https://x", lookup), (77, 79));
    }

    #[test]
    fn osc8_span_walk_stops_at_uri_boundary() {
        // Adjacent OSC 8 cells with a DIFFERENT URI must not extend
        // the span — different hyperlinks live next to each other in
        // shell output like `ls --hyperlink`.
        let cells: HashMap<u16, &'static str> = HashMap::from_iter([
            (3, "https://a"),
            (4, "https://a"),
            (5, "https://b"),
            (6, "https://b"),
        ]);
        let lookup = |c: u16| cells.get(&c).map(|s| s.to_string());

        assert_eq!(osc8_span_walk(4, 79, "https://a", lookup), (3, 4));
        assert_eq!(osc8_span_walk(5, 79, "https://b", lookup), (5, 6));
    }

    #[test]
    fn link_underline_rect_single_cell() {
        // Single-cell URL underline on row 3, column 7 with 10x20 cells.
        let (x, y, w, h) = link_underline_rect(7, 7, 3, 10.0, 20.0);
        assert_eq!((x, y, w, h), (70.0, 79.0, 10.0, 1.0));
    }

    #[test]
    fn link_underline_rect_multi_cell_span() {
        // A 5-cell span (cols 4..8 inclusive) on row 0 at 8x16 cells:
        // x=32, y=15, w=40, h=1.
        let (x, y, w, h) = link_underline_rect(4, 8, 0, 8.0, 16.0);
        assert_eq!((x, y, w, h), (32.0, 15.0, 40.0, 1.0));
    }

    // ============================================================
    // n_press dispatch (PR #161 — word/line selection)
    // ============================================================
    //
    // The full click → expand_word/line → mutate selection → copy
    // pipeline is exercised in the e2e pytest run against a live
    // UI via `tab.expand_selection_at`. The unit-test layer below
    // covers the pure click-count branch in isolation, mirroring
    // PR #175's `osc8_span_walk` pure-helper coverage.

    use roost_linux::word_selection::{WordSpan, DEFAULT_EXTRA_WORD_CHARS};

    #[test]
    fn click_count_span_single_click_falls_through() {
        assert_eq!(
            click_count_span("hello world", 2, 1, DEFAULT_EXTRA_WORD_CHARS),
            None
        );
        // Zero click_count (defensive) also falls through.
        assert_eq!(
            click_count_span("hello world", 2, 0, DEFAULT_EXTRA_WORD_CHARS),
            None
        );
    }

    #[test]
    fn click_count_span_double_click_returns_word() {
        assert_eq!(
            click_count_span("hello world", 8, 2, DEFAULT_EXTRA_WORD_CHARS),
            Some(WordSpan { col0: 6, col1: 10 })
        );
    }

    #[test]
    fn click_count_span_double_click_whitespace_returns_none() {
        assert_eq!(
            click_count_span("hello world", 5, 2, DEFAULT_EXTRA_WORD_CHARS),
            None
        );
    }

    #[test]
    fn click_count_span_triple_click_returns_line() {
        assert_eq!(
            click_count_span("hello world", 0, 3, DEFAULT_EXTRA_WORD_CHARS),
            Some(WordSpan { col0: 0, col1: 10 })
        );
    }

    #[test]
    fn click_count_span_quadruple_click_degrades_to_line() {
        // Match ghostty + iTerm2: 4+ clicks degenerate to line, not
        // some larger selection unit Roost doesn't ship.
        assert_eq!(
            click_count_span("hello world", 0, 4, DEFAULT_EXTRA_WORD_CHARS),
            Some(WordSpan { col0: 0, col1: 10 })
        );
    }

    #[test]
    fn click_count_span_custom_break_chars_splits_path() {
        // Drop `/` and `.` from the extras — `/tmp/foo.txt` splits
        // into segments on double-click. Pins the config lever.
        assert_eq!(
            click_count_span("see /tmp/foo.txt today", 7, 2, "_-+~:@%"),
            Some(WordSpan { col0: 5, col1: 7 })
        );
    }

    // Drop-payload resolver. Mirrors `DropContentResolverTests` on Mac so the
    // two drag-and-drop implementations stay at parity.

    fn p(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn drop_text_single_file_is_escaped() {
        assert_eq!(
            drop_text(&[p("/tmp/My File.png")], None),
            Some("/tmp/My\\ File.png".to_string())
        );
    }

    #[test]
    fn drop_text_multiple_files_are_newline_joined() {
        assert_eq!(
            drop_text(&[p("/tmp/a b.png"), p("/tmp/c.png")], None),
            Some("/tmp/a\\ b.png\n/tmp/c.png".to_string())
        );
    }

    #[test]
    fn drop_text_duplicate_files_are_collapsed() {
        assert_eq!(
            drop_text(&[p("/tmp/shot.png"), p("/tmp/shot.png")], None),
            Some("/tmp/shot.png".to_string())
        );
    }

    #[test]
    fn drop_text_newline_path_is_dropped() {
        assert_eq!(drop_text(&[p("/tmp/ev\nil.png")], None), None);
        assert_eq!(
            drop_text(&[p("/tmp/ev\nil.png"), p("/tmp/ok.png")], None),
            Some("/tmp/ok.png".to_string())
        );
    }

    #[test]
    fn drop_text_string_is_not_escaped() {
        assert_eq!(
            drop_text(&[], Some("git status && ls")),
            Some("git status && ls".to_string())
        );
    }

    #[test]
    fn drop_text_multiline_string_is_preserved() {
        assert_eq!(
            drop_text(&[], Some("line one\nline two")),
            Some("line one\nline two".to_string())
        );
    }

    #[test]
    fn drop_text_files_take_priority_over_string() {
        assert_eq!(
            drop_text(&[p("/tmp/a.png")], Some("ignored")),
            Some("/tmp/a.png".to_string())
        );
    }

    #[test]
    fn drop_text_empty_payload_is_none() {
        assert_eq!(drop_text(&[], None), None);
        assert_eq!(drop_text(&[], Some("")), None);
    }
}
