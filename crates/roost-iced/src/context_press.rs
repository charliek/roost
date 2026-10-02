//! The right-click on one row — a tab pill, a project row, a host band —
//! published with where it landed in the window (plan 073 D9).

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::{keyboard, Element, Event, Length, Point, Rectangle, Size, Vector};
use roost_ui_model::context_menu::ContextTarget;

use crate::Message;

/// Wraps exactly one row, so a reorder strip around it still finds one
/// layout child per id. The row sees a right press first, and only one
/// nothing inside it claimed opens the menu — the strips own the left
/// button alone, so reorder never sees a difference.
pub(crate) struct ContextPressArea<'a> {
    content: Element<'a, Message>,
    target: ContextTarget,
    /// The window's modifiers as the app last heard them.
    modifiers: keyboard::Modifiers,
}

impl<'a> ContextPressArea<'a> {
    pub(crate) fn new(
        content: impl Into<Element<'a, Message>>,
        target: ContextTarget,
        modifiers: keyboard::Modifiers,
    ) -> Self {
        Self {
            content: content.into(),
            target,
            modifiers,
        }
    }
}

/// The scroll translation above this row. A scrollable hands its
/// children the cursor in its content's coordinates, and iced passes the
/// way back to the window only to `overlay` — so it is kept from there
/// for the press, which the menu is drawn at over the whole window.
#[derive(Debug)]
struct State {
    translation: Vector,
    /// A mouse press carries no modifiers, so they are kept here: the
    /// window's, set at every view build — a row made while Control is
    /// already down hears no `ModifiersChanged` until it moves — and then
    /// the keyboard events since, which reach the row ahead of a press
    /// later in the same batch.
    modifiers: keyboard::Modifiers,
}

/// A press that opens the row's menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuPress {
    /// The right button.
    Secondary,
    /// macOS's control-click, which is its right-click and never a left
    /// press: it is claimed before the row sees it, so neither a button
    /// inside the row (a pill's ×, a host's Update) nor the strip around
    /// it takes it for one.
    ControlClick,
}

fn opens_menu(event: &Event, modifiers: keyboard::Modifiers) -> Option<MenuPress> {
    match event {
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
            Some(MenuPress::Secondary)
        }
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            if cfg!(target_os = "macos") && modifiers.control() =>
        {
            Some(MenuPress::ControlClick)
        }
        _ => None,
    }
}

/// A menu press over `bounds`, and where it lands in the window.
fn menu_press(
    event: &Event,
    modifiers: keyboard::Modifiers,
    cursor: mouse::Cursor,
    bounds: Rectangle,
    translation: Vector,
) -> Option<(MenuPress, Point)> {
    let press = opens_menu(event, modifiers)?;
    cursor
        .position_over(bounds)
        .map(|position| (press, position + translation))
}

/// Publish `request` for `press`, on the right side of the row's own
/// handling of the event (`update_row`).
fn route_press<Message>(
    press: Option<(MenuPress, Point)>,
    shell: &mut Shell<'_, Message>,
    request: impl FnOnce(Point) -> Message,
    update_row: impl FnOnce(&mut Shell<'_, Message>),
) {
    if let Some((MenuPress::ControlClick, at)) = press {
        shell.publish(request(at));
        shell.capture_event();
        return;
    }
    update_row(shell);
    if shell.is_event_captured() {
        return;
    }
    if let Some((MenuPress::Secondary, at)) = press {
        shell.publish(request(at));
        shell.capture_event();
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for ContextPressArea<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            translation: Vector::ZERO,
            modifiers: self.modifiers,
        })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.state.downcast_mut::<State>().modifiers = self.modifiers;
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
        let state = tree.state.downcast_mut::<State>();
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) = event {
            state.modifiers = *modifiers;
        }
        let press = menu_press(
            event,
            state.modifiers,
            cursor,
            layout.bounds(),
            state.translation,
        );
        let target = &self.target;
        let content = &mut self.content;
        route_press(
            press,
            shell,
            |at| Message::ContextMenuRequested {
                target: target.clone(),
                at,
            },
            |shell| {
                content.as_widget_mut().update(
                    &mut tree.children[0],
                    event,
                    layout,
                    cursor,
                    renderer,
                    clipboard,
                    shell,
                    viewport,
                );
            },
        );
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
        let none = keyboard::Modifiers::empty();
        assert_eq!(
            menu_press(&press(mouse::Button::Right), none, cursor, row, scrolled),
            Some((MenuPress::Secondary, Point::new(40.0, 60.0)))
        );
        assert_eq!(
            menu_press(&press(mouse::Button::Left), none, cursor, row, scrolled),
            None,
            "the left button stays the strip's"
        );
        let outside = mouse::Cursor::Available(Point::new(40.0, 20.0));
        assert_eq!(
            menu_press(&press(mouse::Button::Right), none, outside, row, scrolled),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_control_click_opens_the_menu_on_macos() {
        let left = press(mouse::Button::Left);
        assert_eq!(
            opens_menu(&left, keyboard::Modifiers::CTRL),
            Some(MenuPress::ControlClick)
        );
        assert_eq!(opens_menu(&left, keyboard::Modifiers::empty()), None);
        assert_eq!(
            opens_menu(&left, keyboard::Modifiers::LOGO),
            None,
            "a command-click is not a right-click"
        );
        assert_eq!(
            opens_menu(&press(mouse::Button::Right), keyboard::Modifiers::CTRL),
            Some(MenuPress::Secondary)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_control_click_stays_the_rows_off_macos() {
        assert_eq!(
            opens_menu(&press(mouse::Button::Left), keyboard::Modifiers::CTRL),
            None
        );
    }

    fn row(modifiers: keyboard::Modifiers) -> Element<'static, Message> {
        ContextPressArea::new(
            iced::widget::Space::new(),
            ContextTarget::Tab(roost_ui_model::keys::TabKey::local(1)),
            modifiers,
        )
        .into()
    }

    fn held(tree: &Tree) -> keyboard::Modifiers {
        tree.state.downcast_ref::<State>().modifiers
    }

    #[test]
    fn a_row_takes_the_windows_modifiers_at_every_view_build() {
        // Control is already down when the row is made, so no
        // `ModifiersChanged` is coming to tell it.
        let mut tree = Tree::new(row(keyboard::Modifiers::CTRL));
        assert_eq!(held(&tree), keyboard::Modifiers::CTRL);
        #[cfg(target_os = "macos")]
        assert_eq!(
            opens_menu(&press(mouse::Button::Left), held(&tree)),
            Some(MenuPress::ControlClick)
        );
        tree.diff(row(keyboard::Modifiers::empty()));
        assert_eq!(
            held(&tree),
            keyboard::Modifiers::empty(),
            "a later build carries the window's modifiers over the row's"
        );
    }

    /// The messages and capture `press` leaves behind, around a row that
    /// publishes `"row"` and, when `row_claims`, captures the event — as
    /// a pill's × button and a dimmed host's pill capture a left press.
    fn routed(
        press: Option<(MenuPress, Point)>,
        row_claims: bool,
    ) -> (Vec<String>, iced::event::Status) {
        let mut messages = Vec::new();
        let status = {
            let mut shell = Shell::new(&mut messages);
            route_press(
                press,
                &mut shell,
                |at| format!("menu at {},{}", at.x, at.y),
                |shell| {
                    shell.publish("row".to_string());
                    if row_claims {
                        shell.capture_event();
                    }
                },
            );
            shell.event_status()
        };
        (messages, status)
    }

    #[test]
    fn a_control_click_is_claimed_before_the_row_and_a_right_press_after_it() {
        use iced::event::Status;

        let at = Point::new(4.0, 5.0);
        assert_eq!(
            routed(Some((MenuPress::ControlClick, at)), true),
            (vec!["menu at 4,5".to_string()], Status::Captured),
            "the row never sees a control-click"
        );
        assert_eq!(
            routed(Some((MenuPress::Secondary, at)), true),
            (vec!["row".to_string()], Status::Captured),
            "a right press the row claims stays the row's"
        );
        assert_eq!(
            routed(Some((MenuPress::Secondary, at)), false),
            (
                vec!["row".to_string(), "menu at 4,5".to_string()],
                Status::Captured
            )
        );
        assert_eq!(
            routed(None, false),
            (vec!["row".to_string()], Status::Ignored)
        );
    }
}
