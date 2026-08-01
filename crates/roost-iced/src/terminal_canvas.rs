use iced::widget::canvas;
use iced::{mouse, Color, Font, Point, Rectangle, Renderer, Size, Theme};
use roost_engine::pointer::{PointerAction, PointerButton};
use roost_vt::{ColorRgb, CursorInfo, CursorVisualStyle, SelectionSpan};
use std::time::{Duration, Instant};

pub const CELL_WIDTH: f32 = 8.4;
pub const CELL_HEIGHT: f32 = 18.0;
pub const TERMINAL_PADDING: f32 = 12.0;
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct DrawCell {
    pub row: u32,
    pub col: u16,
    pub text: String,
    pub foreground: ColorRgb,
    pub background: ColorRgb,
    pub explicit_background: bool,
    pub bold: bool,
    pub italic: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub foreground: ColorRgb,
    pub background: ColorRgb,
    pub cursor: Option<CursorInfo>,
    pub cells: Vec<DrawCell>,
    pub rows_text: Vec<String>,
    pub selection_background: ColorRgb,
    pub selection_spans: Vec<SelectionSpan>,
    pub link_hover: Option<SelectionSpan>,
    pub pointer_shape: String,
}

impl TerminalSnapshot {
    pub fn blank(cols: u16, rows: u16) -> Self {
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
            cells: Vec::new(),
            rows_text: vec![String::new(); usize::from(rows)],
            selection_background: ColorRgb {
                r: 72,
                g: 83,
                b: 109,
            },
            selection_spans: Vec::new(),
            link_hover: None,
            pointer_shape: "default".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TerminalCanvas {
    pub tab_id: i64,
    pub snapshot: TerminalSnapshot,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TerminalPointer {
    Event(TerminalPointerEvent),
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

#[derive(Default)]
pub(crate) struct TerminalCanvasState {
    tab_id: Option<i64>,
    pressed: Option<PointerButton>,
    last_cell: Option<(u32, u32)>,
    was_inside: bool,
    clicks: ClickTracker,
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

impl canvas::Program<crate::Message> for TerminalCanvas {
    type State = TerminalCanvasState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<crate::Message>> {
        if state.tab_id != Some(self.tab_id) {
            state.tab_id = Some(self.tab_id);
            state.pressed = None;
            state.last_cell = None;
            state.was_inside = false;
            state.clicks.reset();
        }
        if matches!(event, canvas::Event::Mouse(mouse::Event::CursorLeft)) {
            if state.was_inside || state.last_cell.is_some() {
                state.was_inside = false;
                if state.pressed.is_none() {
                    state.last_cell = None;
                }
                state.clicks.reset();
                return Some(
                    canvas::Action::publish(crate::Message::TerminalPointer(
                        TerminalPointer::Leave {
                            tab_id: self.tab_id,
                        },
                    ))
                    .and_capture(),
                );
            }
            return None;
        }
        // One native press owns the drag/release sequence. A chorded press
        // must not replace that owner: doing so can strand terminal mouse
        // tracking without the original button's release. Consume the
        // secondary button pair locally and keep motion attributed to the
        // initiating button.
        if state.pressed.is_some()
            && matches!(event, canvas::Event::Mouse(mouse::Event::ButtonPressed(_)))
        {
            state.clicks.reset();
            return Some(canvas::Action::capture());
        }
        if let canvas::Event::Mouse(mouse::Event::ButtonReleased(native_button)) = event {
            if let Some(owner) = state.pressed {
                if mouse_button(*native_button) != Some(owner) {
                    return Some(canvas::Action::capture());
                }
            }
        }
        let captured_gesture = state.pressed.is_some()
            && matches!(
                event,
                canvas::Event::Mouse(
                    mouse::Event::ButtonReleased(_) | mouse::Event::CursorMoved { .. }
                )
            );
        let point = cursor.position_from(Point::new(bounds.x, bounds.y));
        let cell = point.and_then(|point| cell_at(point, self.snapshot.cols, self.snapshot.rows));
        let inside = cursor.is_over(bounds) && cell.is_some();
        if !inside && !captured_gesture {
            if state.was_inside {
                state.was_inside = false;
                state.last_cell = None;
                state.clicks.reset();
                return Some(
                    canvas::Action::publish(crate::Message::TerminalPointer(
                        TerminalPointer::Leave {
                            tab_id: self.tab_id,
                        },
                    ))
                    .and_capture(),
                );
            }
            if matches!(event, canvas::Event::Mouse(mouse::Event::ButtonPressed(_))) {
                state.clicks.reset();
            }
            return None;
        }
        let cell = cell
            .or_else(|| {
                point.and_then(|point| {
                    cell_at_clamped(point, self.snapshot.cols, self.snapshot.rows)
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
            canvas::Event::Mouse(mouse::Event::ButtonPressed(button)) => {
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
            canvas::Event::Mouse(mouse::Event::ButtonReleased(button)) => {
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
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
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
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                state.clicks.reset();
                let vertical = match delta {
                    mouse::ScrollDelta::Lines { y, .. } | mouse::ScrollDelta::Pixels { y, .. } => {
                        *y
                    }
                };
                if vertical == 0.0 {
                    return None;
                }
                TerminalPointer::Event(TerminalPointerEvent {
                    tab_id: self.tab_id,
                    action: PointerAction::Press,
                    button: Some(if vertical > 0.0 {
                        PointerButton::Four
                    } else {
                        PointerButton::Five
                    }),
                    col,
                    row,
                    click_count: 1,
                    inside,
                })
            }
            _ => return None,
        };
        Some(canvas::Action::publish(crate::Message::TerminalPointer(pointer)).and_capture())
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if !cursor.is_over(bounds) {
            return mouse::Interaction::default();
        }
        let cell = cursor
            .position_from(Point::new(bounds.x, bounds.y))
            .and_then(|point| cell_at(point, self.snapshot.cols, self.snapshot.rows));
        if cell.is_none() {
            return mouse::Interaction::default();
        }
        pointer_interaction(self.snapshot.pointer_shape.as_str())
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        frame.fill_rectangle(
            Point::ORIGIN,
            bounds.size(),
            color(self.snapshot.background),
        );

        for cell in &self.snapshot.cells {
            let position = cell_position(cell.col, cell.row);
            if cell.explicit_background {
                frame.fill_rectangle(
                    position,
                    Size::new(CELL_WIDTH.ceil(), CELL_HEIGHT.ceil()),
                    color(cell.background),
                );
            }
            if !cell.text.is_empty() && cell.text != " " {
                let font = Font {
                    weight: if cell.bold {
                        iced::font::Weight::Bold
                    } else {
                        iced::font::Weight::Normal
                    },
                    style: if cell.italic {
                        iced::font::Style::Italic
                    } else {
                        iced::font::Style::Normal
                    },
                    ..Font::MONOSPACE
                };
                frame.fill_text(canvas::Text {
                    content: cell.text.clone(),
                    position: Point::new(position.x, position.y + 1.0),
                    color: color(cell.foreground),
                    size: iced::Pixels(13.5),
                    font,
                    ..canvas::Text::default()
                });
            }
        }

        for span in &self.snapshot.selection_spans {
            frame.fill_rectangle(
                cell_position(span.col0, u32::from(span.row)),
                Size::new(
                    f32::from(span.col1.saturating_sub(span.col0)) * CELL_WIDTH,
                    CELL_HEIGHT,
                ),
                Color {
                    a: 0.35,
                    ..color(self.snapshot.selection_background)
                },
            );
        }

        if let Some(span) = self.snapshot.link_hover {
            let point = cell_position(span.col0, u32::from(span.row));
            frame.fill_rectangle(
                Point::new(point.x, point.y + CELL_HEIGHT - 1.0),
                Size::new(
                    f32::from(span.col1.saturating_sub(span.col0)) * CELL_WIDTH,
                    1.0,
                ),
                color(self.snapshot.foreground),
            );
        }

        if let Some(cursor) = self.snapshot.cursor.filter(|cursor| cursor.visible) {
            let point = cell_position(cursor.col as u16, cursor.row);
            let cursor_color = color(cursor.color.unwrap_or(self.snapshot.foreground));
            match cursor.visual_style {
                CursorVisualStyle::Block => frame.fill_rectangle(
                    point,
                    Size::new(CELL_WIDTH, CELL_HEIGHT),
                    Color {
                        a: 0.55,
                        ..cursor_color
                    },
                ),
                CursorVisualStyle::BlockHollow => frame.stroke_rectangle(
                    point,
                    Size::new(CELL_WIDTH, CELL_HEIGHT),
                    canvas::Stroke::default()
                        .with_color(cursor_color)
                        .with_width(1.0),
                ),
                CursorVisualStyle::Bar => {
                    frame.fill_rectangle(point, Size::new(1.5, CELL_HEIGHT), cursor_color)
                }
                CursorVisualStyle::Underline => frame.fill_rectangle(
                    Point::new(point.x, point.y + CELL_HEIGHT - 2.0),
                    Size::new(CELL_WIDTH, 2.0),
                    cursor_color,
                ),
            }
        }

        vec![frame.into_geometry()]
    }
}

fn cell_position(col: u16, row: u32) -> Point {
    Point::new(
        TERMINAL_PADDING + f32::from(col) * CELL_WIDTH,
        TERMINAL_PADDING + row as f32 * CELL_HEIGHT,
    )
}

fn cell_at(point: Point, cols: u16, rows: u16) -> Option<(u32, u32)> {
    if point.x < TERMINAL_PADDING || point.y < TERMINAL_PADDING {
        return None;
    }
    let col = ((point.x - TERMINAL_PADDING) / CELL_WIDTH).floor() as u32;
    let row = ((point.y - TERMINAL_PADDING) / CELL_HEIGHT).floor() as u32;
    (col < u32::from(cols) && row < u32::from(rows)).then_some((col, row))
}

fn cell_at_clamped(point: Point, cols: u16, rows: u16) -> Option<(u32, u32)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let col = ((point.x - TERMINAL_PADDING) / CELL_WIDTH)
        .floor()
        .clamp(0.0, f32::from(cols.saturating_sub(1))) as u32;
    let row = ((point.y - TERMINAL_PADDING) / CELL_HEIGHT)
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

    #[test]
    fn inverse_swaps_resolved_defaults() {
        let fg = ColorRgb { r: 1, g: 2, b: 3 };
        let bg = ColorRgb { r: 4, g: 5, b: 6 };
        assert_eq!(resolve_colors(None, None, (fg, bg), true), (bg, fg));
    }

    #[test]
    fn canvas_coordinates_map_to_terminal_cells() {
        assert_eq!(
            cell_at(
                Point::new(
                    TERMINAL_PADDING + 5.5 * CELL_WIDTH,
                    TERMINAL_PADDING + 3.5 * CELL_HEIGHT,
                ),
                80,
                24,
            ),
            Some((5, 3))
        );
        assert_eq!(cell_at(Point::new(1.0, 1.0), 80, 24), None);
        assert_eq!(
            cell_at_clamped(Point::new(-50.0, 9_000.0), 80, 24),
            Some((0, 23))
        );
    }

    #[test]
    fn native_press_state_is_carried_into_drag_motion() {
        let program = TerminalCanvas {
            tab_id: 42,
            snapshot: TerminalSnapshot::blank(80, 24),
        };
        let mut state = TerminalCanvasState::default();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let cursor = mouse::Cursor::Available(Point::new(
            TERMINAL_PADDING + 5.5 * CELL_WIDTH,
            TERMINAL_PADDING + 3.5 * CELL_HEIGHT,
        ));
        let press = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
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

        let motion = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::CursorMoved {
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
        let outside_motion = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::CursorMoved {
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
        let release = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
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
        let program = TerminalCanvas {
            tab_id: 42,
            snapshot: TerminalSnapshot::blank(80, 24),
        };
        let mut state = TerminalCanvasState::default();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let cursor = mouse::Cursor::Available(Point::new(
            TERMINAL_PADDING + 5.5 * CELL_WIDTH,
            TERMINAL_PADDING + 3.5 * CELL_HEIGHT,
        ));
        let left_press = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            bounds,
            cursor,
        )
        .expect("left press action")
        .into_inner();
        assert!(left_press.0.is_some());
        assert_eq!(state.pressed, Some(PointerButton::Left));

        let right_press = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
            bounds,
            cursor,
        )
        .expect("chorded press is captured")
        .into_inner();
        assert!(right_press.0.is_none());
        assert_eq!(state.pressed, Some(PointerButton::Left));

        let right_release = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
            bounds,
            cursor,
        )
        .expect("chorded release is captured")
        .into_inner();
        assert!(right_release.0.is_none());
        assert_eq!(state.pressed, Some(PointerButton::Left));

        let motion = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::CursorMoved {
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

        let left_release = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
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
        let program = TerminalCanvas {
            tab_id: 9,
            snapshot: TerminalSnapshot::blank(80, 24),
        };
        let mut state = TerminalCanvasState {
            tab_id: Some(9),
            was_inside: true,
            last_cell: Some((3, 2)),
            ..TerminalCanvasState::default()
        };
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let outside = mouse::Cursor::Available(Point::new(-10.0, 40.0));
        let action = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::CursorMoved {
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
        assert!(<TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(-20.0, 50.0),
            }),
            bounds,
            outside,
        )
        .is_none());
    }

    #[test]
    fn canvas_padding_is_leave_for_hover_and_clamped_for_capture() {
        let program = TerminalCanvas {
            tab_id: 10,
            snapshot: TerminalSnapshot::blank(80, 24),
        };
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let padding = mouse::Cursor::Available(Point::new(790.0, 590.0));
        let mut passive = TerminalCanvasState {
            tab_id: Some(10),
            was_inside: true,
            last_cell: Some((5, 3)),
            ..TerminalCanvasState::default()
        };
        let leave = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut passive,
            &canvas::Event::Mouse(mouse::Event::CursorMoved {
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

        let mut captured = TerminalCanvasState {
            tab_id: Some(10),
            pressed: Some(PointerButton::Left),
            was_inside: true,
            last_cell: Some((5, 3)),
            ..TerminalCanvasState::default()
        };
        let motion = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut captured,
            &canvas::Event::Mouse(mouse::Event::CursorMoved {
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
        let program = TerminalCanvas {
            tab_id: 11,
            snapshot: TerminalSnapshot::blank(80, 24),
        };
        let start = Instant::now();
        let mut state = TerminalCanvasState {
            tab_id: Some(11),
            clicks: ClickTracker {
                sequence: Some(ClickSequence {
                    tab_id: 11,
                    cell: (4, 2),
                    at: start,
                    count: 1,
                }),
            },
            ..TerminalCanvasState::default()
        };
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let cursor = mouse::Cursor::Available(Point::new(
            TERMINAL_PADDING + 4.5 * CELL_WIDTH,
            TERMINAL_PADDING + 2.5 * CELL_HEIGHT,
        ));
        let _ = <TerminalCanvas as canvas::Program<crate::Message>>::update(
            &program,
            &mut state,
            &canvas::Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            bounds,
            cursor,
        );
        assert!(state.clicks.sequence.is_none());
    }

    #[test]
    fn link_interaction_is_confined_to_canvas_bounds() {
        let mut snapshot = TerminalSnapshot::blank(80, 24);
        snapshot.link_hover = Some(SelectionSpan {
            row: 0,
            col0: 0,
            col1: 4,
        });
        snapshot.pointer_shape = "pointer".into();
        let program = TerminalCanvas {
            tab_id: 1,
            snapshot,
        };
        let state = TerminalCanvasState::default();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        assert_eq!(
            <TerminalCanvas as canvas::Program<crate::Message>>::mouse_interaction(
                &program,
                &state,
                bounds,
                mouse::Cursor::Available(Point::new(20.0, 20.0)),
            ),
            mouse::Interaction::Pointer
        );
        assert_eq!(
            <TerminalCanvas as canvas::Program<crate::Message>>::mouse_interaction(
                &program,
                &state,
                bounds,
                mouse::Cursor::Available(Point::new(790.0, 590.0)),
            ),
            mouse::Interaction::default()
        );
        assert_eq!(
            <TerminalCanvas as canvas::Program<crate::Message>>::mouse_interaction(
                &program,
                &state,
                bounds,
                mouse::Cursor::Available(Point::new(900.0, 20.0)),
            ),
            mouse::Interaction::default()
        );
        assert_eq!(
            pointer_interaction("crosshair"),
            mouse::Interaction::Crosshair
        );
        assert_eq!(pointer_interaction("unknown"), mouse::Interaction::Text);
    }
}
