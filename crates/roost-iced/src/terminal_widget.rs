//! Renderer-neutral Iced terminal widget.

use iced::advanced::text::{Paragraph as _, Renderer as _};
use iced::advanced::widget::{self, Widget};
use iced::advanced::{
    input_method, layout, renderer, text, Clipboard, InputMethod, Layout, Renderer as _, Shell,
};
#[cfg(test)]
use iced::event;
use iced::{
    alignment, mouse, Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle,
    Renderer, Size, Theme,
};
use roost_engine::pointer::{PointerAction, PointerButton};
use roost_ui_model::sprite::{sprite_geometry, tessellate, SpriteGeometry, SpritePrimitive};
use roost_vt::{ColorRgb, CursorInfo, CursorVisualStyle, SelectionSpan};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The grid is edge-pinned: it starts at the widget's own origin and keeps
/// every pixel the layout gives it (Mac parity — `app.window_metrics` puts
/// the terminal flush under the tab band). Kept as a named seam so the cell
/// math below stays symbolic.
pub const TERMINAL_PADDING: f32 = 0.0;
const POINT_TO_LOGICAL_PIXEL: f64 = 96.0 / 72.0;
const TERMINAL_LINE_HEIGHT: f32 = 1.2;
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// One renderer-resolved terminal cell grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalMetrics {
    pub font: Font,
    pub font_pixels: f32,
    pub cell_width: f32,
    pub cell_height: f32,
}

impl TerminalMetrics {
    /// Resolve the generic monospace grid from a Rust UI point size.
    pub fn measure(size_pt: f64) -> Result<Self, String> {
        Self::measure_with_font(size_pt, Font::MONOSPACE)
    }

    /// Resolve a supplied renderer family at a Rust UI point size.
    pub fn measure_with_font(size_pt: f64, font: Font) -> Result<Self, String> {
        let pixels = size_pt * POINT_TO_LOGICAL_PIXEL;
        if !pixels.is_finite() || pixels <= 0.0 || pixels > f64::from(f32::MAX) {
            return Err(format!(
                "font size {size_pt}pt cannot be represented as Iced logical pixels"
            ));
        }
        let font_pixels = pixels as f32;
        if !font_pixels.is_finite() || font_pixels <= 0.0 {
            return Err(format!(
                "font size {size_pt}pt narrowed to invalid Iced logical pixels"
            ));
        }

        type Paragraph = <Renderer as text::Renderer>::Paragraph;
        let paragraph = Paragraph::with_text(text::Text {
            content: "M",
            bounds: Size::INFINITE,
            size: Pixels(font_pixels),
            line_height: text::LineHeight::Relative(TERMINAL_LINE_HEIGHT),
            font,
            align_x: text::Alignment::Default,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Auto,
            wrapping: text::Wrapping::None,
        });
        let measured = paragraph.min_bounds();
        let cell_width = measured.width.floor();
        let cell_height = measured.height.floor();
        if !cell_width.is_finite()
            || !cell_height.is_finite()
            || cell_width < 1.0
            || cell_height < 1.0
        {
            return Err(format!(
                "font size {size_pt}pt measured an invalid Iced cell {}x{}",
                measured.width, measured.height
            ));
        }
        Ok(Self {
            font,
            font_pixels,
            cell_width,
            cell_height,
        })
    }

    #[cfg(test)]
    fn fixed(cell_width: f32, cell_height: f32) -> Self {
        Self {
            font: Font::MONOSPACE,
            font_pixels: 13.5,
            cell_width,
            cell_height,
        }
    }
}

/// Misc Technical (U+2300-23FF), Geometric Shapes (U+25A0-25FF), and Misc
/// Symbols (U+2600-26FF) codepoints whose Unicode default is TEXT
/// presentation (`Emoji_Presentation=No` in UTS #51's `emoji-data.txt`).
/// These render monochrome everywhere except cosmic-text's fallback
/// cascade, which resolves them to Apple Color Emoji on macOS (the ⏺ bug).
/// This is a curated range list, not an embedded UCD table: it excludes
/// the specific sub-ranges/codepoints within these three blocks that
/// emoji-data.txt marks `Emoji_Presentation=Yes` (so they keep the
/// emoji-cascade path, matching their Unicode default), and it does not
/// attempt to cover astral-plane or other BMP symbol blocks — narrowed to
/// what shows up in terminal output (spinners, media-control glyphs like
/// ⏺⏸⏵, status dots, misc technical icons).
fn is_default_text_presentation(c: char) -> bool {
    let cp = c as u32;
    match cp {
        0x2300..=0x23FF => !matches!(cp, 0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3),
        0x25A0..=0x25FF => !matches!(cp, 0x25FD..=0x25FE),
        0x2600..=0x26FF => !matches!(
            cp,
            0x2614..=0x2615
                | 0x2648..=0x2653
                | 0x267F
                | 0x2693
                | 0x26A1
                | 0x26AA..=0x26AB
                | 0x26BD..=0x26BE
                | 0x26C4..=0x26C5
                | 0x26CE
                | 0x26D4
                | 0x26EA
                | 0x26F2..=0x26F3
                | 0x26F5
                | 0x26FA
                | 0x26FD
        ),
        _ => false,
    }
}

/// A cluster wants the monochrome-capable fallback family when its base
/// codepoint defaults to text presentation and the cluster carries no
/// U+FE0F (VS16) — VS16 is an explicit request for the emoji glyph and
/// must keep today's cascade-to-color-emoji behavior.
fn wants_monochrome_fallback(text: &str) -> bool {
    const VS16: char = '\u{FE0F}';
    text.chars()
        .next()
        .is_some_and(is_default_text_presentation)
        && !text.contains(VS16)
}

/// Platform-specific monochrome-capable family for
/// [`wants_monochrome_fallback`] clusters, picked empirically (no
/// per-platform font-availability probe at runtime — cosmic-text's
/// cascade falls through gracefully if the named family is absent).
///
/// macOS: "Apple Symbols" was tried first (matches the mac app's own
/// cascade intuition) but does not actually cover U+23FA (`fc-list
/// ":charset=23fa" family` on a stock macOS install lists only `Apple
/// Color Emoji`, `STIX Two Math`, and `.LastResort`), so cosmic-text fell
/// through past it to color emoji anyway. "STIX Two Math" does cover it
/// and was confirmed monochrome by screenshot + pixel scan (no
/// blue-dominant pixels in the glyph's cell, vs. hundreds when routed
/// through the emoji cascade).
///
/// Linux: DejaVu Sans is the existing fallback family in this codebase's
/// terminal font list (`roost-ui-model::typography`) and has broad Misc
/// Technical/Symbols coverage; not independently re-verified here (no
/// fontconfig database for it on this dev Mac) — the Linux-box pass
/// (brief §3) is the place to confirm it visually.
fn monochrome_fallback_family() -> iced::font::Family {
    #[cfg(target_os = "macos")]
    {
        iced::font::Family::Name("STIX Two Math")
    }
    #[cfg(not(target_os = "macos"))]
    {
        iced::font::Family::Name("DejaVu Sans")
    }
}

fn draw_font(base: Font, text: &str, bold: bool, italic: bool) -> Font {
    Font {
        family: if UnicodeWidthStr::width(text) > 1 {
            // Cosmic Text's named and generic monospace families do not
            // discover every platform fallback (notably CJK on macOS). Each
            // render cell already owns its fixed terminal advance, so wide
            // clusters may use the system fallback chain across their
            // allotted cells without changing the grid. Ordinary and
            // combining cells must retain the configured renderer family.
            iced::font::Family::SansSerif
        } else if wants_monochrome_fallback(text) {
            monochrome_fallback_family()
        } else {
            base.family
        },
        weight: if bold {
            iced::font::Weight::Bold
        } else {
            iced::font::Weight::Normal
        },
        style: if italic {
            iced::font::Style::Italic
        } else {
            iced::font::Style::Normal
        },
        ..base
    }
}

/// The text primitive every terminal glyph draw shares: one cell-tall run
/// on the grid's own baseline, never wrapped or aligned — the cell's
/// position is the caller's, not the layout's.
fn cell_text(content: String, font: Font, metrics: TerminalMetrics) -> text::Text<String> {
    text::Text {
        content,
        bounds: Size::new(f32::INFINITY, metrics.cell_height),
        size: Pixels(metrics.font_pixels),
        line_height: text::LineHeight::Relative(TERMINAL_LINE_HEIGHT),
        font,
        align_x: text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: text::Shaping::Auto,
        wrapping: text::Wrapping::None,
    }
}

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
    pub fn build(cells: &[roost_vt::Cell], defaults: (ColorRgb, ColorRgb), cols: u16) -> Self {
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

/// One live platform-IME composition.
///
/// **Overlay invariant — a preedit is draw-time only.** Composition text
/// is never written into the terminal, so the VT never sees a character
/// the user has not committed and clearing a preedit restores the cells
/// by construction. `cursor` is the IME's caret as a byte range into
/// `text`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImePreedit {
    pub text: String,
    pub cursor: Option<std::ops::Range<usize>>,
}

impl ImePreedit {
    /// How many terminal cells precede the composition caret.
    fn caret_cells(&self) -> u16 {
        let head = self
            .cursor
            .as_ref()
            .map(|cursor| cursor.start)
            .filter(|start| self.text.is_char_boundary(*start))
            .unwrap_or(self.text.len());
        preedit_cells(&self.text[..head])
    }
}

/// Where a composition sits on the cursor's row.
///
/// Shared by the overlay and the rectangle the OS anchors its candidate
/// window on, so the two can never disagree — a composition that slid
/// left to stay on the row has to take its caret with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreeditPlacement {
    start_col: u16,
    caret_col: u16,
    total: u16,
}

fn preedit_placement(preedit: &ImePreedit, cursor_col: u16, cols: u16) -> PreeditPlacement {
    // No wrap: a composition that would run past the row's end slides
    // left so its tail — and the caret riding it — stays on screen.
    let total = preedit_cells(&preedit.text);
    let start_col = cursor_col.min(cols.saturating_sub(total));
    PreeditPlacement {
        start_col,
        caret_col: start_col
            .saturating_add(preedit.caret_cells())
            .min(cols.saturating_sub(1)),
        total,
    }
}

#[derive(Debug, Clone)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub foreground: ColorRgb,
    pub background: ColorRgb,
    pub cursor: Option<CursorInfo>,
    /// Theme cursor color — the fallback when `cursor.color` (an OSC-12
    /// set, cleared by OSC 112) is `None`. GTK/mac parity: both fall back
    /// to `theme.cursor`, not `foreground`.
    pub cursor_color: ColorRgb,
    /// Indexed by viewport row; `grid.len() == rows`.
    pub grid: Vec<Arc<RenderedRow>>,
    pub selection_background: ColorRgb,
    pub selection_spans: Vec<SelectionSpan>,
    pub link_hover: Option<SelectionSpan>,
    pub pointer_shape: String,
    pub preedit: Option<ImePreedit>,
}

impl TerminalSnapshot {
    pub fn blank(cols: u16, rows: u16) -> Self {
        // Every row shares one empty `RenderedRow`: rows are replaced
        // wholesale, never mutated in place. `RenderedRow::text` is `""`
        // here, which is what a `refresh_snapshot`-built blank row also
        // trims down to — `tab.dump` depends on the two agreeing.
        let blank_row = Arc::new(RenderedRow::default());
        Self {
            cols,
            rows,
            foreground: ColorRgb {
                r: 214,
                g: 218,
                b: 224,
            },
            background: ColorRgb {
                r: 18,
                g: 20,
                b: 24,
            },
            cursor: None,
            cursor_color: ColorRgb {
                r: 214,
                g: 218,
                b: 224,
            },
            grid: vec![blank_row; usize::from(rows)],
            selection_background: ColorRgb {
                r: 72,
                g: 83,
                b: 109,
            },
            selection_spans: Vec::new(),
            link_hover: None,
            pointer_shape: "default".into(),
            preedit: None,
        }
    }
}

/// The cursor color the renderer should paint: an OSC-12 override
/// (`cursor.color`) wins when present, otherwise the theme's cursor color
/// carried on the snapshot. GTK/mac parity (both fall back to
/// `theme.cursor`, not `foreground`). Pure so it's unit-testable — the
/// draw path that consumes it needs a live `Renderer` and can't be.
fn effective_cursor_color(cursor: &CursorInfo, snapshot: &TerminalSnapshot) -> ColorRgb {
    cursor.color.unwrap_or(snapshot.cursor_color)
}

#[derive(Debug, Clone)]
pub struct TerminalWidget {
    pub tab_id: i64,
    pub snapshot: TerminalSnapshot,
    pub metrics: TerminalMetrics,
    pub metric_generation: u64,
    /// Whether this terminal owns keyboard input right now — the app
    /// computes it as "the keyboard route is this tab and the window is
    /// focused". Only then does the widget ask the platform for an IME.
    pub ime_active: bool,
    /// Whether the terminal owns the keyboard: the window has focus AND
    /// the keyboard route is a terminal (not the palette, rename editor,
    /// or confirm overlay). Either being false draws the cursor hollow
    /// (mac parity, `TerminalView.cursorRenderMode` /
    /// `hasFocus = windowIsKey && viewIsFirstResponder`).
    pub focused: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TerminalPointer {
    Event(TerminalPointerEvent),
    Wheel(TerminalWheelEvent),
    Leave { tab_id: i64 },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalPointerEvent {
    pub tab_id: i64,
    pub action: PointerAction,
    pub button: Option<PointerButton>,
    pub col: u32,
    pub row: u32,
    pub click_count: u8,
    pub inside: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalWheelEvent {
    pub tab_id: i64,
    /// Positive moves toward older history; negative toward the live bottom.
    pub history_rows: f64,
    pub col: u32,
    pub row: u32,
}

fn wheel_history_rows(delta: mouse::ScrollDelta, cell_height: f32) -> f64 {
    match delta {
        // Match the existing Swift policy: one discrete notch moves three
        // terminal rows. Iced already gives the conventional positive-up sign.
        mouse::ScrollDelta::Lines { y, .. } => f64::from(y) * 3.0,
        // Smooth native deltas are physical/logical pixels. Convert them into
        // rows here; TerminalScroll owns cross-event fractional accumulation.
        mouse::ScrollDelta::Pixels { y, .. } => f64::from(y) / f64::from(cell_height),
    }
}

#[derive(Default)]
pub(crate) struct TerminalWidgetState {
    tab_id: Option<i64>,
    metric_generation: u64,
    pressed: Option<PointerButton>,
    last_cell: Option<(u32, u32)>,
    was_inside: bool,
    clicks: ClickTracker,
    last_mouse_interaction: Option<mouse::Interaction>,
}

#[derive(Debug, Clone, Copy)]
struct ClickSequence {
    tab_id: i64,
    cell: (u32, u32),
    at: Instant,
    count: u8,
}

#[derive(Default)]
struct ClickTracker {
    sequence: Option<ClickSequence>,
}

impl ClickTracker {
    fn primary_press(&mut self, tab_id: i64, cell: (u32, u32), now: Instant) -> u8 {
        let count = self
            .sequence
            .filter(|sequence| {
                sequence.tab_id == tab_id
                    && sequence.cell == cell
                    && now.saturating_duration_since(sequence.at) <= MULTI_CLICK_INTERVAL
            })
            .map_or(1, |sequence| sequence.count.saturating_add(1));
        self.sequence = Some(ClickSequence {
            tab_id,
            cell,
            at: now,
            count,
        });
        count
    }

    fn reset(&mut self) {
        self.sequence = None;
    }
}

#[derive(Debug)]
struct PointerOutcome {
    message: Option<crate::Message>,
    captured: bool,
}

impl PointerOutcome {
    fn publish(message: crate::Message) -> Self {
        Self {
            message: Some(message),
            captured: true,
        }
    }

    fn capture() -> Self {
        Self {
            message: None,
            captured: true,
        }
    }

    #[cfg(test)]
    fn into_inner(self) -> (Option<crate::Message>, (), event::Status) {
        (
            self.message,
            (),
            if self.captured {
                event::Status::Captured
            } else {
                event::Status::Ignored
            },
        )
    }
}

impl TerminalWidget {
    fn update_pointer(
        &self,
        state: &mut TerminalWidgetState,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<PointerOutcome> {
        if state.tab_id != Some(self.tab_id) || state.metric_generation != self.metric_generation {
            state.tab_id = Some(self.tab_id);
            state.metric_generation = self.metric_generation;
            state.pressed = None;
            state.last_cell = None;
            state.was_inside = false;
            state.clicks.reset();
        }
        if matches!(event, Event::Mouse(mouse::Event::CursorLeft)) {
            if state.was_inside || state.last_cell.is_some() {
                state.was_inside = false;
                if state.pressed.is_none() {
                    state.last_cell = None;
                }
                state.clicks.reset();
                return Some(PointerOutcome::publish(crate::Message::TerminalPointer(
                    TerminalPointer::Leave {
                        tab_id: self.tab_id,
                    },
                )));
            }
            return None;
        }
        // One native press owns the drag/release sequence. A chorded press
        // must not replace that owner: doing so can strand terminal mouse
        // tracking without the original button's release. Consume the
        // secondary button pair locally and keep motion attributed to the
        // initiating button.
        if state.pressed.is_some() && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)))
        {
            state.clicks.reset();
            return Some(PointerOutcome::capture());
        }
        if let Event::Mouse(mouse::Event::ButtonReleased(native_button)) = event {
            if let Some(owner) = state.pressed {
                if mouse_button(*native_button) != Some(owner) {
                    return Some(PointerOutcome::capture());
                }
            }
        }
        let captured_gesture = state.pressed.is_some()
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonReleased(_) | mouse::Event::CursorMoved { .. })
            );
        let point = cursor.position_from(Point::new(bounds.x, bounds.y));
        let cell = point
            .and_then(|point| cell_at(point, self.snapshot.cols, self.snapshot.rows, self.metrics));
        let inside = cursor.is_over(bounds) && cell.is_some();
        if !inside && !captured_gesture {
            if state.was_inside {
                state.was_inside = false;
                state.last_cell = None;
                state.clicks.reset();
                return Some(PointerOutcome::publish(crate::Message::TerminalPointer(
                    TerminalPointer::Leave {
                        tab_id: self.tab_id,
                    },
                )));
            }
            if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
                state.clicks.reset();
            }
            return None;
        }
        let cell = cell
            .or_else(|| {
                point.and_then(|point| {
                    cell_at_clamped(point, self.snapshot.cols, self.snapshot.rows, self.metrics)
                })
            })
            .or(state.last_cell)?;
        let (col, row) = cell;
        if !inside {
            state.clicks.reset();
        }
        state.was_inside = inside;
        state.last_cell = inside.then_some(cell).or(state.last_cell);
        let pointer = match event {
            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                let Some(button) = mouse_button(*button) else {
                    state.clicks.reset();
                    return None;
                };
                state.pressed = Some(button);
                let click_count = if button == PointerButton::Left {
                    state
                        .clicks
                        .primary_press(self.tab_id, cell, Instant::now())
                } else {
                    state.clicks.reset();
                    1
                };
                TerminalPointer::Event(TerminalPointerEvent {
                    tab_id: self.tab_id,
                    action: PointerAction::Press,
                    button: Some(button),
                    col,
                    row,
                    click_count,
                    inside,
                })
            }
            Event::Mouse(mouse::Event::ButtonReleased(button)) => {
                let button = mouse_button(*button)?;
                if state.pressed == Some(button) {
                    state.pressed = None;
                }
                if !inside {
                    state.last_cell = None;
                }
                TerminalPointer::Event(TerminalPointerEvent {
                    tab_id: self.tab_id,
                    action: PointerAction::Release,
                    button: Some(button),
                    col,
                    row,
                    click_count: 0,
                    inside,
                })
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                TerminalPointer::Event(TerminalPointerEvent {
                    tab_id: self.tab_id,
                    action: PointerAction::Motion,
                    button: state.pressed,
                    col,
                    row,
                    click_count: 0,
                    inside,
                })
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                state.clicks.reset();
                let history_rows = wheel_history_rows(*delta, self.metrics.cell_height);
                if history_rows == 0.0 {
                    return None;
                }
                TerminalPointer::Wheel(TerminalWheelEvent {
                    tab_id: self.tab_id,
                    history_rows,
                    col,
                    row,
                })
            }
            _ => return None,
        };
        Some(PointerOutcome::publish(crate::Message::TerminalPointer(
            pointer,
        )))
    }

    /// The IME strategy this widget re-issues on every `RedrawRequested`
    /// — iced honors `request_input_method` only during that event, so a
    /// request made anywhere else is dropped.
    ///
    /// `preedit` is always `None`: that is what makes iced_winit clear
    /// its own over-the-spot overlay and leaves the on-the-spot drawing
    /// to [`TerminalWidget::draw`].
    fn input_method(&self, bounds: Rectangle) -> InputMethod<&str> {
        if !self.ime_active {
            return InputMethod::Disabled;
        }
        InputMethod::Enabled {
            cursor: self.ime_cursor_rect(bounds),
            purpose: input_method::Purpose::Terminal,
            preedit: None,
        }
    }

    /// Where the OS should keep its candidate window clear of, in window
    /// coordinates. With a composition up this tracks the composition
    /// caret rather than the terminal's own cursor cell.
    fn ime_cursor_rect(&self, bounds: Rectangle) -> Rectangle {
        let size = Size::new(self.metrics.cell_width, self.metrics.cell_height);
        let Some(cursor) = self.snapshot.cursor.filter(|cursor| cursor.visible) else {
            return Rectangle::new(bounds.position(), size);
        };
        let col = self
            .snapshot
            .preedit
            .as_ref()
            .map_or(cursor.col as u16, |preedit| {
                preedit_placement(preedit, cursor.col as u16, self.snapshot.cols).caret_col
            });
        Rectangle::new(
            cell_position(bounds.position(), col, cursor.row, self.metrics),
            size,
        )
    }

    /// Overlay the live composition on the cursor row. Returns the glyph
    /// draws it contributed. Nothing here touches the grid — a preedit is
    /// pixels only (see [`ImePreedit`]).
    fn draw_preedit(&self, renderer: &mut Renderer, bounds: Rectangle, clip: Rectangle) -> u64 {
        // A cursor the terminal is not reporting — hidden by DECTCEM, or
        // scrolled out of the viewport — leaves nowhere to anchor the
        // composition. The IME stays enabled; only the overlay is
        // suppressed.
        let (Some(preedit), Some(cursor)) = (
            self.snapshot.preedit.as_ref(),
            self.snapshot.cursor.filter(|cursor| cursor.visible),
        ) else {
            return 0;
        };
        let clusters = preedit_clusters(&preedit.text);
        if clusters.is_empty() {
            return 0;
        }
        let metrics = self.metrics;
        let placement = preedit_placement(preedit, cursor.col as u16, self.snapshot.cols);
        let start_col = placement.start_col;
        let origin = cell_position(bounds.position(), start_col, cursor.row, metrics);
        let span = f32::from(placement.total.min(self.snapshot.cols)) * metrics.cell_width;
        fill_quad(
            renderer,
            Rectangle::new(origin, Size::new(span, metrics.cell_height)),
            color(self.snapshot.selection_background),
        );

        let mut fill_text_calls = 0;
        let mut col = start_col;
        for (cluster, width) in clusters {
            if col >= self.snapshot.cols {
                break;
            }
            let position = cell_position(bounds.position(), col, cursor.row, metrics);
            renderer.fill_text(
                cell_text(
                    cluster.to_string(),
                    draw_font(metrics.font, cluster, false, false),
                    metrics,
                ),
                Point::new(position.x, position.y + 1.0),
                color(self.snapshot.foreground),
                clip,
            );
            fill_text_calls += 1;
            col = col.saturating_add(width);
        }

        fill_quad(
            renderer,
            Rectangle::new(
                Point::new(origin.x, origin.y + metrics.cell_height - 2.0),
                Size::new(span, 2.0),
            ),
            color(self.snapshot.foreground),
        );
        fill_text_calls
    }

    /// The composition message this event owes the app, if any. An IME
    /// event reaching a terminal that does not own input belongs to
    /// whatever surface does, so it is left uncaptured.
    fn ime_message(&self, event: &Event) -> Option<crate::Message> {
        match event {
            Event::InputMethod(event) if self.ime_active => {
                Some(crate::Message::Ime(event.clone()))
            }
            _ => None,
        }
    }

    fn pointer_interaction_at(
        &self,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if !cursor.is_over(bounds) {
            return mouse::Interaction::default();
        }
        let cell = cursor
            .position_from(Point::new(bounds.x, bounds.y))
            .and_then(|point| cell_at(point, self.snapshot.cols, self.snapshot.rows, self.metrics));
        if cell.is_none() {
            return mouse::Interaction::default();
        }
        pointer_interaction(self.snapshot.pointer_shape.as_str())
    }
}

impl Widget<crate::Message, Theme, Renderer> for TerminalWidget {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<TerminalWidgetState>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(TerminalWidgetState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut widget::Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, crate::Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<TerminalWidgetState>();
        if let Some(outcome) = self.update_pointer(state, event, layout.bounds(), cursor) {
            if let Some(message) = outcome.message {
                shell.publish(message);
            }
            if outcome.captured {
                shell.capture_event();
            }
        }
        if let Some(message) = self.ime_message(event) {
            shell.publish(message);
            shell.capture_event();
        }

        let interaction = self.pointer_interaction_at(layout.bounds(), cursor);
        if matches!(
            event,
            Event::Window(iced::window::Event::RedrawRequested(_))
        ) {
            state.last_mouse_interaction = Some(interaction);
            shell.request_input_method(&self.input_method(layout.bounds()));
        } else if state
            .last_mouse_interaction
            .is_some_and(|previous| previous != interaction)
        {
            shell.request_redraw();
        }

        let _ = (renderer, viewport);
    }

    fn mouse_interaction(
        &self,
        _tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        self.pointer_interaction_at(layout.bounds(), cursor)
    }

    fn draw(
        &self,
        _tree: &widget::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let draw_started_at = Instant::now();
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            crate::perf::record_draw(draw_started_at.elapsed(), 0);
            return;
        };

        let mut fill_text_calls: u64 = 0;
        renderer.with_layer(clip, |renderer| {
            fill_quad(renderer, bounds, color(self.snapshot.background));
            let metrics = self.metrics;

            for (row_idx, row) in self.snapshot.grid.iter().enumerate() {
                // The row index comes from the grid position, never from
                // the cell — see `DrawCell`.
                let row_y = row_idx as u32;
                for cell in &row.cells {
                    let position = cell_position(bounds.position(), cell.col, row_y, metrics);
                    if cell.explicit_background {
                        fill_quad(
                            renderer,
                            // Preserve the Canvas path's overdraw policy. Adjacent
                            // antialiased quads can otherwise expose hairlines of
                            // the default background under tiny-skia.
                            Rectangle::new(
                                position,
                                Size::new(metrics.cell_width, metrics.cell_height),
                            ),
                            color(cell.background),
                        );
                    }
                    if !cell.text.is_empty() && cell.text != " " {
                        // Sprite-render single-codepoint cells whose codepoint
                        // falls in one of the geometric ranges (GTK does the
                        // same in `terminal_view::paint`). Multi-codepoint
                        // graphemes skip this path because the sprite layer is
                        // by-codepoint, not by-grapheme.
                        let mut chars = cell.text.chars();
                        if let (Some(c), None) = (chars.next(), chars.next()) {
                            if let Some(geometry) = sprite_geometry(
                                c as u32,
                                f64::from(metrics.cell_width),
                                f64::from(metrics.cell_height),
                            ) {
                                draw_sprite(renderer, position, &geometry, color(cell.foreground));
                                // A sprite *replaces* a glyph draw, so it counts
                                // as one — same semantics as GTK.
                                fill_text_calls += 1;
                                continue;
                            }
                        }
                        let font =
                            draw_font(metrics.font, cell.text.as_str(), cell.bold, cell.italic);
                        renderer.fill_text(
                            cell_text(cell.text.clone(), font, metrics),
                            Point::new(position.x, position.y + 1.0),
                            color(cell.foreground),
                            clip,
                        );
                        fill_text_calls += 1;
                    }
                }
            }

            for span in &self.snapshot.selection_spans {
                fill_quad(
                    renderer,
                    Rectangle::new(
                        cell_position(bounds.position(), span.col0, u32::from(span.row), metrics),
                        Size::new(
                            f32::from(span.col1.saturating_sub(span.col0)) * metrics.cell_width,
                            metrics.cell_height,
                        ),
                    ),
                    Color {
                        a: 0.35,
                        ..color(self.snapshot.selection_background)
                    },
                );
            }

            if let Some(span) = self.snapshot.link_hover {
                let point =
                    cell_position(bounds.position(), span.col0, u32::from(span.row), metrics);
                fill_quad(
                    renderer,
                    Rectangle::new(
                        Point::new(point.x, point.y + metrics.cell_height - 1.0),
                        Size::new(
                            f32::from(span.col1.saturating_sub(span.col0)) * metrics.cell_width,
                            1.0,
                        ),
                    ),
                    color(self.snapshot.foreground),
                );
            }

            if let Some(cursor) = self.snapshot.cursor.filter(|cursor| cursor.visible) {
                let point =
                    cell_position(bounds.position(), cursor.col as u16, cursor.row, metrics);
                let cursor_color = color(effective_cursor_color(&cursor, &self.snapshot));
                let visual_style = if self.focused {
                    cursor.visual_style
                } else {
                    CursorVisualStyle::BlockHollow
                };
                match visual_style {
                    CursorVisualStyle::Block => fill_quad(
                        renderer,
                        Rectangle::new(point, Size::new(metrics.cell_width, metrics.cell_height)),
                        Color {
                            a: 0.55,
                            ..cursor_color
                        },
                    ),
                    CursorVisualStyle::BlockHollow => renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle::new(
                                point,
                                Size::new(metrics.cell_width, metrics.cell_height),
                            ),
                            border: Border {
                                color: cursor_color,
                                width: 1.0,
                                ..Border::default()
                            },
                            snap: false,
                            ..renderer::Quad::default()
                        },
                        Color::TRANSPARENT,
                    ),
                    CursorVisualStyle::Bar => fill_quad(
                        renderer,
                        Rectangle::new(point, Size::new(1.5, metrics.cell_height)),
                        cursor_color,
                    ),
                    CursorVisualStyle::Underline => fill_quad(
                        renderer,
                        Rectangle::new(
                            Point::new(point.x, point.y + metrics.cell_height - 2.0),
                            Size::new(metrics.cell_width, 2.0),
                        ),
                        cursor_color,
                    ),
                }
            }

            fill_text_calls += self.draw_preedit(renderer, bounds, clip);
        });
        crate::perf::record_draw(draw_started_at.elapsed(), fill_text_calls);
    }
}

impl From<TerminalWidget> for Element<'_, crate::Message> {
    fn from(widget: TerminalWidget) -> Self {
        Self::new(widget)
    }
}

/// Draw one cell's sprite geometry as quads at `origin`.
///
/// iced has no per-quad antialiasing switch (GTK gets `Antialias::None`
/// for the block layer, hence `SpriteGeometry::antialias` being ignored
/// here). A fractional shared edge between two quads composites to a
/// hairline — partial coverage twice is not full coverage — so the seam
/// story on this side is **integer edge snapping**: each rect's absolute
/// edges are rounded before the `Rectangle` is built. Cell strides are
/// integers (floored metrics), so adjacent cells round their shared edge
/// to the same integer and tiling is exact regardless of a fractional
/// widget origin. The f64→f32 cast happens after rounding; integer values
/// at terminal magnitudes are exactly representable in f32.
fn draw_sprite(
    renderer: &mut Renderer,
    origin: Point,
    geometry: &SpriteGeometry,
    foreground: Color,
) {
    for prim in &geometry.primitives {
        let alpha = match *prim {
            SpritePrimitive::Rect { alpha, .. } => alpha,
            SpritePrimitive::CornerArc { .. } | SpritePrimitive::Diagonal { .. } => 1.0,
        };
        let fill = Color {
            a: alpha as f32,
            ..foreground
        };
        for rect in tessellate(prim) {
            let left = (f64::from(origin.x) + rect.x).round();
            let top = (f64::from(origin.y) + rect.y).round();
            let right = (f64::from(origin.x) + rect.x + rect.w).round();
            let bottom = (f64::from(origin.y) + rect.y + rect.h).round();
            let (w, h) = (right - left, bottom - top);
            if w <= 0.0 || h <= 0.0 {
                continue;
            }
            fill_quad(
                renderer,
                Rectangle::new(
                    Point::new(left as f32, top as f32),
                    Size::new(w as f32, h as f32),
                ),
                fill,
            );
        }
    }
}

fn fill_quad(renderer: &mut Renderer, bounds: Rectangle, background: Color) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            snap: false,
            ..renderer::Quad::default()
        },
        background,
    );
}

/// How many terminal cells a composition occupies, counted exactly the
/// way the overlay lays it out. The grid's own widths come from
/// libghostty, which is not consulted for overlay text that never enters
/// the terminal — `unicode-width` is the same table it uses to decide
/// that a CJK cluster takes two cells, and is already this crate's source
/// for the wide-cell font policy in `draw_font`.
fn preedit_cells(text: &str) -> u16 {
    preedit_clusters(text)
        .iter()
        .fold(0, |total, (_, width)| total.saturating_add(*width))
}

/// Split composition text into (cluster, cell width) pairs. Zero-width
/// codepoints — combining marks, variation selectors — attach to the
/// cluster they modify instead of claiming a cell of their own.
fn preedit_clusters(text: &str) -> Vec<(&str, u16)> {
    let mut clusters: Vec<(&str, u16)> = Vec::new();
    let mut start = 0;
    for (index, character) in text.char_indices() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        let end = index + character.len_utf8();
        match clusters.last_mut() {
            Some(last) if width == 0 => last.0 = &text[start..end],
            _ => {
                start = index;
                clusters.push((&text[index..end], width.max(1)));
            }
        }
    }
    clusters
}

fn cell_position(origin: Point, col: u16, row: u32, metrics: TerminalMetrics) -> Point {
    Point::new(
        origin.x + TERMINAL_PADDING + f32::from(col) * metrics.cell_width,
        origin.y + TERMINAL_PADDING + row as f32 * metrics.cell_height,
    )
}

fn cell_at(point: Point, cols: u16, rows: u16, metrics: TerminalMetrics) -> Option<(u32, u32)> {
    if point.x < TERMINAL_PADDING || point.y < TERMINAL_PADDING {
        return None;
    }
    let col = ((point.x - TERMINAL_PADDING) / metrics.cell_width).floor() as u32;
    let row = ((point.y - TERMINAL_PADDING) / metrics.cell_height).floor() as u32;
    (col < u32::from(cols) && row < u32::from(rows)).then_some((col, row))
}

fn cell_at_clamped(
    point: Point,
    cols: u16,
    rows: u16,
    metrics: TerminalMetrics,
) -> Option<(u32, u32)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let col = ((point.x - TERMINAL_PADDING) / metrics.cell_width)
        .floor()
        .clamp(0.0, f32::from(cols.saturating_sub(1))) as u32;
    let row = ((point.y - TERMINAL_PADDING) / metrics.cell_height)
        .floor()
        .clamp(0.0, f32::from(rows.saturating_sub(1))) as u32;
    Some((col, row))
}

fn mouse_button(button: mouse::Button) -> Option<PointerButton> {
    match button {
        mouse::Button::Left => Some(PointerButton::Left),
        mouse::Button::Right => Some(PointerButton::Right),
        mouse::Button::Middle => Some(PointerButton::Middle),
        _ => None,
    }
}

fn pointer_interaction(shape: &str) -> mouse::Interaction {
    match shape {
        "pointer" => mouse::Interaction::Pointer,
        "crosshair" => mouse::Interaction::Crosshair,
        "grab" => mouse::Interaction::Grab,
        "grabbing" => mouse::Interaction::Grabbing,
        "not-allowed" => mouse::Interaction::NotAllowed,
        "col-resize" | "e-resize" | "w-resize" => mouse::Interaction::ResizingHorizontally,
        "row-resize" | "n-resize" | "s-resize" => mouse::Interaction::ResizingVertically,
        "ne-resize" | "sw-resize" => mouse::Interaction::ResizingDiagonallyUp,
        "nw-resize" | "se-resize" => mouse::Interaction::ResizingDiagonallyDown,
        "wait" => mouse::Interaction::Wait,
        "progress" => mouse::Interaction::Progress,
        "help" => mouse::Interaction::Help,
        "move" => mouse::Interaction::Move,
        "text" | "default" => mouse::Interaction::Text,
        _ => mouse::Interaction::Text,
    }
}

pub fn color(value: ColorRgb) -> Color {
    Color::from_rgb8(value.r, value.g, value.b)
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

    const CELL_WIDTH: f32 = 8.4;
    const CELL_HEIGHT: f32 = 18.0;

    fn metrics() -> TerminalMetrics {
        TerminalMetrics::fixed(CELL_WIDTH, CELL_HEIGHT)
    }

    fn widget(tab_id: i64, snapshot: TerminalSnapshot) -> TerminalWidget {
        TerminalWidget {
            tab_id,
            snapshot,
            metrics: metrics(),
            metric_generation: 1,
            ime_active: false,
            focused: true,
        }
    }

    fn cursor_info(color: Option<ColorRgb>) -> CursorInfo {
        CursorInfo {
            col: 0,
            row: 0,
            wide_tail: false,
            visible: true,
            blinking: false,
            visual_style: CursorVisualStyle::Block,
            color,
        }
    }

    #[test]
    fn effective_cursor_color_falls_back_to_the_snapshot_theme_color_without_an_override() {
        let mut snapshot = TerminalSnapshot::blank(80, 24);
        snapshot.cursor_color = ColorRgb {
            r: 255,
            g: 204,
            b: 102,
        };
        let cursor = cursor_info(None);
        assert_eq!(
            effective_cursor_color(&cursor, &snapshot),
            snapshot.cursor_color
        );
        assert_ne!(
            effective_cursor_color(&cursor, &snapshot),
            snapshot.foreground
        );
    }

    #[test]
    fn effective_cursor_color_prefers_an_osc12_override_over_the_theme_color() {
        let mut snapshot = TerminalSnapshot::blank(80, 24);
        snapshot.cursor_color = ColorRgb {
            r: 255,
            g: 204,
            b: 102,
        };
        let override_color = ColorRgb {
            r: 10,
            g: 20,
            b: 30,
        };
        let cursor = cursor_info(Some(override_color));
        assert_eq!(effective_cursor_color(&cursor, &snapshot), override_color);
    }

    #[test]
    fn measured_metrics_are_positive_and_scale_with_point_size() {
        let default = TerminalMetrics::measure(13.0).expect("default metrics");
        let larger = TerminalMetrics::measure(18.0).expect("larger metrics");
        assert!(default.cell_width >= 1.0);
        assert!(default.cell_height >= 1.0);
        assert_eq!(default.cell_width.fract(), 0.0);
        assert_eq!(default.cell_height.fract(), 0.0);
        assert!(larger.font_pixels > default.font_pixels);
        assert!(larger.cell_width >= default.cell_width);
        assert!(larger.cell_height > default.cell_height);
    }

    #[test]
    fn supplied_font_family_reaches_metrics_and_style_variants() {
        let selected = Font::with_name("Roost Test Family");
        let metrics = TerminalMetrics::measure_with_font(13.0, selected)
            .expect("named family metrics fall back safely when unavailable");
        assert_eq!(metrics.font, selected);
        let normal = draw_font(metrics.font, "M", false, false);
        let bold = draw_font(metrics.font, "M", true, false);
        let italic = draw_font(metrics.font, "e\u{301}", false, true);
        assert_eq!(normal.family, selected.family);
        assert_eq!(bold.family, selected.family);
        assert_eq!(italic.family, selected.family);
        assert_eq!(bold.weight, iced::font::Weight::Bold);
        assert_eq!(italic.style, iced::font::Style::Italic);
        assert_eq!(
            draw_font(metrics.font, "界", false, false).family,
            iced::font::Family::SansSerif,
            "wide cells retain the intentional platform fallback policy"
        );
        assert_eq!(
            draw_font(metrics.font, "你", false, false).family,
            iced::font::Family::SansSerif,
            "wide cells retain the intentional platform fallback policy"
        );
        assert_eq!(
            draw_font(metrics.font, "a", false, false).family,
            selected.family,
            "ordinary ascii keeps the configured renderer family"
        );
    }

    #[test]
    fn default_text_presentation_clusters_route_to_the_monochrome_family() {
        let selected = Font::with_name("Roost Test Family");
        assert_eq!(
            draw_font(selected, "⏺", false, false).family,
            monochrome_fallback_family(),
            "U+23FA has no VS16 and defaults to text presentation \
             (the W16 bug repro — Apple Color Emoji picks it up from any \
             base family in cosmic-text's cascade)"
        );
    }

    #[test]
    fn vs16_clusters_keep_the_emoji_cascade_path() {
        // unicode-width already treats any VS16 (U+FE0F) cluster as width
        // 2, so this hits draw_font's pre-existing wide-cluster branch
        // (SansSerif) before ever reaching wants_monochrome_fallback --
        // the assertion that matters is that it is NOT the monochrome
        // family, i.e. VS16's explicit request for the emoji glyph is
        // honored either way.
        let selected = Font::with_name("Roost Test Family");
        let family = draw_font(selected, "⏺\u{FE0F}", false, false).family;
        assert_ne!(
            family,
            monochrome_fallback_family(),
            "VS16 is an explicit request for the emoji glyph; it must not \
             be routed to the monochrome-capable family"
        );
        assert_eq!(family, iced::font::Family::SansSerif);
    }

    #[test]
    fn default_emoji_presentation_codepoints_are_unaffected() {
        // Same reasoning as the VS16 case: unicode-width already scores
        // Emoji_Presentation=Yes BMP codepoints like U+231A as width 2, so
        // draw_font's wide-cluster branch (SansSerif) claims it first.
        // is_default_text_presentation's own exclusion of these codepoints
        // (tested directly below) is what would matter if that ever
        // changed.
        let selected = Font::with_name("Roost Test Family");
        let family = draw_font(selected, "\u{231A}", false, false).family;
        assert_ne!(
            family,
            monochrome_fallback_family(),
            "U+231A (watch) defaults to EMOJI presentation per \
             emoji-data.txt; it must keep cascading to color emoji, not \
             get routed to the monochrome family"
        );
        assert_eq!(family, iced::font::Family::SansSerif);
    }

    #[test]
    fn is_default_text_presentation_curated_ranges() {
        // Bare media-control glyphs terminal apps actually emit.
        assert!(is_default_text_presentation('⏺')); // U+23FA record button
        assert!(is_default_text_presentation('⏸')); // U+23F8 pause bar
        assert!(is_default_text_presentation('⏵')); // U+23F5 play triangle

        // Emoji_Presentation=Yes exclusions within the curated blocks stay
        // on the emoji-cascade path.
        assert!(!is_default_text_presentation('\u{231A}')); // watch
        assert!(!is_default_text_presentation('\u{26A1}')); // high voltage
        assert!(!is_default_text_presentation('\u{26AA}')); // white circle

        // Outside the curated blocks entirely.
        assert!(!is_default_text_presentation('a'));
        assert!(!is_default_text_presentation('你'));
        assert!(!is_default_text_presentation('\u{1F600}')); // 😀, astral
    }

    #[test]
    fn wants_monochrome_fallback_requires_no_vs16() {
        assert!(wants_monochrome_fallback("⏺"));
        assert!(!wants_monochrome_fallback("⏺\u{FE0F}"));
        assert!(!wants_monochrome_fallback("a"));
        assert!(!wants_monochrome_fallback(""));
    }

    #[test]
    fn invalid_or_unrenderable_point_sizes_are_rejected() {
        for size in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX] {
            assert!(TerminalMetrics::measure(size).is_err(), "accepted {size}");
        }
    }

    #[test]
    fn inverse_swaps_resolved_defaults() {
        let fg = ColorRgb { r: 1, g: 2, b: 3 };
        let bg = ColorRgb { r: 4, g: 5, b: 6 };
        assert_eq!(resolve_colors(None, None, (fg, bg), true), (bg, fg));
    }

    #[test]
    fn widget_coordinates_map_to_terminal_cells_from_a_nonzero_origin() {
        let origin = Point::new(220.0, 44.0);
        assert_eq!(
            cell_position(origin, 5, 3, metrics()),
            Point::new(
                origin.x + TERMINAL_PADDING + 5.0 * CELL_WIDTH,
                origin.y + TERMINAL_PADDING + 3.0 * CELL_HEIGHT,
            )
        );
        assert_eq!(
            cell_at(
                Point::new(
                    TERMINAL_PADDING + 5.5 * CELL_WIDTH,
                    TERMINAL_PADDING + 3.5 * CELL_HEIGHT,
                ),
                80,
                24,
                metrics(),
            ),
            Some((5, 3))
        );
        // Edge-pinned grid: the widget's own origin is cell (0, 0) — there is
        // no inset gutter to reject — so only points before the origin or past
        // the last cell fall outside.
        assert_eq!(
            cell_at(
                Point::new(TERMINAL_PADDING, TERMINAL_PADDING),
                80,
                24,
                metrics()
            ),
            Some((0, 0))
        );
        assert_eq!(
            cell_at(
                Point::new(TERMINAL_PADDING - 1.0, TERMINAL_PADDING),
                80,
                24,
                metrics()
            ),
            None
        );
        assert_eq!(
            cell_at(
                Point::new(TERMINAL_PADDING + 80.0 * CELL_WIDTH, TERMINAL_PADDING),
                80,
                24,
                metrics()
            ),
            None
        );
        assert_eq!(
            cell_at_clamped(Point::new(-50.0, 9_000.0), 80, 24, metrics()),
            Some((0, 23))
        );
    }

    #[test]
    fn non_pointer_events_are_ignored_and_a_new_tab_resets_gesture_state() {
        let widget = widget(22, TerminalSnapshot::blank(80, 24));
        let mut state = TerminalWidgetState {
            tab_id: Some(21),
            metric_generation: 1,
            pressed: Some(PointerButton::Left),
            last_cell: Some((7, 4)),
            was_inside: true,
            clicks: ClickTracker {
                sequence: Some(ClickSequence {
                    tab_id: 21,
                    cell: (7, 4),
                    at: Instant::now(),
                    count: 2,
                }),
            },
            last_mouse_interaction: Some(mouse::Interaction::Pointer),
        };
        let bounds = Rectangle::new(Point::new(220.0, 44.0), Size::new(800.0, 600.0));

        let outcome = widget.update_pointer(
            &mut state,
            &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                iced::keyboard::Modifiers::SHIFT,
            )),
            bounds,
            mouse::Cursor::Unavailable,
        );

        assert!(outcome.is_none(), "keyboard input must remain uncaptured");
        assert_eq!(state.tab_id, Some(22));
        assert_eq!(state.pressed, None);
        assert_eq!(state.last_cell, None);
        assert!(!state.was_inside);
        assert!(state.clicks.sequence.is_none());
    }

    #[test]
    fn metric_generation_change_resets_a_captured_pointer_gesture() {
        let mut widget = widget(22, TerminalSnapshot::blank(80, 24));
        widget.metric_generation = 8;
        let mut state = TerminalWidgetState {
            tab_id: Some(22),
            metric_generation: 7,
            pressed: Some(PointerButton::Left),
            last_cell: Some((7, 4)),
            was_inside: true,
            ..TerminalWidgetState::default()
        };
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));

        assert!(widget
            .update_pointer(
                &mut state,
                &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                    iced::keyboard::Modifiers::SHIFT,
                )),
                bounds,
                mouse::Cursor::Unavailable,
            )
            .is_none());
        assert_eq!(state.metric_generation, 8);
        assert_eq!(state.pressed, None);
        assert_eq!(state.last_cell, None);
        assert!(!state.was_inside);
    }

    #[test]
    fn native_press_state_is_carried_into_drag_motion() {
        let program = widget(42, TerminalSnapshot::blank(80, 24));
        let mut state = TerminalWidgetState::default();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let cursor = mouse::Cursor::Available(Point::new(
            TERMINAL_PADDING + 5.5 * CELL_WIDTH,
            TERMINAL_PADDING + 3.5 * CELL_HEIGHT,
        ));
        let press = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                bounds,
                cursor,
            )
            .expect("press action")
            .into_inner()
            .0
            .expect("press message");
        let crate::Message::TerminalPointer(TerminalPointer::Event(TerminalPointerEvent {
            tab_id,
            action,
            button,
            col,
            row,
            click_count,
            inside,
        })) = press
        else {
            panic!("unexpected press message")
        };
        assert_eq!(action, PointerAction::Press);
        assert_eq!(tab_id, 42);
        assert_eq!(button, Some(PointerButton::Left));
        assert_eq!((col, row, click_count, inside), (5, 3, 1, true));

        let motion = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(1.0, 1.0),
                }),
                bounds,
                cursor,
            )
            .expect("motion action")
            .into_inner()
            .0
            .expect("motion message");
        let crate::Message::TerminalPointer(TerminalPointer::Event(TerminalPointerEvent {
            action,
            button,
            inside,
            ..
        })) = motion
        else {
            panic!("unexpected motion message")
        };
        assert_eq!(action, PointerAction::Motion);
        assert_eq!(button, Some(PointerButton::Left));
        assert!(inside);

        let outside = mouse::Cursor::Available(Point::new(-20.0, 900.0));
        let outside_motion = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(-20.0, 900.0),
                }),
                bounds,
                outside,
            )
            .expect("outside motion action")
            .into_inner()
            .0
            .expect("outside motion message");
        assert!(matches!(
            outside_motion,
            crate::Message::TerminalPointer(TerminalPointer::Event(TerminalPointerEvent {
                action: PointerAction::Motion,
                col: 0,
                row: 23,
                inside: false,
                ..
            }))
        ));
        let release = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                outside,
            )
            .expect("release action")
            .into_inner()
            .0
            .expect("release message");
        let crate::Message::TerminalPointer(TerminalPointer::Event(TerminalPointerEvent {
            action,
            col,
            row,
            inside,
            ..
        })) = release
        else {
            panic!("unexpected release message")
        };
        assert_eq!(action, PointerAction::Release);
        assert_eq!((col, row, inside), (0, 23, false));
        assert_eq!(state.pressed, None);
    }

    #[test]
    fn chorded_button_cannot_steal_the_captured_gesture() {
        let program = widget(42, TerminalSnapshot::blank(80, 24));
        let mut state = TerminalWidgetState::default();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let cursor = mouse::Cursor::Available(Point::new(
            TERMINAL_PADDING + 5.5 * CELL_WIDTH,
            TERMINAL_PADDING + 3.5 * CELL_HEIGHT,
        ));
        let left_press = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                bounds,
                cursor,
            )
            .expect("left press action")
            .into_inner();
        assert!(left_press.0.is_some());
        assert_eq!(state.pressed, Some(PointerButton::Left));

        let right_press = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
                bounds,
                cursor,
            )
            .expect("chorded press is captured")
            .into_inner();
        assert!(right_press.0.is_none());
        assert_eq!(right_press.2, event::Status::Captured);
        assert_eq!(state.pressed, Some(PointerButton::Left));

        let right_release = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                bounds,
                cursor,
            )
            .expect("chorded release is captured")
            .into_inner();
        assert!(right_release.0.is_none());
        assert_eq!(state.pressed, Some(PointerButton::Left));

        let motion = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: cursor.position().expect("available cursor"),
                }),
                bounds,
                cursor,
            )
            .expect("captured motion")
            .into_inner()
            .0;
        assert!(matches!(
            motion,
            Some(crate::Message::TerminalPointer(TerminalPointer::Event(
                TerminalPointerEvent {
                    action: PointerAction::Motion,
                    button: Some(PointerButton::Left),
                    ..
                }
            )))
        ));

        let left_release = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                cursor,
            )
            .expect("owner release")
            .into_inner()
            .0;
        assert!(matches!(
            left_release,
            Some(crate::Message::TerminalPointer(TerminalPointer::Event(
                TerminalPointerEvent {
                    action: PointerAction::Release,
                    button: Some(PointerButton::Left),
                    ..
                }
            )))
        ));
        assert_eq!(state.pressed, None);
    }

    #[test]
    fn click_tracker_counts_matching_sequence_and_resets() {
        let mut tracker = ClickTracker::default();
        let start = Instant::now();
        assert_eq!(tracker.primary_press(1, (4, 2), start), 1);
        assert_eq!(
            tracker.primary_press(1, (4, 2), start + Duration::from_millis(100)),
            2
        );
        assert_eq!(
            tracker.primary_press(1, (4, 2), start + Duration::from_millis(200)),
            3
        );
        assert_eq!(
            tracker.primary_press(1, (5, 2), start + Duration::from_millis(250)),
            1
        );
        assert_eq!(
            tracker.primary_press(2, (5, 2), start + Duration::from_millis(300)),
            1
        );
        assert_eq!(
            tracker.primary_press(2, (5, 2), start + Duration::from_millis(900)),
            1
        );
        tracker.reset();
        assert_eq!(
            tracker.primary_press(2, (5, 2), start + Duration::from_millis(950)),
            1
        );
        tracker.sequence = Some(ClickSequence {
            tab_id: 2,
            cell: (5, 2),
            at: start + Duration::from_millis(960),
            count: u8::MAX,
        });
        assert_eq!(
            tracker.primary_press(2, (5, 2), start + Duration::from_millis(970)),
            u8::MAX
        );
    }

    #[test]
    fn passive_move_out_publishes_one_leave() {
        let program = widget(9, TerminalSnapshot::blank(80, 24));
        let mut state = TerminalWidgetState {
            tab_id: Some(9),
            metric_generation: 1,
            was_inside: true,
            last_cell: Some((3, 2)),
            ..TerminalWidgetState::default()
        };
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let outside = mouse::Cursor::Available(Point::new(-10.0, 40.0));
        let action = program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(-10.0, 40.0),
                }),
                bounds,
                outside,
            )
            .expect("leave action")
            .into_inner()
            .0;
        assert!(matches!(
            action,
            Some(crate::Message::TerminalPointer(TerminalPointer::Leave {
                tab_id: 9
            }))
        ));
        assert!(program
            .update_pointer(
                &mut state,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(-20.0, 50.0),
                }),
                bounds,
                outside,
            )
            .is_none());
    }

    #[test]
    fn widget_padding_is_leave_for_hover_and_clamped_for_capture() {
        let program = widget(10, TerminalSnapshot::blank(80, 24));
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let padding = mouse::Cursor::Available(Point::new(790.0, 590.0));
        let mut passive = TerminalWidgetState {
            tab_id: Some(10),
            metric_generation: 1,
            was_inside: true,
            last_cell: Some((5, 3)),
            ..TerminalWidgetState::default()
        };
        let leave = program
            .update_pointer(
                &mut passive,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(790.0, 590.0),
                }),
                bounds,
                padding,
            )
            .expect("padding leave")
            .into_inner()
            .0;
        assert!(matches!(
            leave,
            Some(crate::Message::TerminalPointer(TerminalPointer::Leave {
                tab_id: 10
            }))
        ));
        assert_eq!(passive.last_cell, None);

        let mut captured = TerminalWidgetState {
            tab_id: Some(10),
            metric_generation: 1,
            pressed: Some(PointerButton::Left),
            was_inside: true,
            last_cell: Some((5, 3)),
            ..TerminalWidgetState::default()
        };
        let motion = program
            .update_pointer(
                &mut captured,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(790.0, 590.0),
                }),
                bounds,
                padding,
            )
            .expect("captured padding motion")
            .into_inner()
            .0;
        assert!(matches!(
            motion,
            Some(crate::Message::TerminalPointer(TerminalPointer::Event(
                TerminalPointerEvent {
                    action: PointerAction::Motion,
                    col: 79,
                    row: 23,
                    inside: false,
                    ..
                }
            )))
        ));
    }

    #[test]
    fn wheel_resets_the_native_click_sequence() {
        let program = widget(11, TerminalSnapshot::blank(80, 24));
        let start = Instant::now();
        let mut state = TerminalWidgetState {
            tab_id: Some(11),
            metric_generation: 1,
            clicks: ClickTracker {
                sequence: Some(ClickSequence {
                    tab_id: 11,
                    cell: (4, 2),
                    at: start,
                    count: 1,
                }),
            },
            ..TerminalWidgetState::default()
        };
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let cursor = mouse::Cursor::Available(Point::new(
            TERMINAL_PADDING + 4.5 * CELL_WIDTH,
            TERMINAL_PADDING + 2.5 * CELL_HEIGHT,
        ));
        let _ = program.update_pointer(
            &mut state,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            bounds,
            cursor,
        );
        assert!(state.clicks.sequence.is_none());
    }

    #[test]
    fn wheel_units_normalize_to_positive_history_rows() {
        assert_eq!(
            wheel_history_rows(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 }, CELL_HEIGHT,),
            3.0
        );
        assert_eq!(
            wheel_history_rows(mouse::ScrollDelta::Lines { x: 0.0, y: -2.0 }, CELL_HEIGHT,),
            -6.0
        );
        assert_eq!(
            wheel_history_rows(
                mouse::ScrollDelta::Pixels {
                    x: 0.0,
                    y: CELL_HEIGHT / 2.0,
                },
                CELL_HEIGHT
            ),
            0.5
        );
    }

    fn composing_snapshot(text: &str, cursor: Option<std::ops::Range<usize>>) -> TerminalSnapshot {
        composing_snapshot_at(4, text, cursor)
    }

    fn composing_snapshot_at(
        col: u32,
        text: &str,
        cursor: Option<std::ops::Range<usize>>,
    ) -> TerminalSnapshot {
        let mut snapshot = TerminalSnapshot::blank(80, 24);
        snapshot.cursor = Some(CursorInfo {
            col,
            row: 2,
            wide_tail: false,
            visible: true,
            blinking: false,
            visual_style: CursorVisualStyle::Block,
            color: None,
        });
        snapshot.preedit = Some(ImePreedit {
            text: text.to_string(),
            cursor,
        });
        snapshot
    }

    #[test]
    fn preedit_clusters_measure_wide_and_combining_codepoints_in_cells() {
        assert_eq!(preedit_clusters("ni"), vec![("n", 1), ("i", 1)]);
        assert_eq!(preedit_clusters("你好"), vec![("你", 2), ("好", 2)]);
        assert_eq!(preedit_clusters("e\u{0301}"), vec![("e\u{0301}", 1)]);
        assert_eq!(preedit_cells("你好"), 4);
        assert_eq!(preedit_cells("e\u{0301}"), 1);
        assert!(preedit_clusters("").is_empty());
    }

    /// A composition never wraps: once it would run past the row it slides
    /// left so the tail — and the caret the IME anchors on — stays visible.
    /// The caret has to slide with it, or the OS parks its candidate
    /// window past the end of the row.
    #[test]
    fn an_overflowing_preedit_slides_itself_and_its_caret_onto_the_row() {
        let caret_at_end = |text: &str| ImePreedit {
            text: text.into(),
            cursor: None,
        };
        assert_eq!(
            preedit_placement(&caret_at_end("abc"), 4, 80),
            PreeditPlacement {
                start_col: 4,
                caret_col: 7,
                total: 3
            }
        );
        assert_eq!(
            preedit_placement(&caret_at_end("你好好"), 78, 80),
            PreeditPlacement {
                start_col: 74,
                caret_col: 79,
                total: 6
            }
        );
        // Longer than the whole row: everything that fits still lands on it.
        let long = "x".repeat(200);
        let placement = preedit_placement(&caret_at_end(&long), 4, 80);
        assert_eq!(placement.start_col, 0);
        assert_eq!(placement.caret_col, 79);
    }

    #[test]
    fn the_ime_caret_tracks_the_composition_cursor_in_cells() {
        let preedit = ImePreedit {
            text: "你好".into(),
            cursor: Some(3..3),
        };
        assert_eq!(preedit.caret_cells(), 2);
        assert_eq!(
            ImePreedit {
                text: "你好".into(),
                cursor: None,
            }
            .caret_cells(),
            4
        );
        // A byte range the IME reports off a char boundary must not panic.
        assert_eq!(
            ImePreedit {
                text: "你好".into(),
                cursor: Some(1..1),
            }
            .caret_cells(),
            4
        );
    }

    #[test]
    fn only_a_terminal_that_owns_input_asks_for_an_input_method() {
        let bounds = Rectangle::new(Point::new(220.0, 44.0), Size::new(800.0, 600.0));
        let mut program = widget(3, composing_snapshot("你", Some(0..0)));
        assert_eq!(program.input_method(bounds), InputMethod::Disabled);
        assert!(program
            .ime_message(&Event::InputMethod(input_method::Event::Commit(
                "你".into()
            )))
            .is_none());

        program.ime_active = true;
        let InputMethod::Enabled {
            cursor,
            purpose,
            preedit,
        } = program.input_method(bounds)
        else {
            panic!("an active terminal must enable the input method")
        };
        assert_eq!(purpose, input_method::Purpose::Terminal);
        // `None` is what makes iced_winit clear its own over-the-spot
        // overlay and leave the drawing to us.
        assert!(preedit.is_none());
        assert_eq!(
            cursor,
            Rectangle::new(
                cell_position(bounds.position(), 4, 2, metrics()),
                Size::new(CELL_WIDTH, CELL_HEIGHT)
            )
        );

        let mut carried = widget(3, composing_snapshot("你好", Some(3..3)));
        carried.ime_active = true;
        let InputMethod::Enabled { cursor, .. } = carried.input_method(bounds) else {
            panic!("enabled")
        };
        assert_eq!(
            cursor.x,
            cell_position(bounds.position(), 4, 2, metrics()).x + 2.0 * CELL_WIDTH,
            "the candidate window follows the composition caret"
        );

        let mut slid = widget(3, composing_snapshot_at(78, "你好好", None));
        slid.ime_active = true;
        let InputMethod::Enabled { cursor, .. } = slid.input_method(bounds) else {
            panic!("enabled")
        };
        assert_eq!(
            cursor.x,
            cell_position(bounds.position(), 79, 2, metrics()).x,
            "a slid composition drags the candidate window onto the row with it"
        );

        // Both a cursor the terminal never reported and one DECTCEM hid
        // leave nothing to anchor on.
        let mut invisible = composing_snapshot("你", Some(0..0));
        invisible.cursor = invisible.cursor.map(|cursor| CursorInfo {
            visible: false,
            ..cursor
        });
        for snapshot in [TerminalSnapshot::blank(80, 24), invisible] {
            let mut hidden = widget(3, snapshot);
            hidden.ime_active = true;
            let InputMethod::Enabled { cursor, .. } = hidden.input_method(bounds) else {
                panic!("enabled")
            };
            assert_eq!(
                cursor.position(),
                bounds.position(),
                "an unanchorable cursor falls back to the widget origin"
            );
        }
    }

    #[test]
    fn an_active_terminal_claims_every_input_method_event() {
        let mut program = widget(3, composing_snapshot("n", Some(1..1)));
        program.ime_active = true;
        for event in [
            input_method::Event::Opened,
            input_method::Event::Preedit("ni".into(), Some(2..2)),
            input_method::Event::Commit("你".into()),
            input_method::Event::Closed,
        ] {
            let Some(crate::Message::Ime(carried)) =
                program.ime_message(&Event::InputMethod(event.clone()))
            else {
                panic!("an active terminal must claim {event:?}")
            };
            assert_eq!(carried, event);
        }
        assert!(program
            .ime_message(&Event::Mouse(mouse::Event::CursorLeft))
            .is_none());
    }

    #[test]
    fn link_interaction_is_confined_to_widget_bounds() {
        let mut snapshot = TerminalSnapshot::blank(80, 24);
        snapshot.link_hover = Some(SelectionSpan {
            row: 0,
            col0: 0,
            col1: 4,
        });
        snapshot.pointer_shape = "pointer".into();
        let program = widget(1, snapshot);
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        assert_eq!(
            program
                .pointer_interaction_at(bounds, mouse::Cursor::Available(Point::new(20.0, 20.0)),),
            mouse::Interaction::Pointer
        );
        assert_eq!(
            program.pointer_interaction_at(
                bounds,
                mouse::Cursor::Available(Point::new(790.0, 590.0)),
            ),
            mouse::Interaction::default()
        );
        assert_eq!(
            program
                .pointer_interaction_at(bounds, mouse::Cursor::Available(Point::new(900.0, 20.0)),),
            mouse::Interaction::default()
        );
        assert_eq!(
            pointer_interaction("crosshair"),
            mouse::Interaction::Crosshair
        );
        assert_eq!(pointer_interaction("unknown"), mouse::Interaction::Text);
    }
}
