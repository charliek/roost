//! Native pointer gesture adapter for stable-ID reordering strips. The
//! tab strip is one instantiation; the widget is axis- and
//! message-parametrized so a vertical strip can reuse the same gesture
//! state machine.

use std::time::{Duration, Instant};

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::{window, Element, Event, Length, Point, Rectangle, Size, Vector};
use roost_ui_model::keys::{HostId, ProjectKey, TabKey};
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
    Vertical,
}

/// `host` is the instance whose id-space this event's ids belong to: one
/// section per host, so two hosts listing a project `4` publish two
/// different gestures rather than one ambiguous id.
///
/// `scope_id` scopes a gesture to the surface it started on (the active
/// project id for the tab strip, the host's incarnation for a project
/// list) — it is only ever compared for equality, never run through
/// `same_members`' zero reject, which applies to the item-id list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StripEvent {
    Started {
        host: HostId,
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
    },
    DragBegan {
        host: HostId,
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
    },
    Preview {
        host: HostId,
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
        ordered_ids: Vec<i64>,
    },
    Commit {
        host: HostId,
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
        ordered_ids: Vec<i64>,
    },
    Ended {
        host: HostId,
        scope_id: i64,
        source_id: i64,
        context_generation: u64,
        original_ids: Vec<i64>,
    },
    Cancel {
        host: HostId,
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
    /// The strip's own model is the snapshot's BARE ids (positions,
    /// hit-testing, member equality) while the messages it publishes are
    /// host-qualified, so the constructors below take the instance those
    /// ids belong to — the one joint between the two.
    host: HostId,
    on_select: fn(HostId, i64) -> Message,
    on_rename: fn(HostId, i64) -> Message,
    on_event: fn(StripEvent) -> Message,
}

impl<'a> ReorderStrip<'a> {
    pub(crate) fn tabs(
        content: impl Into<Element<'a, Message>>,
        host: HostId,
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
            host,
            on_select: |host, id| Message::TabSelected(TabKey::new(host, id)),
            on_rename: |host, id| Message::BeginRenameTab(TabKey::new(host, id)),
            on_event: Message::TabStrip,
        }
    }

    /// The sidebar project list is one strip per host section, so the
    /// incarnation is the scope it keys gestures to (`HostId::LOCAL` is
    /// `0`, which is what the single-section window has always compared
    /// against). Iced keys widget state by tree position and sections
    /// come and go, so one strip's `previous_press` can be handed to
    /// another host's strip — `previous.scope_id != self.scope_id`
    /// below is the guard that drops it.
    pub(crate) fn projects(
        content: impl Into<Element<'a, Message>>,
        host: HostId,
        ids: Vec<i64>,
        context_generation: u64,
        enabled: bool,
    ) -> Self {
        Self {
            content: content.into(),
            axis: Axis::Vertical,
            scope_id: i64::from(host.raw()),
            ids,
            context_generation,
            enabled,
            host,
            on_select: |host, id| Message::ProjectSelected(ProjectKey::new(host, id)),
            on_rename: |host, id| Message::BeginRenameProject(ProjectKey::new(host, id)),
            on_event: Message::ProjectStrip,
        }
    }
}

impl ReorderStrip<'_> {
    fn scope(&self) -> StripScope {
        StripScope {
            host: self.host,
            scope_id: self.scope_id,
            context_generation: self.context_generation,
        }
    }
}

#[derive(Debug)]
struct Gesture {
    /// The instance this gesture's ids belong to. The tab strip's
    /// `scope_id` is a bare project id, which two instances can both
    /// own, so the scope alone does not separate them.
    host: HostId,
    scope_id: i64,
    source_id: i64,
    original_ids: Vec<i64>,
    ordered_ids: Vec<i64>,
    origin: Point,
    context_generation: u64,
    dragging: bool,
}

/// Wall-clock reach of a double-click, matching what Iced's own
/// [`mouse::Click`] allows.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(300);
/// How many rendered frames may separate the two presses when the wall
/// clock alone would refuse them. Iced charges its 300 ms from the moment
/// each press is *processed*, and with the 16 ms tick gone the app is
/// idle when the first press lands: it is processed at once and the frame
/// that press schedules — 200 ms and up under software rendering — is
/// spent inside the budget, so the second press reads as a fresh single
/// click and the rename never opens. Frames are the honest unit for that
/// case; a stalled renderer only ever gets a couple of them in edgewise
/// (the repaint that blew the budget was two of these, and a healthy
/// wgpu frame pair measures three inside 94 ms — well under the wall
/// window that already accepts it).
const DOUBLE_CLICK_FRAME_GRACE: u64 = 4;
/// The ceiling the frame grace may never exceed. An idle app renders
/// nothing between presses, so frames alone would call two deliberate
/// clicks seconds apart one gesture.
const DOUBLE_CLICK_STALL_CEILING: Duration = Duration::from_millis(1000);
const DOUBLE_CLICK_DISTANCE: f32 = 6.0;

#[derive(Clone, Copy, Debug)]
struct ScopedPress {
    position: Point,
    at: Instant,
    /// The strip's frame counter when this press was processed — the
    /// budget the wall clock cannot see.
    frame: u64,
    /// See [`Gesture::host`]: without it a press on local project `4`
    /// and one on host H's project `4` read as one double-click, and the
    /// rename editor would open on a row the first press never touched.
    host: HostId,
    scope_id: i64,
    source_id: i64,
    context_generation: u64,
}

fn press_continues_gesture(
    previous: &ScopedPress,
    position: Point,
    at: Instant,
    frame: u64,
) -> bool {
    if previous.position.distance(position) >= DOUBLE_CLICK_DISTANCE {
        return false;
    }
    let gap = at.saturating_duration_since(previous.at);
    let frame_gap = frame.saturating_sub(previous.frame);
    // The grace branch requires at least one rendered frame between the
    // presses: it exists for a renderer stalled mid-double-click, and a
    // zero-frame gap means nothing stalled — two slow clicks on an
    // already-selected row (no redraw between them) must stay two clicks.
    gap <= DOUBLE_CLICK_WINDOW
        || (frame_gap > 0
            && frame_gap <= DOUBLE_CLICK_FRAME_GRACE
            && gap <= DOUBLE_CLICK_STALL_CEILING)
}

#[derive(Debug, Default)]
struct State {
    gesture: Option<Gesture>,
    previous_press: Option<ScopedPress>,
    /// Rendered frames since the strip existed. Only differences matter.
    frames: u64,
}

/// A strip's identity for a gesture: whose ids, which surface, and
/// which generation of it. The three always travel together — a gesture
/// is only ever this strip's while all three still match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StripScope {
    host: HostId,
    scope_id: i64,
    context_generation: u64,
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
    fn previous_press_for(&self, scope: StripScope, source_id: i64) -> Option<ScopedPress> {
        self.previous_press.filter(|previous| {
            previous.host == scope.host
                && previous.scope_id == scope.scope_id
                && previous.source_id == source_id
                && previous.context_generation == scope.context_generation
        })
    }

    fn settle_release(&mut self, scope: StripScope, ids: &[i64]) -> Option<StripEvent> {
        let gesture = self.gesture.take()?;
        if !context_valid(&gesture, scope, ids) {
            // The gesture's own host, not this strip's: after a widget
            // state reflow they can differ, and the App matches a cancel
            // against the preview the gesture armed.
            Some(StripEvent::Cancel {
                host: gesture.host,
                context_generation: gesture.context_generation,
            })
        } else if gesture.dragging {
            Some(StripEvent::Commit {
                host: gesture.host,
                scope_id: gesture.scope_id,
                source_id: gesture.source_id,
                context_generation: gesture.context_generation,
                original_ids: gesture.original_ids,
                ordered_ids: gesture.ordered_ids,
            })
        } else {
            Some(StripEvent::Ended {
                host: gesture.host,
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
        at: Instant,
        scope: StripScope,
        source_id: i64,
        ids: &[i64],
    ) -> PressSettlement {
        let press = ScopedPress {
            position,
            at,
            frame: self.frames,
            host: scope.host,
            scope_id: scope.scope_id,
            source_id,
            context_generation: scope.context_generation,
        };
        let double = self
            .previous_press_for(scope, source_id)
            .is_some_and(|previous| press_continues_gesture(&previous, position, at, self.frames));
        // A third press must start over rather than rename again, which is
        // what Iced's own Double → Triple step bought.
        self.previous_press = (!double).then_some(press);
        if double {
            self.gesture = None;
            PressSettlement::DoubleClick
        } else {
            self.gesture = Some(Gesture {
                host: scope.host,
                scope_id: scope.scope_id,
                source_id,
                original_ids: ids.to_vec(),
                ordered_ids: ids.to_vec(),
                origin: position,
                context_generation: scope.context_generation,
                dragging: false,
            });
            PressSettlement::Started(StripEvent::Started {
                host: scope.host,
                scope_id: scope.scope_id,
                source_id,
                context_generation: scope.context_generation,
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
            host: gesture.host,
            scope_id: gesture.scope_id,
            source_id: gesture.source_id,
            context_generation: gesture.context_generation,
        })
    }

    fn settle_owned_release(
        &mut self,
        event: &Event,
        scope: StripScope,
        ids: &[i64],
    ) -> ReleaseSettlement {
        if !matches!(
            event,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
        ) || self.gesture.is_none()
        {
            return ReleaseSettlement::Unowned;
        }
        ReleaseSettlement::Owned(self.settle_release(scope, ids))
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

fn context_valid(gesture: &Gesture, scope: StripScope, ids: &[i64]) -> bool {
    gesture.host == scope.host
        && gesture.context_generation == scope.context_generation
        && gesture.scope_id == scope.scope_id
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
            self.scope(),
            &self.ids,
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
        // Counted before every early return: the frame budget only means
        // anything if a stalled renderer still moves it.
        if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            state.frames = state.frames.wrapping_add(1);
        }
        if shell.is_event_captured() {
            if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
                state.previous_press = None;
            }
            return;
        }
        if !self.enabled {
            state.previous_press = None;
            if let Some(gesture) = state.gesture.take() {
                shell.publish((self.on_event)(StripEvent::Cancel {
                    host: gesture.host,
                    context_generation: gesture.context_generation,
                }));
            }
            return;
        }

        if state.previous_press.is_some_and(|previous| {
            previous.host != self.host
                || previous.scope_id != self.scope_id
                || previous.context_generation != self.context_generation
                || !self.ids.contains(&previous.source_id)
        }) {
            state.previous_press = None;
        }

        if let Some(gesture) = state
            .gesture
            .take_if(|gesture| !context_valid(gesture, self.scope(), &self.ids))
        {
            state.previous_press = None;
            shell.publish((self.on_event)(StripEvent::Cancel {
                host: gesture.host,
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
                    frames = state.frames,
                    ids = ?self.ids,
                    child_bounds = ?layout.children().map(|child| child.bounds()).collect::<Vec<_>>(),
                    "Iced reorder strip armed pointer press"
                );
                shell.publish((self.on_select)(self.host, source_id));
                match state.arm_press(position, Instant::now(), self.scope(), source_id, &self.ids)
                {
                    PressSettlement::Started(event) => {
                        shell.publish((self.on_event)(event));
                    }
                    PressSettlement::DoubleClick => {
                        shell.publish((self.on_rename)(self.host, source_id));
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
                            host: gesture.host,
                            scope_id: gesture.scope_id,
                            source_id: gesture.source_id,
                            context_generation: gesture.context_generation,
                            original_ids: gesture.original_ids.clone(),
                            ordered_ids,
                        }));
                    }
                    Ok(None) => {}
                    Err(_) => {
                        let host = gesture.host;
                        let context_generation = gesture.context_generation;
                        state.gesture = None;
                        shell.publish((self.on_event)(StripEvent::Cancel {
                            host,
                            context_generation,
                        }));
                    }
                }
                shell.capture_event();
            }
            Event::Window(window::Event::Unfocused) => {
                state.previous_press = None;
                if let Some(gesture) = state.gesture.take() {
                    shell.publish((self.on_event)(StripEvent::Cancel {
                        host: gesture.host,
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

    fn scope(host: HostId, scope_id: i64, context_generation: u64) -> StripScope {
        StripScope {
            host,
            scope_id,
            context_generation,
        }
    }

    fn gesture(dragging: bool) -> Gesture {
        gesture_on(HostId::LOCAL, dragging)
    }

    fn gesture_on(host: HostId, dragging: bool) -> Gesture {
        Gesture {
            host,
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
            state.settle_release(scope(HostId::LOCAL, 7, 4), &[10, 20, 30]),
            Some(StripEvent::Commit {
                host: HostId::LOCAL,
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: vec![10, 20, 30],
                ordered_ids: vec![20, 30, 10],
            })
        );
        assert_eq!(
            state.settle_release(scope(HostId::LOCAL, 7, 4), &[10, 20, 30]),
            None
        );
    }

    #[test]
    fn armed_left_release_is_owned_before_children_and_other_releases_are_not() {
        let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        let mut state = State {
            gesture: Some(gesture(true)),
            ..State::default()
        };
        assert!(matches!(
            state.settle_owned_release(&release, scope(HostId::LOCAL, 7, 4), &[10, 20, 30]),
            ReleaseSettlement::Owned(Some(StripEvent::Commit { .. }))
        ));
        assert_eq!(
            state.settle_owned_release(&release, scope(HostId::LOCAL, 7, 4), &[10, 20, 30]),
            ReleaseSettlement::Unowned
        );

        let right_release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right));
        let mut right = State {
            gesture: Some(gesture(true)),
            ..State::default()
        };
        assert_eq!(
            right.settle_owned_release(&right_release, scope(HostId::LOCAL, 7, 4), &[10, 20, 30]),
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
            stale.settle_release(scope(HostId::LOCAL, 7, 5), &[10, 20, 30]),
            Some(StripEvent::Cancel {
                host: HostId::LOCAL,
                context_generation: 4,
            })
        );

        let mut click = State {
            gesture: Some(gesture(false)),
            ..State::default()
        };
        assert_eq!(
            click.settle_release(scope(HostId::LOCAL, 7, 4), &[10, 20, 30]),
            Some(StripEvent::Ended {
                host: HostId::LOCAL,
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: vec![10, 20, 30],
            })
        );
        assert!(click.gesture.is_none());
    }

    /// Two hosts each own a project `4`, so an event that named only the
    /// bare id would be ambiguous the moment a second section exists.
    #[test]
    fn every_published_event_names_the_instance_its_ids_belong_to() {
        let host = HostId::new(7);
        let ids = [10, 20, 30];
        let origin = Point::new(100.0, 20.0);
        let mut state = State::default();

        let PressSettlement::Started(started) =
            state.arm_press(origin, Instant::now(), scope(host, 4, 4), 10, &ids)
        else {
            panic!("a first press starts a gesture");
        };
        assert!(matches!(started, StripEvent::Started { host: h, .. } if h == host));
        assert!(matches!(
            state.begin_drag(Point::new(140.0, 20.0)),
            Some(StripEvent::DragBegan { host: h, .. }) if h == host
        ));
        assert!(matches!(
            state.settle_release(scope(host, 4, 4), &ids),
            Some(StripEvent::Commit { host: h, .. }) if h == host
        ));

        let mut ended = State {
            gesture: Some(gesture_on(host, false)),
            ..State::default()
        };
        assert!(matches!(
            ended.settle_release(scope(host, 7, 4), &ids),
            Some(StripEvent::Ended { host: h, .. }) if h == host
        ));

        let mut cancelled = State {
            gesture: Some(gesture_on(host, true)),
            ..State::default()
        };
        assert!(matches!(
            cancelled.settle_release(scope(host, 7, 5), &ids),
            Some(StripEvent::Cancel { host: h, .. }) if h == host
        ));
    }

    /// The scope is the incarnation, not a constant: iced keys widget
    /// state by tree position, so a section appearing or disappearing can
    /// hand one strip's `previous_press` to another host's strip, and the
    /// scope comparison is what drops it.
    #[test]
    fn a_project_strips_scope_is_its_own_incarnation() {
        let strip = |host| {
            ReorderStrip::projects(iced::widget::Space::new(), host, vec![10, 20], 4, true).scope_id
        };
        assert_eq!(strip(HostId::LOCAL), 0);
        assert_eq!(strip(HostId::new(3)), 3);
        assert_ne!(strip(HostId::new(3)), strip(HostId::new(4)));
    }

    #[test]
    fn ended_inactive_press_preserves_the_second_press_as_a_double_click() {
        let position = Point::new(250.0, 17.0);
        let ids = [10, 20, 30];
        let now = Instant::now();
        let mut state = State::default();
        assert!(matches!(
            state.arm_press(position, now, scope(HostId::LOCAL, 7, 4), 10, &ids),
            PressSettlement::Started(StripEvent::Started { source_id: 10, .. })
        ));
        assert!(matches!(
            state.settle_release(scope(HostId::LOCAL, 7, 4), &ids),
            Some(StripEvent::Ended { source_id: 10, .. })
        ));
        assert_eq!(
            state.arm_press(
                position,
                now + Duration::from_millis(100),
                scope(HostId::LOCAL, 7, 4),
                10,
                &ids
            ),
            PressSettlement::DoubleClick
        );
        assert!(state.gesture.is_none());
        // The third press starts over: a double-click must not rename on
        // every further press of the same sequence.
        assert!(matches!(
            state.arm_press(
                position,
                now + Duration::from_millis(200),
                scope(HostId::LOCAL, 7, 4),
                10,
                &ids
            ),
            PressSettlement::Started(StripEvent::Started { source_id: 10, .. })
        ));
    }

    /// The regression the frame grace exists for: a software-rendered
    /// frame between the presses spends the whole 300 ms wall budget, and
    /// the gesture must survive it — but only while the renderer is what
    /// ate the time.
    #[test]
    fn a_stalled_frame_between_presses_still_reads_as_one_double_click() {
        let position = Point::new(100.0, 52.0);
        let now = Instant::now();
        let previous = ScopedPress {
            host: HostId::LOCAL,
            position,
            at: now,
            frame: 4,
            scope_id: 7,
            source_id: 10,
            context_generation: 4,
        };

        assert!(press_continues_gesture(
            &previous,
            position,
            now + Duration::from_millis(511),
            6,
        ));
        assert!(
            !press_continues_gesture(&previous, position, now + Duration::from_millis(511), 40),
            "a busy renderer that kept up is a fresh click, not a stalled gesture"
        );
        assert!(
            !press_continues_gesture(&previous, position, now + Duration::from_millis(4_000), 6,),
            "an idle app renders nothing, so frames alone must not fuse deliberate clicks"
        );
        assert!(
            !press_continues_gesture(
                &previous,
                Point::new(140.0, 52.0),
                now + Duration::from_millis(100),
                4,
            ),
            "a press on another spot is another gesture"
        );
        assert!(
            !press_continues_gesture(&previous, position, now + Duration::from_millis(511), 4),
            "no rendered frame between the presses means nothing stalled — \
             two slow clicks on an already-selected row stay two clicks"
        );
    }

    /// The tab strip's scope is a bare project id, and a local project
    /// and a host's can both be `7`. Without the host on the press, a
    /// click on local tab 10 followed — inside the double-click window,
    /// after a palette jump that moved the selection without burning the
    /// generation — by a click on host H's tab 10 would open the rename
    /// editor on a row the first click never touched.
    #[test]
    fn a_press_pair_that_spans_two_instances_is_not_a_double_click() {
        let position = Point::new(250.0, 17.0);
        let ids = [10, 20, 30];
        let now = Instant::now();
        let mut state = State::default();

        assert!(matches!(
            state.arm_press(position, now, scope(HostId::LOCAL, 7, 4), 10, &ids),
            PressSettlement::Started(StripEvent::Started { .. })
        ));
        assert!(matches!(
            state.settle_release(scope(HostId::LOCAL, 7, 4), &ids),
            Some(StripEvent::Ended { .. })
        ));
        assert!(
            matches!(
                state.arm_press(
                    position,
                    now + Duration::from_millis(100),
                    scope(HostId::new(3), 7, 4),
                    10,
                    &ids,
                ),
                PressSettlement::Started(StripEvent::Started { .. })
            ),
            "another instance's project 7 is another surface"
        );

        // The same pair on one instance still renames, so the guard is
        // the host and nothing else.
        let mut same = State::default();
        assert!(matches!(
            same.arm_press(position, now, scope(HostId::new(3), 7, 4), 10, &ids),
            PressSettlement::Started(StripEvent::Started { .. })
        ));
        assert!(matches!(
            same.settle_release(scope(HostId::new(3), 7, 4), &ids),
            Some(StripEvent::Ended { .. })
        ));
        assert_eq!(
            same.arm_press(
                position,
                now + Duration::from_millis(100),
                scope(HostId::new(3), 7, 4),
                10,
                &ids,
            ),
            PressSettlement::DoubleClick
        );
    }

    /// A gesture keeps publishing under the instance it armed on, even
    /// if the strip holding its widget state is re-rendered as another
    /// section's — the App matches a cancel against the preview the
    /// gesture armed, not against whatever is on screen now.
    #[test]
    fn a_cancel_names_the_gesture_s_own_instance_not_the_strips() {
        let host = HostId::new(3);
        let ids = [10, 20, 30];
        let mut state = State::default();
        assert!(matches!(
            state.arm_press(Point::ORIGIN, Instant::now(), scope(host, 7, 4), 10, &ids),
            PressSettlement::Started(StripEvent::Started { .. })
        ));
        assert!(matches!(
            state.settle_release(scope(HostId::LOCAL, 7, 4), &ids),
            Some(StripEvent::Cancel { host: h, .. }) if h == host
        ));
    }

    #[test]
    fn double_click_history_is_scoped_to_stable_id_project_and_generation() {
        let state = State {
            previous_press: Some(ScopedPress {
                host: HostId::LOCAL,
                position: Point::new(10.0, 10.0),
                at: Instant::now(),
                frame: 0,
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
            }),
            ..State::default()
        };
        assert!(state
            .previous_press_for(scope(HostId::LOCAL, 7, 4), 10)
            .is_some());
        assert!(state
            .previous_press_for(scope(HostId::LOCAL, 7, 4), 20)
            .is_none());
        assert!(state
            .previous_press_for(scope(HostId::LOCAL, 8, 4), 10)
            .is_none());
        assert!(state
            .previous_press_for(scope(HostId::LOCAL, 7, 5), 10)
            .is_none());
        assert!(
            state
                .previous_press_for(scope(HostId::new(3), 7, 4), 10)
                .is_none(),
            "the tab strip's scope is a bare project id, which another \
             instance can own too — the host is what separates them"
        );
    }

    #[test]
    fn drag_began_publishes_once_per_threshold_crossing() {
        let ids = [10, 20, 30];
        let origin = Point::new(100.0, 20.0);
        let mut state = State::default();
        assert!(matches!(
            state.arm_press(origin, Instant::now(), scope(HostId::LOCAL, 7, 4), 10, &ids),
            PressSettlement::Started(StripEvent::Started { .. })
        ));
        assert_eq!(state.begin_drag(Point::new(104.0, 20.0)), None);
        assert_eq!(
            state.begin_drag(Point::new(108.0, 20.0)),
            Some(StripEvent::DragBegan {
                host: HostId::LOCAL,
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
            })
        );
        assert_eq!(state.begin_drag(Point::new(180.0, 60.0)), None);
        assert_eq!(state.begin_drag(origin), None);
        assert!(matches!(
            state.settle_release(scope(HostId::LOCAL, 7, 4), &ids),
            Some(StripEvent::Commit { .. })
        ));

        assert_eq!(state.begin_drag(Point::new(180.0, 60.0)), None);
        let elsewhere = Point::new(400.0, 20.0);
        assert!(matches!(
            state.arm_press(
                elsewhere,
                Instant::now(),
                scope(HostId::LOCAL, 7, 4),
                30,
                &ids
            ),
            PressSettlement::Started(StripEvent::Started { .. })
        ));
        assert_eq!(
            state.begin_drag(Point::new(408.0, 20.0)),
            Some(StripEvent::DragBegan {
                host: HostId::LOCAL,
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
            state.settle_release(scope(HostId::LOCAL, 7, 4), &rendered),
            Some(StripEvent::Commit {
                host: HostId::LOCAL,
                scope_id: 7,
                source_id: 10,
                context_generation: 4,
                original_ids: rendered.to_vec(),
                ordered_ids: rendered.to_vec(),
            })
        );
    }
}
