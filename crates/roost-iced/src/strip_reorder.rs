//! Native pointer gesture adapter for stable-ID reordering strips. The
//! tab strip is one instantiation; the widget is axis- and
//! message-parametrized so a vertical strip can reuse the same gesture
//! state machine.

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::{window, Element, Event, Length, Point, Rectangle, Size, Vector};
use roost_ui_model::reorder::{moved_ids, ReorderError};

use crate::Message;

const DRAG_THRESHOLD: f32 = 8.0;

/// Provides a same-event root release fallback after direct child ownership.
/// The boundary delegates without changing capture, then publishes for an
/// application-owned preview even when a reflowed scrollable withheld the
/// event from the tab strip.
pub(crate) struct ReleaseBoundary<'a> {
    content: Element<'a, Message>,
    enabled: bool,
}

impl<'a> ReleaseBoundary<'a> {
    pub(crate) fn new(content: impl Into<Element<'a, Message>>, enabled: bool) -> Self {
        Self {
            content: content.into(),
            enabled,
        }
    }
}

fn release_after_child<Message>(
    event: &Event,
    shell: &mut Shell<'_, Message>,
    enabled: bool,
    release: impl FnOnce() -> Message,
    update_child: impl FnOnce(&mut Shell<'_, Message>),
) {
    update_child(shell);
    if enabled
        && matches!(
            event,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
        )
    {
        tracing::debug!("Iced root observed left-button release");
        shell.publish(release());
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for ReleaseBoundary<'_> {
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
        let content = &mut self.content;
        let child_tree = &mut tree.children[0];
        release_after_child(
            event,
            shell,
            self.enabled,
            || Message::StripPointerReleased,
            |shell| {
                content.as_widget_mut().update(
                    child_tree, event, layout, cursor, renderer, clipboard, shell, viewport,
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
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<ReleaseBoundary<'a>> for Element<'a, Message> {
    fn from(boundary: ReleaseBoundary<'a>) -> Self {
        Element::new(boundary)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Axis {
    Horizontal,
    // Only the unit tests instantiate this until the vertical (project)
    // strip call site lands.
    #[allow(dead_code)]
    Vertical,
}

/// `scope_id` scopes a gesture to the surface it started on (the active
/// project id for the tab strip, `0` for the project list) — it is only
/// ever compared for equality, never run through `same_members`' zero
/// reject, which applies to the item-id list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StripEvent {
    Started {
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
    },
    DragBegan {
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
    },
    Preview {
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
        ordered_ids: Vec<i64>,
    },
    Commit {
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
        ordered_ids: Vec<i64>,
    },
    Ended {
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
    },
    Cancel {
        context_generation: u64,
    },
}

pub(crate) struct ReorderStrip<'a> {
    content: Element<'a, Message>,
    axis: Axis,
    scope_id: i64,
    ids: Vec<i64>,
    context_generation: u64,
    enabled: bool,
    on_select: fn(i64) -> Message,
    on_rename: fn(i64) -> Message,
    on_event: fn(StripEvent) -> Message,
}

impl<'a> ReorderStrip<'a> {
    pub(crate) fn tabs(
        content: impl Into<Element<'a, Message>>,
        project_id: i64,
        ids: Vec<i64>,
        context_generation: u64,
        enabled: bool,
    ) -> Self {
        Self {
            content: content.into(),
            axis: Axis::Horizontal,
            scope_id: project_id,
            ids,
            context_generation,
            enabled,
            on_select: Message::TabSelected,
            on_rename: Message::BeginRenameTab,
            on_event: Message::TabStrip,
        }
    }
}

#[derive(Debug)]
struct Gesture {
    scope_id: i64,
    source_id: i64,
    original_ids: Vec<i64>,
    ordered_ids: Vec<i64>,
    origin: Point,
    context_generation: u64,
    dragging: bool,
}

#[derive(Clone, Copy, Debug)]
struct ScopedClick {
    click: mouse::Click,
    scope_id: i64,
    source_id: i64,
    context_generation: u64,
}

#[derive(Debug, Default)]
struct State {
    gesture: Option<Gesture>,
    previous_click: Option<ScopedClick>,
}

#[derive(Debug, PartialEq, Eq)]
enum ReleaseSettlement {
    Unowned,
    Owned(Option<StripEvent>),
}

#[derive(Debug, PartialEq, Eq)]
enum PressSettlement {
    Started(StripEvent),
    DoubleClick,
}

impl State {
    fn previous_click_for(
        &self,
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
    ) -> Option<mouse::Click> {
        self.previous_click
            .filter(|previous| {
                previous.scope_id == scope_id
                    && previous.source_id == source_id
                    && previous.context_generation == context_generation
            })
            .map(|previous| previous.click)
    }

    fn settle_release(
        &mut self,
        scope_id: i64,
        ids: &[i64],
        context_generation: u64,
    ) -> Option<StripEvent> {
        let gesture = self.gesture.take()?;
        if !context_valid(&gesture, scope_id, ids, context_generation) {
            Some(StripEvent::Cancel {
                context_generation: gesture.context_generation,
            })
        } else if gesture.dragging {
            Some(StripEvent::Commit {
                scope_id: gesture.scope_id,
                source_id: gesture.source_id,
                context_generation: gesture.context_generation,
                original_ids: gesture.original_ids,
                ordered_ids: gesture.ordered_ids,
            })
        } else {
            Some(StripEvent::Ended {
                scope_id: gesture.scope_id,
                source_id: gesture.source_id,
                context_generation: gesture.context_generation,
                original_ids: gesture.original_ids,
            })
        }
    }

    fn arm_press(
        &mut self,
        position: Point,
        scope_id: i64,
        source_id: i64,
        ids: &[i64],
        context_generation: u64,
    ) -> PressSettlement {
        let click = mouse::Click::new(
            position,
            mouse::Button::Left,
            self.previous_click_for(scope_id, source_id, context_generation),
        );
        self.previous_click = Some(ScopedClick {
            click,
            scope_id,
            source_id,
            context_generation,
        });
        if click.kind() == mouse::click::Kind::Double {
            self.gesture = None;
            PressSettlement::DoubleClick
        } else {
            self.gesture = Some(Gesture {
                scope_id,
                source_id,
                original_ids: ids.to_vec(),
                ordered_ids: ids.to_vec(),
                origin: position,
                context_generation,
                dragging: false,
            });
            PressSettlement::Started(StripEvent::Started {
                scope_id,
                source_id,
                context_generation,
                original_ids: ids.to_vec(),
            })
        }
    }

    /// Threshold crossing is published once per gesture: `Started` fires
    /// on bare press, so only this transition means "a drag is underway".
    fn begin_drag(&mut self, position: Point) -> Option<StripEvent> {
        let gesture = self.gesture.as_mut()?;
        if gesture.dragging || !crossed_threshold(gesture.origin, position) {
            return None;
        }
        gesture.dragging = true;
        Some(StripEvent::DragBegan {
            scope_id: gesture.scope_id,
            source_id: gesture.source_id,
            context_generation: gesture.context_generation,
        })
    }

    fn settle_owned_release(
        &mut self,
        event: &Event,
        scope_id: i64,
        ids: &[i64],
        context_generation: u64,
    ) -> ReleaseSettlement {
        if !matches!(
            event,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
        ) || self.gesture.is_none()
        {
            return ReleaseSettlement::Unowned;
        }
        ReleaseSettlement::Owned(self.settle_release(scope_id, ids, context_generation))
    }
}

fn same_members(ids: &[i64], expected: &[i64]) -> bool {
    if ids.len() != expected.len() || ids.contains(&0) {
        return false;
    }
    let mut left = ids.to_vec();
    let mut right = expected.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left.windows(2).all(|pair| pair[0] != pair[1]) && left == right
}

fn raw_target_index(axis: Axis, layout: Layout<'_>, position: Point) -> usize {
    layout
        .children()
        .take_while(|child| match axis {
            Axis::Horizontal => position.x >= child.bounds().center_x(),
            Axis::Vertical => position.y >= child.bounds().center_y(),
        })
        .count()
}

fn id_at(layout: Layout<'_>, ids: &[i64], position: Point) -> Option<i64> {
    layout
        .children()
        .zip(ids.iter().copied())
        .find_map(|(child, id)| child.bounds().contains(position).then_some(id))
}

fn crossed_threshold(origin: Point, current: Point) -> bool {
    origin.distance(current) >= DRAG_THRESHOLD
}

fn next_preview_order(
    rendered_ids: &[i64],
    source_id: i64,
    raw_target_idx: usize,
    current_preview: &[i64],
) -> Result<Option<Vec<i64>>, ReorderError> {
    let ordered_ids = moved_ids(rendered_ids, source_id, raw_target_idx)?
        .unwrap_or_else(|| rendered_ids.to_vec());
    Ok((ordered_ids != current_preview).then_some(ordered_ids))
}

fn context_valid(gesture: &Gesture, scope_id: i64, ids: &[i64], context_generation: u64) -> bool {
    gesture.context_generation == context_generation
        && gesture.scope_id == scope_id
        && same_members(ids, &gesture.original_ids)
}

impl Widget<Message, iced::Theme, iced::Renderer> for ReorderStrip<'_> {
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
        match tree.state.downcast_mut::<State>().settle_owned_release(
            event,
            self.scope_id,
            &self.ids,
            self.context_generation,
        ) {
            ReleaseSettlement::Unowned => {}
            ReleaseSettlement::Owned(settlement) => {
                if let Some(event) = settlement {
                    shell.publish((self.on_event)(event));
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

        let state = tree.state.downcast_mut::<State>();
        if shell.is_event_captured() {
            if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
                state.previous_click = None;
            }
            return;
        }
        if !self.enabled {
            state.previous_click = None;
            if let Some(gesture) = state.gesture.take() {
                shell.publish((self.on_event)(StripEvent::Cancel {
                    context_generation: gesture.context_generation,
                }));
            }
            return;
        }

        if state.previous_click.is_some_and(|previous| {
            previous.scope_id != self.scope_id
                || previous.context_generation != self.context_generation
                || !self.ids.contains(&previous.source_id)
        }) {
            state.previous_click = None;
        }

        if let Some(gesture) = state.gesture.take_if(|gesture| {
            !context_valid(gesture, self.scope_id, &self.ids, self.context_generation)
        }) {
            state.previous_click = None;
            shell.publish((self.on_event)(StripEvent::Cancel {
                context_generation: gesture.context_generation,
            }));
        }

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(position) = cursor.position() else {
                    return;
                };
                let Some(source_id) = id_at(layout, &self.ids, position) else {
                    return;
                };
                tracing::debug!(
                    ?position,
                    source_id,
                    ids = ?self.ids,
                    child_bounds = ?layout.children().map(|child| child.bounds()).collect::<Vec<_>>(),
                    "Iced reorder strip armed pointer press"
                );
                shell.publish((self.on_select)(source_id));
                match state.arm_press(
                    position,
                    self.scope_id,
                    source_id,
                    &self.ids,
                    self.context_generation,
                ) {
                    PressSettlement::Started(event) => {
                        shell.publish((self.on_event)(event));
                    }
                    PressSettlement::DoubleClick => {
                        shell.publish((self.on_rename)(source_id));
                    }
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if state.gesture.is_none() {
                    return;
                }
                let Some(position) = cursor.position() else {
                    return;
                };
                if let Some(began) = state.begin_drag(position) {
                    shell.publish((self.on_event)(began));
                }
                let Some(gesture) = &mut state.gesture else {
                    return;
                };
                if !gesture.dragging {
                    return;
                }
                let raw_target = raw_target_index(self.axis, layout, position);
                match next_preview_order(
                    &self.ids,
                    gesture.source_id,
                    raw_target,
                    &gesture.ordered_ids,
                ) {
                    Ok(Some(ordered_ids)) => {
                        gesture.ordered_ids.clone_from(&ordered_ids);
                        shell.publish((self.on_event)(StripEvent::Preview {
                            scope_id: gesture.scope_id,
                            source_id: gesture.source_id,
                            context_generation: gesture.context_generation,
                            original_ids: gesture.original_ids.clone(),
                            ordered_ids,
                        }));
                    }
                    Ok(None) => {}
                    Err(_) => {
                        let context_generation = gesture.context_generation;
                        state.gesture = None;
                        shell.publish((self.on_event)(StripEvent::Cancel { context_generation }));
                    }
                }
                shell.capture_event();
            }
            Event::Window(window::Event::Unfocused) => {
                state.previous_click = None;
                if let Some(gesture) = state.gesture.take() {
                    shell.publish((self.on_event)(StripEvent::Cancel {
                        context_generation: gesture.context_generation,
                    }));
                }
            }
            _ => {}
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
        let child = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        );
        if child != mouse::Interaction::None {
            return child;
        }
        if !self.enabled {
            return mouse::Interaction::None;
        }
        let state = tree.state.downcast_ref::<State>();
        if state
            .gesture
            .as_ref()
            .is_some_and(|gesture| gesture.dragging)
        {
            mouse::Interaction::Grabbing
        } else if cursor
            .position()
            .is_some_and(|position| id_at(layout, &self.ids, position).is_some())
        {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::None
        }
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

impl<'a> From<ReorderStrip<'a>> for Element<'a, Message> {
    fn from(strip: ReorderStrip<'a>) -> Self {
        Element::new(strip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_release_follows_child_without_changing_child_capture() {
        let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        let mut messages = Vec::new();
        let status = {
            let mut shell = Shell::new(&mut messages);
            release_after_child(
                &release,
                &mut shell,
                true,
                || "root",
                |shell| {
                    shell.publish("child");
                    shell.capture_event();
                },
            );
            shell.event_status()
        };
        assert_eq!(messages, ["child", "root"]);
        assert_eq!(status, iced::event::Status::Captured);

        messages.clear();
        let status = {
            let mut shell = Shell::new(&mut messages);
            release_after_child(
                &release,
                &mut shell,
                true,
                || "root",
                |shell| {
                    shell.publish("ignored child");
                },
            );
            shell.event_status()
        };
        assert_eq!(messages, ["ignored child", "root"]);
        assert_eq!(status, iced::event::Status::Ignored);

        messages.clear();
        let status = {
            let mut shell = Shell::new(&mut messages);
            release_after_child(
                &release,
                &mut shell,
                false,
                || "root",
                |shell| {
                    shell.publish("disabled child");
                    shell.capture_event();
                },
            );
            shell.event_status()
        };
        assert_eq!(messages, ["disabled child"]);
        assert_eq!(status, iced::event::Status::Captured);

        messages.clear();
        let motion = Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(10.0, 10.0),
        });
        let mut shell = Shell::new(&mut messages);
        release_after_child(
            &motion,
            &mut shell,
            true,
            || "root",
            |shell| {
                shell.publish("motion child");
            },
        );
        assert_eq!(messages, ["motion child"]);
    }

    #[test]
    fn threshold_is_inclusive_and_direction_independent() {
        let origin = Point::new(10.0, 10.0);
        assert!(!crossed_threshold(origin, Point::new(17.9, 10.0)));
        assert!(crossed_threshold(origin, Point::new(18.0, 10.0)));
        assert!(crossed_threshold(origin, Point::new(2.0, 10.0)));
        assert!(crossed_threshold(origin, Point::new(14.8, 16.4)));
    }

    #[test]
    fn membership_comparison_ignores_preview_order_but_not_identity() {
        assert!(same_members(&[3, 1, 2], &[1, 2, 3]));
        assert!(!same_members(&[3, 1], &[1, 2, 3]));
        assert!(!same_members(&[3, 1, 4], &[1, 2, 3]));
        assert!(!same_members(&[1, 1], &[1, 1]));
        assert!(!same_members(&[0, 1], &[0, 1]));
    }

    fn gesture(dragging: bool) -> Gesture {
        Gesture {
            scope_id: 7,
            source_id: 10,
            original_ids: vec![10, 20, 30],
            ordered_ids: if dragging {
                vec![20, 30, 10]
            } else {
                vec![10, 20, 30]
            },
            origin: Point::ORIGIN,
            context_generation: 4,
            dragging,
        }
    }

    #[test]
    fn release_settles_without_cursor_coordinates_and_is_one_shot() {
        let mut state = State {
            gesture: Some(gesture(true)),
            ..State::default()
        };
        assert_eq!(
            state.settle_release(7, &[10, 20, 30], 4),
            Some(StripEvent::Commit {
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: vec![10, 20, 30],
                ordered_ids: vec![20, 30, 10],
            })
        );
        assert_eq!(state.settle_release(7, &[10, 20, 30], 4), None);
    }

    #[test]
    fn armed_left_release_is_owned_before_children_and_other_releases_are_not() {
        let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        let mut state = State {
            gesture: Some(gesture(true)),
            ..State::default()
        };
        assert!(matches!(
            state.settle_owned_release(&release, 7, &[10, 20, 30], 4),
            ReleaseSettlement::Owned(Some(StripEvent::Commit { .. }))
        ));
        assert_eq!(
            state.settle_owned_release(&release, 7, &[10, 20, 30], 4),
            ReleaseSettlement::Unowned
        );

        let right_release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right));
        let mut right = State {
            gesture: Some(gesture(true)),
            ..State::default()
        };
        assert_eq!(
            right.settle_owned_release(&right_release, 7, &[10, 20, 30], 4),
            ReleaseSettlement::Unowned
        );
        assert!(right.gesture.is_some());
    }

    #[test]
    fn release_cancels_stale_context_and_subthreshold_click_ends_without_commit() {
        let mut stale = State {
            gesture: Some(gesture(true)),
            ..State::default()
        };
        assert_eq!(
            stale.settle_release(7, &[10, 20, 30], 5),
            Some(StripEvent::Cancel {
                context_generation: 4,
            })
        );

        let mut click = State {
            gesture: Some(gesture(false)),
            ..State::default()
        };
        assert_eq!(
            click.settle_release(7, &[10, 20, 30], 4),
            Some(StripEvent::Ended {
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: vec![10, 20, 30],
            })
        );
        assert!(click.gesture.is_none());
    }

    #[test]
    fn ended_inactive_press_preserves_the_second_press_as_a_double_click() {
        let position = Point::new(250.0, 17.0);
        let ids = [10, 20, 30];
        let mut state = State::default();
        assert!(matches!(
            state.arm_press(position, 7, 10, &ids, 4),
            PressSettlement::Started(StripEvent::Started { source_id: 10, .. })
        ));
        assert!(matches!(
            state.settle_release(7, &ids, 4),
            Some(StripEvent::Ended { source_id: 10, .. })
        ));
        assert_eq!(
            state.arm_press(position, 7, 10, &ids, 4),
            PressSettlement::DoubleClick
        );
        assert!(state.gesture.is_none());
    }

    #[test]
    fn double_click_history_is_scoped_to_stable_id_project_and_generation() {
        let click = mouse::Click::new(Point::new(10.0, 10.0), mouse::Button::Left, None);
        let state = State {
            previous_click: Some(ScopedClick {
                click,
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
            }),
            ..State::default()
        };
        assert!(state.previous_click_for(7, 10, 4).is_some());
        assert!(state.previous_click_for(7, 20, 4).is_none());
        assert!(state.previous_click_for(8, 10, 4).is_none());
        assert!(state.previous_click_for(7, 10, 5).is_none());
    }

    #[test]
    fn drag_began_publishes_once_per_threshold_crossing() {
        let ids = [10, 20, 30];
        let origin = Point::new(100.0, 20.0);
        let mut state = State::default();
        assert!(matches!(
            state.arm_press(origin, 7, 10, &ids, 4),
            PressSettlement::Started(StripEvent::Started { .. })
        ));
        assert_eq!(state.begin_drag(Point::new(104.0, 20.0)), None);
        assert_eq!(
            state.begin_drag(Point::new(108.0, 20.0)),
            Some(StripEvent::DragBegan {
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
            })
        );
        assert_eq!(state.begin_drag(Point::new(180.0, 60.0)), None);
        assert_eq!(state.begin_drag(origin), None);
        assert!(matches!(
            state.settle_release(7, &ids, 4),
            Some(StripEvent::Commit { .. })
        ));

        assert_eq!(state.begin_drag(Point::new(180.0, 60.0)), None);
        let elsewhere = Point::new(400.0, 20.0);
        assert!(matches!(
            state.arm_press(elsewhere, 7, 30, &ids, 4),
            PressSettlement::Started(StripEvent::Started { .. })
        ));
        assert_eq!(
            state.begin_drag(Point::new(408.0, 20.0)),
            Some(StripEvent::DragBegan {
                scope_id: 7,
                source_id: 30,
                context_generation: 4,
            })
        );
    }

    fn stacked_rows(heights: [f32; 3]) -> layout::Node {
        let mut top = 0.0;
        let children = heights
            .iter()
            .map(|height| {
                let node =
                    layout::Node::new(Size::new(200.0, *height)).move_to(Point::new(0.0, top));
                top += height;
                node
            })
            .collect();
        layout::Node::with_children(Size::new(200.0, top), children)
    }

    #[test]
    fn vertical_target_index_counts_rows_past_their_centers_with_mixed_heights() {
        // Centers at y = 20, 50, 90 for heights 40 / 20 / 60.
        let node = stacked_rows([40.0, 20.0, 60.0]);
        let layout = Layout::new(&node);
        let index = |y| raw_target_index(Axis::Vertical, layout, Point::new(10.0, y));
        assert_eq!(index(0.0), 0);
        assert_eq!(index(19.9), 0);
        assert_eq!(index(20.0), 1);
        assert_eq!(index(49.9), 1);
        assert_eq!(index(50.0), 2);
        assert_eq!(index(89.9), 2);
        assert_eq!(index(90.0), 3);
        assert_eq!(index(500.0), 3);

        // The vertical axis ignores x; the horizontal axis reads only x
        // (every row shares center_x = 100).
        assert_eq!(
            raw_target_index(Axis::Vertical, layout, Point::new(500.0, 20.0)),
            1
        );
        assert_eq!(
            raw_target_index(Axis::Horizontal, layout, Point::new(500.0, 20.0)),
            3
        );
        assert_eq!(
            raw_target_index(Axis::Horizontal, layout, Point::new(99.9, 500.0)),
            0
        );
    }

    #[test]
    fn batched_reversal_restores_rendered_order_before_release() {
        let rendered = [10, 20, 30];
        let mut ordered = rendered.to_vec();
        ordered = next_preview_order(&rendered, 10, 3, &ordered)
            .unwrap()
            .unwrap();
        assert_eq!(ordered, [20, 30, 10]);

        // No redraw has occurred: moving back beside the source is `None`
        // relative to the rendered order, but must replace the prior preview.
        ordered = next_preview_order(&rendered, 10, 1, &ordered)
            .unwrap()
            .unwrap();
        assert_eq!(ordered, rendered);

        let mut gesture = gesture(true);
        gesture.ordered_ids = ordered;
        let mut state = State {
            gesture: Some(gesture),
            ..State::default()
        };
        assert_eq!(
            state.settle_release(7, &rendered, 4),
            Some(StripEvent::Commit {
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: rendered.to_vec(),
                ordered_ids: rendered.to_vec(),
            })
        );
    }
}
