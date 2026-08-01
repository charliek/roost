use iced::widget::canvas;
use iced::{mouse, Color, Font, Point, Rectangle, Renderer, Size, Theme};
use roost_engine::pointer::{PointerAction, PointerButton};
use roost_vt::{ColorRgb, CursorInfo, CursorVisualStyle};

pub const CELL_WIDTH: f32 = 8.4;
pub const CELL_HEIGHT: f32 = 18.0;
pub const TERMINAL_PADDING: f32 = 12.0;

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
        }
    }
}

#[derive(Debug, Clone)]
pub struct TerminalCanvas {
    pub tab_id: i64,
    pub snapshot: TerminalSnapshot,
    pub mouse_tracking: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalPointer {
    pub action: PointerAction,
    pub button: Option<PointerButton>,
    pub col: u32,
    pub row: u32,
}

#[derive(Default)]
pub(crate) struct TerminalCanvasState {
    tab_id: Option<i64>,
    pressed: Option<PointerButton>,
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
        }
        if !self.mouse_tracking {
            state.pressed = None;
            return None;
        }
        let point = cursor.position_in(bounds)?;
        let (col, row) = cell_at(point, self.snapshot.cols, self.snapshot.rows)?;
        let pointer = match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                let button = mouse_button(*button)?;
                state.pressed = Some(button);
                TerminalPointer {
                    action: PointerAction::Press,
                    button: Some(button),
                    col,
                    row,
                }
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(button)) => {
                let button = mouse_button(*button)?;
                state.pressed = None;
                TerminalPointer {
                    action: PointerAction::Release,
                    button: Some(button),
                    col,
                    row,
                }
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => TerminalPointer {
                action: PointerAction::Motion,
                button: state.pressed,
                col,
                row,
            },
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let vertical = match delta {
                    mouse::ScrollDelta::Lines { y, .. } | mouse::ScrollDelta::Pixels { y, .. } => {
                        *y
                    }
                };
                if vertical == 0.0 {
                    return None;
                }
                TerminalPointer {
                    action: PointerAction::Press,
                    button: Some(if vertical > 0.0 {
                        PointerButton::Four
                    } else {
                        PointerButton::Five
                    }),
                    col,
                    row,
                }
            }
            _ => return None,
        };
        Some(canvas::Action::publish(crate::Message::TerminalPointer(
            pointer,
        )))
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

fn mouse_button(button: mouse::Button) -> Option<PointerButton> {
    match button {
        mouse::Button::Left => Some(PointerButton::Left),
        mouse::Button::Right => Some(PointerButton::Right),
        mouse::Button::Middle => Some(PointerButton::Middle),
        _ => None,
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
    }

    #[test]
    fn native_press_state_is_carried_into_drag_motion() {
        let program = TerminalCanvas {
            tab_id: 42,
            snapshot: TerminalSnapshot::blank(80, 24),
            mouse_tracking: true,
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
        let crate::Message::TerminalPointer(press) = press else {
            panic!("unexpected press message")
        };
        assert_eq!(press.action, PointerAction::Press);
        assert_eq!(press.button, Some(PointerButton::Left));
        assert_eq!((press.col, press.row), (5, 3));

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
        let crate::Message::TerminalPointer(motion) = motion else {
            panic!("unexpected motion message")
        };
        assert_eq!(motion.action, PointerAction::Motion);
        assert_eq!(motion.button, Some(PointerButton::Left));
    }
}
