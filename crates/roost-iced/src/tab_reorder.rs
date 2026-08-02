//! Native pointer gesture adapter for stable-ID tab reordering.

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::{window, Element, Event, Length, Point, Rectangle, Size, Vector};
use roost_ui_model::reorder::{moved_ids, ReorderError};

use crate::Message;

const DRAG_THRESHOLD: f32 = 8.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TabStripEvent {
    Started {
        project_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
    },
    Preview {
        project_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
        ordered_ids: Vec<i64>,
    },
    Commit {
        project_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
        ordered_ids: Vec<i64>,
    },
    Cancel {
        context_generation: u64,
    },
}

pub(crate) struct TabStrip<'a> {
    content: Element<'a, Message>,
    project_id: i64,
    ids: Vec<i64>,
    context_generation: u64,
    enabled: bool,
}

impl<'a> TabStrip<'a> {
    pub(crate) fn new(
        content: impl Into<Element<'a, Message>>,
        project_id: i64,
        ids: Vec<i64>,
        context_generation: u64,
        enabled: bool,
    ) -> Self {
        Self {
            content: content.into(),
            project_id,
            ids,
            context_generation,
            enabled,
        }
    }
}

#[derive(Debug)]
struct Gesture {
    project_id: i64,
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
    project_id: i64,
    source_id: i64,
    context_generation: u64,
}

#[derive(Debug, Default)]
struct State {
    gesture: Option<Gesture>,
    previous_click: Option<ScopedClick>,
}

impl State {
    fn previous_click_for(
        &self,
        project_id: i64,
        source_id: i64,
        context_generation: u64,
    ) -> Option<mouse::Click> {
        self.previous_click
            .filter(|previous| {
                previous.project_id == project_id
                    && previous.source_id == source_id
                    && previous.context_generation == context_generation
            })
            .map(|previous| previous.click)
    }

    fn settle_release(
        &mut self,
        project_id: i64,
        ids: &[i64],
        context_generation: u64,
    ) -> Option<TabStripEvent> {
        let gesture = self.gesture.take()?;
        if !gesture.dragging {
            return None;
        }
        if context_valid(&gesture, project_id, ids, context_generation) {
            Some(TabStripEvent::Commit {
                project_id: gesture.project_id,
                source_id: gesture.source_id,
                context_generation: gesture.context_generation,
                original_ids: gesture.original_ids,
                ordered_ids: gesture.ordered_ids,
            })
        } else {
            Some(TabStripEvent::Cancel {
                context_generation: gesture.context_generation,
            })
        }
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

fn raw_target_index(layout: Layout<'_>, x: f32) -> usize {
    layout
        .children()
        .take_while(|child| x >= child.bounds().center_x())
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

fn context_valid(gesture: &Gesture, project_id: i64, ids: &[i64], context_generation: u64) -> bool {
    gesture.context_generation == context_generation
        && gesture.project_id == project_id
        && same_members(ids, &gesture.original_ids)
}

impl Widget<Message, iced::Theme, iced::Renderer> for TabStrip<'_> {
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
                shell.publish(Message::TabStrip(TabStripEvent::Cancel {
                    context_generation: gesture.context_generation,
                }));
            }
            return;
        }

        if state.previous_click.is_some_and(|previous| {
            previous.project_id != self.project_id
                || previous.context_generation != self.context_generation
                || !self.ids.contains(&previous.source_id)
        }) {
            state.previous_click = None;
        }

        if let Some(gesture) = state.gesture.take_if(|gesture| {
            !context_valid(gesture, self.project_id, &self.ids, self.context_generation)
        }) {
            state.previous_click = None;
            shell.publish(Message::TabStrip(TabStripEvent::Cancel {
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
                let click = mouse::Click::new(
                    position,
                    mouse::Button::Left,
                    state.previous_click_for(self.project_id, source_id, self.context_generation),
                );
                state.previous_click = Some(ScopedClick {
                    click,
                    project_id: self.project_id,
                    source_id,
                    context_generation: self.context_generation,
                });
                shell.publish(Message::TabSelected(source_id));
                if click.kind() == mouse::click::Kind::Double {
                    state.gesture = None;
                    shell.publish(Message::BeginRenameTab(source_id));
                } else {
                    state.gesture = Some(Gesture {
                        project_id: self.project_id,
                        source_id,
                        original_ids: self.ids.clone(),
                        ordered_ids: self.ids.clone(),
                        origin: position,
                        context_generation: self.context_generation,
                        dragging: false,
                    });
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let Some(gesture) = &mut state.gesture else {
                    return;
                };
                let Some(position) = cursor.position() else {
                    return;
                };
                if !gesture.dragging && crossed_threshold(gesture.origin, position) {
                    gesture.dragging = true;
                    shell.publish(Message::TabStrip(TabStripEvent::Started {
                        project_id: gesture.project_id,
                        source_id: gesture.source_id,
                        context_generation: gesture.context_generation,
                        original_ids: gesture.original_ids.clone(),
                    }));
                }
                if !gesture.dragging {
                    return;
                }
                let raw_target = raw_target_index(layout, position.x);
                match next_preview_order(
                    &self.ids,
                    gesture.source_id,
                    raw_target,
                    &gesture.ordered_ids,
                ) {
                    Ok(Some(ordered_ids)) => {
                        gesture.ordered_ids.clone_from(&ordered_ids);
                        shell.publish(Message::TabStrip(TabStripEvent::Preview {
                            project_id: gesture.project_id,
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
                        shell.publish(Message::TabStrip(TabStripEvent::Cancel {
                            context_generation,
                        }));
                    }
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let owned = state.gesture.is_some();
                if let Some(event) =
                    state.settle_release(self.project_id, &self.ids, self.context_generation)
                {
                    shell.publish(Message::TabStrip(event));
                }
                if owned {
                    shell.capture_event();
                }
            }
            Event::Window(window::Event::Unfocused) => {
                state.previous_click = None;
                if let Some(gesture) = state.gesture.take() {
                    shell.publish(Message::TabStrip(TabStripEvent::Cancel {
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

impl<'a> From<TabStrip<'a>> for Element<'a, Message> {
    fn from(strip: TabStrip<'a>) -> Self {
        Element::new(strip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            project_id: 7,
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
            Some(TabStripEvent::Commit {
                project_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: vec![10, 20, 30],
                ordered_ids: vec![20, 30, 10],
            })
        );
        assert_eq!(state.settle_release(7, &[10, 20, 30], 4), None);
    }

    #[test]
    fn release_cancels_stale_context_and_subthreshold_click_does_not_commit() {
        let mut stale = State {
            gesture: Some(gesture(true)),
            ..State::default()
        };
        assert_eq!(
            stale.settle_release(7, &[10, 20, 30], 5),
            Some(TabStripEvent::Cancel {
                context_generation: 4,
            })
        );

        let mut click = State {
            gesture: Some(gesture(false)),
            ..State::default()
        };
        assert_eq!(click.settle_release(7, &[10, 20, 30], 4), None);
        assert!(click.gesture.is_none());
    }

    #[test]
    fn double_click_history_is_scoped_to_stable_id_project_and_generation() {
        let click = mouse::Click::new(Point::new(10.0, 10.0), mouse::Button::Left, None);
        let state = State {
            previous_click: Some(ScopedClick {
                click,
                project_id: 7,
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
            Some(TabStripEvent::Commit {
                project_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: rendered.to_vec(),
                ordered_ids: rendered.to_vec(),
            })
        );
    }
}
