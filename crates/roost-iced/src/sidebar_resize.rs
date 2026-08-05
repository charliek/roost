//! Pointer-drag resize adapter for the sidebar/terminal seam. The grip wraps
//! the expanded split so it can claim seam events before the sidebar or the
//! terminal see them; the collapsed layout is rendered unwrapped.

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::{window, Element, Event, Length, Point, Rectangle, Size, Vector};
use roost_engine::{SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH};

use crate::Message;

/// Half-width of the pointer hit zone straddling the seam.
const GRIP_HALF_WIDTH: f32 = 3.0;

const MIN_WIDTH: f32 = SIDEBAR_MIN_WIDTH as f32;
const MAX_WIDTH: f32 = SIDEBAR_MAX_WIDTH as f32;

pub(crate) struct SidebarResizeGrip<'a> {
    content: Element<'a, Message>,
    current_width: f32,
}

impl<'a> SidebarResizeGrip<'a> {
    pub(crate) fn new(content: impl Into<Element<'a, Message>>, current_width: f32) -> Self {
        Self {
            content: content.into(),
            current_width,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    start_x: f32,
    start_width: f32,
}

#[derive(Debug, Default)]
struct State {
    drag: Option<Drag>,
    last_cursor: Option<Point>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum GripEvent {
    Dragged { width: f32 },
    Ended,
}

/// `capture_event` alone does not stop delegation, so ownership has to be
/// decided before the content is offered the event at all.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ownership {
    Delegate,
    /// Captured by the grip; the content never sees the event.
    Own(Option<GripEvent>),
    /// The grip reacts, but the event still belongs to the whole tree.
    PublishAndDelegate(GripEvent),
}

fn seam_x(layout: Layout<'_>) -> Option<f32> {
    layout.children().next().map(|sidebar| {
        let bounds = sidebar.bounds();
        bounds.x + bounds.width
    })
}

fn cursor_in_zone(bounds: Rectangle, seam_x: f32, position: Point) -> bool {
    position.y >= bounds.y
        && position.y <= bounds.y + bounds.height
        && (position.x - seam_x).abs() <= GRIP_HALF_WIDTH
}

fn over_seam_at(layout: Layout<'_>, position: Point) -> bool {
    seam_x(layout).is_some_and(|seam| cursor_in_zone(layout.bounds(), seam, position))
}

fn over_seam(layout: Layout<'_>, cursor: mouse::Cursor) -> bool {
    cursor
        .position()
        .is_some_and(|position| over_seam_at(layout, position))
}

fn dragged_width(drag: Drag, x: f32) -> f32 {
    (drag.start_width + (x - drag.start_x)).clamp(MIN_WIDTH, MAX_WIDTH)
}

fn owns_event(
    state: &mut State,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    current_width: f32,
) -> Ownership {
    // Recorded before every early return: `ButtonPressed` carries no position
    // of its own, and iced hit-tests it against the newest cursor of the batch
    // it was drained with. A frame behind, that is wherever the pointer
    // travelled *after* the button went down, so the last move the grip
    // actually saw is the honest press anchor (issue #295). A pointer that
    // left the window invalidates it — the next entry can land anywhere, and
    // no move need be observed before the press.
    match event {
        Event::Mouse(mouse::Event::CursorMoved { position, .. }) => {
            state.last_cursor = Some(*position);
        }
        Event::Mouse(mouse::Event::CursorLeft) => state.last_cursor = None,
        _ => {}
    }
    // Anchoring only ever *replaces* an available batch cursor. With no cursor
    // at all the grip has always been a no-op, and a stale anchor must not
    // start arming presses it used to ignore.
    let anchored = cursor
        .position()
        .map(|batch| state.last_cursor.unwrap_or(batch));

    match event {
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
            // A second press during a live drag belongs to the grip too: it
            // owns the matching release, so letting the content have the press
            // starts a terminal selection that can never be completed.
            if state.drag.is_some() {
                return Ownership::Own(None);
            }
            let Some(position) = anchored.filter(|position| over_seam_at(layout, *position)) else {
                return Ownership::Delegate;
            };
            state.drag = Some(Drag {
                start_x: position.x,
                start_width: current_width,
            });
            Ownership::Own(None)
        }
        // The move's own position keeps the drag live once the pointer leaves
        // the window, where `mouse::Cursor` reports nothing.
        Event::Mouse(mouse::Event::CursorMoved { position, .. }) => match state.drag {
            Some(drag) => {
                // A pointer travelling past a clamp bound recomputes the same
                // width every move; publishing it would request a redraw per
                // move for a frame that cannot differ.
                let width = dragged_width(drag, position.x);
                Ownership::Own((width != current_width).then_some(GripEvent::Dragged { width }))
            }
            None => Ownership::Delegate,
        },
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            if state.drag.take().is_none() {
                return Ownership::Delegate;
            }
            Ownership::Own(Some(GripEvent::Ended))
        }
        // A half-finished resize is a valid width, so losing focus commits it
        // rather than reverting. Unfocus is broadcast to the whole tree, so it
        // still has to reach the content — a focused text input needs it.
        Event::Window(window::Event::Unfocused) => {
            if state.drag.take().is_none() {
                return Ownership::Delegate;
            }
            Ownership::PublishAndDelegate(GripEvent::Ended)
        }
        _ => Ownership::Delegate,
    }
}

fn grip_message(event: GripEvent) -> Message {
    match event {
        GripEvent::Dragged { width } => Message::SidebarResizeDragged { width },
        GripEvent::Ended => Message::SidebarResizeEnded,
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for SidebarResizeGrip<'_> {
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
        match owns_event(
            tree.state.downcast_mut::<State>(),
            event,
            layout,
            cursor,
            self.current_width,
        ) {
            Ownership::Delegate => {}
            Ownership::PublishAndDelegate(event) => shell.publish(grip_message(event)),
            Ownership::Own(publish) => {
                if let Some(event) = publish {
                    shell.publish(grip_message(event));
                }
                shell.capture_event();
                return;
            }
        }

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
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        // Short-circuit: the terminal reports a text cursor over its whole
        // surface, so asking the content first would always lose the seam.
        if over_seam(layout, cursor) || tree.state.downcast_ref::<State>().drag.is_some() {
            return mouse::Interaction::ResizingHorizontally;
        }
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
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<SidebarResizeGrip<'a>> for Element<'a, Message> {
    fn from(grip: SidebarResizeGrip<'a>) -> Self {
        Element::new(grip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sidebar_width: f32) -> layout::Node {
        let sidebar = layout::Node::new(Size::new(sidebar_width, 400.0));
        let main = layout::Node::new(Size::new(800.0 - sidebar_width, 400.0))
            .move_to(Point::new(sidebar_width, 0.0));
        layout::Node::with_children(Size::new(800.0, 400.0), vec![sidebar, main])
    }

    fn press() -> Event {
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
    }

    fn release() -> Event {
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
    }

    fn moved(x: f32) -> Event {
        Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(x, 20.0),
        })
    }

    fn left() -> Event {
        Event::Mouse(mouse::Event::CursorLeft)
    }

    fn dragging() -> State {
        State {
            drag: Some(Drag {
                start_x: 220.0,
                start_width: 220.0,
            }),
            ..State::default()
        }
    }

    /// Drives the real ownership path: the seam zone is computed from
    /// `sidebar_width`'s layout, never handed in precomputed.
    fn owns(
        state: &mut State,
        event: &Event,
        sidebar_width: f32,
        cursor_x: Option<f32>,
        current_width: f32,
    ) -> Ownership {
        let node = split(sidebar_width);
        let cursor = cursor_x.map_or(mouse::Cursor::Unavailable, |x| {
            mouse::Cursor::Available(Point::new(x, 20.0))
        });
        owns_event(state, event, Layout::new(&node), cursor, current_width)
    }

    #[test]
    fn seam_zone_straddles_the_sidebar_edge_and_respects_vertical_bounds() {
        let node = split(220.0);
        let layout = Layout::new(&node);
        let seam = seam_x(layout).expect("split has a sidebar child");
        assert_eq!(seam, 220.0);

        let bounds = layout.bounds();
        let at = |x, y| cursor_in_zone(bounds, seam, Point::new(x, y));
        assert!(at(220.0, 200.0));
        assert!(at(217.0, 200.0));
        assert!(at(223.0, 200.0));
        assert!(!at(216.9, 200.0));
        assert!(!at(223.1, 200.0));
        assert!(!at(220.0, -0.1));
        assert!(!at(220.0, 400.1));
    }

    #[test]
    fn dragged_width_tracks_the_pointer_and_clamps_to_the_engine_bounds() {
        let drag = Drag {
            start_x: 220.0,
            start_width: 220.0,
        };
        assert_eq!(dragged_width(drag, 220.0), 220.0);
        assert_eq!(dragged_width(drag, 300.0), 300.0);
        assert_eq!(dragged_width(drag, 160.0), MIN_WIDTH);
        assert_eq!(dragged_width(drag, 100.0), MIN_WIDTH);
        assert_eq!(dragged_width(drag, 900.0), MAX_WIDTH);

        // A drag started off-center keeps the grab offset.
        let offset = Drag {
            start_x: 222.0,
            start_width: 220.0,
        };
        assert_eq!(dragged_width(offset, 262.0), 260.0);
    }

    #[test]
    fn seam_press_is_owned_and_a_press_elsewhere_is_delegated() {
        let mut state = State::default();
        assert_eq!(
            owns(&mut state, &press(), 220.0, Some(221.0), 220.0),
            Ownership::Own(None)
        );
        assert_eq!(
            state.drag,
            Some(Drag {
                start_x: 221.0,
                start_width: 220.0,
            })
        );

        let mut elsewhere = State::default();
        assert_eq!(
            owns(&mut elsewhere, &press(), 220.0, Some(600.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(elsewhere.drag, None);
    }

    #[test]
    fn motion_and_release_are_owned_only_while_a_drag_is_live() {
        let mut idle = State::default();
        assert_eq!(
            owns(&mut idle, &moved(600.0), 220.0, Some(600.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut idle, &release(), 220.0, Some(600.0), 220.0),
            Ownership::Delegate
        );

        let mut live = dragging();
        assert_eq!(
            owns(&mut live, &moved(300.0), 220.0, Some(300.0), 220.0),
            Ownership::Own(Some(GripEvent::Dragged { width: 300.0 }))
        );
        // The pointer left the window: the drag still owns the motion.
        assert_eq!(
            owns(&mut live, &moved(280.0), 220.0, None, 220.0),
            Ownership::Own(Some(GripEvent::Dragged { width: 280.0 }))
        );
        assert_eq!(
            owns(&mut live, &release(), 220.0, None, 220.0),
            Ownership::Own(Some(GripEvent::Ended))
        );
        assert_eq!(live.drag, None);
        assert_eq!(
            owns(&mut live, &release(), 220.0, None, 220.0),
            Ownership::Delegate
        );
    }

    #[test]
    fn a_move_that_recomputes_the_current_width_owns_without_publishing() {
        let mut live = dragging();
        assert_eq!(
            owns(&mut live, &moved(220.0), 220.0, Some(220.0), 220.0),
            Ownership::Own(None)
        );
        // Past the clamp bound the width stops moving, so neither does the app.
        let mut clamped = State {
            drag: Some(Drag {
                start_x: 220.0,
                start_width: MIN_WIDTH,
            }),
            ..State::default()
        };
        assert_eq!(
            owns(&mut clamped, &moved(100.0), 220.0, Some(100.0), MIN_WIDTH),
            Ownership::Own(None)
        );
    }

    #[test]
    fn unfocus_commits_a_live_drag_and_still_reaches_the_content() {
        let mut live = dragging();
        let unfocused = Event::Window(window::Event::Unfocused);
        assert_eq!(
            owns(&mut live, &unfocused, 220.0, None, 220.0),
            Ownership::PublishAndDelegate(GripEvent::Ended)
        );
        assert_eq!(live.drag, None);
        assert_eq!(
            owns(&mut live, &unfocused, 220.0, None, 220.0),
            Ownership::Delegate
        );
    }

    #[test]
    fn a_second_press_during_a_drag_is_owned_without_restarting_the_gesture() {
        let anchor = Drag {
            start_x: 220.0,
            start_width: 220.0,
        };
        for cursor_x in [Some(221.0), Some(600.0), None] {
            let mut live = dragging();
            assert_eq!(
                owns(&mut live, &press(), 220.0, cursor_x, 300.0),
                Ownership::Own(None),
                "press at {cursor_x:?} during a drag must never reach the content"
            );
            assert_eq!(live.drag, Some(anchor), "the original anchor must survive");
        }
    }

    #[test]
    fn unrelated_events_are_always_delegated() {
        let mut live = dragging();
        let right_press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right));
        assert_eq!(
            owns(&mut live, &right_press, 220.0, Some(220.0), 220.0),
            Ownership::Delegate
        );
        assert!(live.drag.is_some());
    }

    #[test]
    fn drag_start_width_comes_from_the_constructor_not_the_seam_position() {
        // The sidebar's rendered edge and the widget's reported width can
        // disagree mid-frame; the press must anchor on the reported width.
        let mut state = State::default();
        assert!(matches!(
            owns(&mut state, &press(), 300.0, Some(300.0), 300.0),
            Ownership::Own { .. }
        ));
        assert_eq!(
            owns(&mut state, &moved(340.0), 300.0, Some(340.0), 300.0),
            Ownership::Own(Some(GripEvent::Dragged { width: 340.0 }))
        );
    }

    #[test]
    fn a_press_is_hit_tested_where_the_last_move_landed_not_at_the_batch_cursor() {
        // The batch reports 600 for both events — a frame-behind UI drains
        // the press together with the move that followed it.
        let mut state = State::default();
        assert_eq!(
            owns(&mut state, &moved(221.0), 220.0, Some(600.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut state, &press(), 220.0, Some(600.0), 220.0),
            Ownership::Own(None)
        );
        assert_eq!(
            state.drag,
            Some(Drag {
                start_x: 221.0,
                start_width: 220.0,
            })
        );

        // The inverse: a press whose honest position left the zone must not
        // be rescued by a batch cursor that drifted back onto the seam.
        let mut away = State::default();
        assert_eq!(
            owns(&mut away, &moved(600.0), 220.0, Some(221.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut away, &press(), 220.0, Some(221.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(away.drag, None);
    }

    #[test]
    fn a_press_before_any_move_falls_back_to_the_batch_cursor() {
        let mut state = State::default();
        assert_eq!(
            owns(&mut state, &press(), 220.0, Some(222.0), 220.0),
            Ownership::Own(None)
        );
        assert_eq!(
            state.drag,
            Some(Drag {
                start_x: 222.0,
                start_width: 220.0,
            })
        );

        let mut outside = State::default();
        assert_eq!(
            owns(&mut outside, &press(), 220.0, Some(600.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(outside.drag, None);
    }

    #[test]
    fn a_pointer_that_left_the_window_drops_the_anchor() {
        // Re-entering somewhere else and pressing before any move is observed
        // must not be hit-tested where the pointer used to be.
        let mut state = State::default();
        assert_eq!(
            owns(&mut state, &moved(221.0), 220.0, Some(221.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut state, &left(), 220.0, None, 220.0),
            Ownership::Delegate
        );
        assert_eq!(state.last_cursor, None);
        assert_eq!(
            owns(&mut state, &press(), 220.0, Some(600.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(state.drag, None);
    }

    #[test]
    fn a_press_with_no_batch_cursor_stays_a_no_op_however_fresh_the_anchor() {
        let mut state = State::default();
        assert_eq!(
            owns(&mut state, &moved(221.0), 220.0, Some(221.0), 220.0),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut state, &press(), 220.0, None, 220.0),
            Ownership::Delegate
        );
        assert_eq!(state.drag, None);
    }
}
