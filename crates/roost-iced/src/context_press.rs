//! The right-click on one row — a tab pill, a project row, a host band —
//! published with where it landed in the window (plan 073 D9).

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector};
use roost_ui_model::context_menu::ContextTarget;

use crate::Message;

/// Wraps exactly one row, so a reorder strip around it still finds one
/// layout child per id. The row sees every event first, and only a press
/// nothing inside it claimed opens the menu — the strips own the left
/// button alone, so reorder never sees a difference.
pub(crate) struct ContextPressArea<'a> {
    content: Element<'a, Message>,
    target: ContextTarget,
}

impl<'a> ContextPressArea<'a> {
    pub(crate) fn new(content: impl Into<Element<'a, Message>>, target: ContextTarget) -> Self {
        Self {
            content: content.into(),
            target,
        }
    }
}

/// The scroll translation above this row. A scrollable hands its
/// children the cursor in its content's coordinates, and iced passes the
/// way back to the window only to `overlay` — so it is kept from there
/// for the press, which the menu is drawn at over the whole window.
#[derive(Debug, Default)]
struct State {
    translation: Vector,
}

fn opens_menu(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right))
    )
}

/// Where a menu press over `bounds` lands in the window.
fn menu_press(
    event: &Event,
    cursor: mouse::Cursor,
    bounds: Rectangle,
    translation: Vector,
) -> Option<Point> {
    if !opens_menu(event) {
        return None;
    }
    cursor
        .position_over(bounds)
        .map(|position| position + translation)
}

impl Widget<Message, iced::Theme, iced::Renderer> for ContextPressArea<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if shell.is_event_captured() {
            return;
        }
        let translation = tree.state.downcast_ref::<State>().translation;
        if let Some(at) = menu_press(event, cursor, layout.bounds(), translation) {
            shell.publish(Message::ContextMenuRequested {
                target: self.target.clone(),
                at,
            });
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        tree.state.downcast_mut::<State>().translation = translation;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<ContextPressArea<'a>> for Element<'a, Message> {
    fn from(area: ContextPressArea<'a>) -> Self {
        Element::new(area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(button: mouse::Button) -> Event {
        Event::Mouse(mouse::Event::ButtonPressed(button))
    }

    #[test]
    fn a_right_press_on_a_scrolled_row_lands_where_the_window_drew_it() {
        let row = Rectangle::new(Point::new(0.0, 300.0), Size::new(200.0, 32.0));
        // Scrolled 250 down: the row is drawn at y 50..82, and the
        // scrollable hands its children the cursor 250 lower.
        let scrolled = Vector::new(0.0, -250.0);
        let cursor = mouse::Cursor::Available(Point::new(40.0, 310.0));
        assert_eq!(
            menu_press(&press(mouse::Button::Right), cursor, row, scrolled),
            Some(Point::new(40.0, 60.0))
        );
        assert_eq!(
            menu_press(&press(mouse::Button::Left), cursor, row, scrolled),
            None,
            "the left button stays the strip's"
        );
        let outside = mouse::Cursor::Available(Point::new(40.0, 20.0));
        assert_eq!(
            menu_press(&press(mouse::Button::Right), outside, row, scrolled),
            None
        );
    }
}
