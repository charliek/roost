//! The sidebar's project list / Agents panel split (plan 080 D8).
//!
//! One widget owns the project list and the panel, lays them out itself,
//! in whole pixels, from one `fraction`, and draws the divider between
//! them. The divider is the grip — dragging it is this widget's own
//! gesture — so it is drawn exactly where the panel starts and no row
//! loses a press to it. Both children are always present: hiding the panel
//! gives it and the divider zero height, and emptying it the divider,
//! rather than a different tree, so the project list keeps its scroll
//! position across both.

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::widget::container;
use iced::{window, Element, Event, Length, Point, Rectangle, Size, Vector};
use roost_engine::{AGENTS_PANEL_MAX_FRACTION, AGENTS_PANEL_MIN_FRACTION};

use crate::chrome::{self, ChromePalette};
use crate::resize_grip::{self, Gesture, GripState, Motion};
use crate::Message;

const MIN_FRACTION: f32 = AGENTS_PANEL_MIN_FRACTION as f32;
const MAX_FRACTION: f32 = AGENTS_PANEL_MAX_FRACTION as f32;

/// The project list keeps at least one host band and two rows.
const PROJECTS_FLOOR: f32 = chrome::BAND_HEIGHT + 2.0 * chrome::ROW_HEIGHT;

/// How tall the panel is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PanelHeight {
    /// `show-sidebar-agents = false`: no divider and no panel.
    Hidden,
    /// No agents: no divider, and the panel is exactly this tall.
    Content(f32),
    /// The split: `fraction` of the height below the divider, never less
    /// than `floor` while the project list keeps [`PROJECTS_FLOOR`].
    Split { fraction: f32, floor: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Heights {
    projects: f32,
    divider: f32,
    panel: f32,
}

/// The three parts' heights, top to bottom, for a grip `total` tall.
///
/// The project list and divider are whole pixels, so the panel starts on
/// one; a fractional `total` leaves its fraction at the panel's bottom
/// edge. A hidden panel takes none of the fraction: a sliver of height
/// would pass `draw`'s zero-height filter.
fn heights(total: f32, panel: PanelHeight) -> Heights {
    let total = total.max(0.0);
    let (divider, panel) = match panel {
        PanelHeight::Hidden => {
            return Heights {
                projects: total,
                divider: 0.0,
                panel: 0.0,
            };
        }
        PanelHeight::Content(height) => (0.0, height.min(total)),
        PanelHeight::Split { fraction, floor } => {
            let divider = chrome::AGENTS_SPLIT_DIVIDER_HEIGHT.min(total);
            (divider, shown_panel(fraction, total - divider, floor))
        }
    };
    let projects = (total - divider - panel).floor().max(0.0);
    Heights {
        projects,
        divider,
        panel: total - divider - projects,
    }
}

/// The panel's height at `fraction` of the `below` the divider leaves.
/// When both floors cannot fit, the project list's wins and the panel
/// keeps what is left — down to its header, which stays on screen so the
/// sort control does.
fn shown_panel(fraction: f32, below: f32, floor: f32) -> f32 {
    let most = (below - PROJECTS_FLOOR)
        .max(chrome::AGENT_PANEL_HEADER_HEIGHT)
        .min(below);
    (fraction * below).round().max(floor).min(most)
}

/// Where the parts sit in a grip `width` wide, in child order: the project
/// list, the panel, then the divider between them.
fn part_bounds(width: f32, heights: Heights) -> [Rectangle; 3] {
    let at = |top: f32, height: f32| Rectangle::new(Point::new(0.0, top), Size::new(width, height));
    [
        at(0.0, heights.projects),
        at(heights.projects + heights.divider, heights.panel),
        at(heights.projects, heights.divider),
    ]
}

/// The divider's place among the grip's layout children.
const DIVIDER_CHILD: usize = 2;

/// A drag of the divider as far as the app has seen it (plan 080 D8): the
/// fraction it last published, which the panel shows ahead of the
/// workspace until the drag ends and writes it through.
///
/// Every path that can strand the grip mid-drag ends it — the release,
/// anything that rebuilds the root (a toast, an overlay, a collapse) and
/// anything that takes the divider away (the toggle, the last agent going)
/// — or the panel would keep showing a fraction no relaunch restores.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct AgentsSplitDrag {
    live: Option<f32>,
    /// Which drags are still the grip's. The grip records it at a press and
    /// stamps it on what it publishes; every end the app forces moves it
    /// on, so a drag it ended can neither resume in a widget that outlived
    /// the end (an op mid-drag, a seam drag over a divider drag whose
    /// release was lost) nor come back through a message already queued
    /// behind the end. A press that an end overtakes within one batch is
    /// let go too, and has to be made again.
    epoch: u64,
}

impl AgentsSplitDrag {
    pub(crate) fn epoch(self) -> u64 {
        self.epoch
    }

    /// A move's fraction. Kept only while the divider is `drawn`: a move
    /// drained in the same batch as whatever took the divider away would
    /// start a drag no release is coming to end.
    pub(crate) fn dragged(&mut self, epoch: u64, fraction: f32, drawn: bool) {
        if drawn && epoch == self.epoch {
            self.live = Some(fraction);
        }
    }

    /// The fraction the panel shows: the drag's, else `stored`.
    pub(crate) fn fraction(self, stored: f32) -> f32 {
        self.live.unwrap_or(stored)
    }

    /// The grip's own end — its release, or the window losing focus — with
    /// the share to commit: the last one a move published, as it is, or
    /// none for a press that never moved. Each was clamped to what its own
    /// move drew, and a floor or a resize since clamps the stored share's
    /// drawing as it did the live one's, so the commit leaves the divider
    /// where it was.
    pub(crate) fn ended(&mut self, epoch: u64) -> Option<f32> {
        if epoch != self.epoch {
            return None;
        }
        self.live.take()
    }

    /// An end the app forces, with the fraction to commit when one was in
    /// flight. It ends the grip's drag too, even one that has published
    /// nothing yet.
    pub(crate) fn interrupt(&mut self) -> Option<f32> {
        self.epoch = self.epoch.wrapping_add(1);
        self.live.take()
    }

    /// Ends the drag once nothing draws its divider; a drag whose divider
    /// is still drawn is the grip's to finish.
    pub(crate) fn settle(&mut self, drawn: bool) -> Option<f32> {
        if drawn {
            None
        } else {
            self.interrupt()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    /// How far below the divider's top it was grabbed.
    grab: f32,
    epoch: u64,
    /// The share this drag last published.
    published: Option<f32>,
}

type State = GripState<Drag>;

#[derive(Clone, Copy, Debug, PartialEq)]
enum SplitEvent {
    Dragged { fraction: f32 },
    Ended,
}

/// The divider as laid out, and the split it was laid out at.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Divider {
    bounds: Rectangle,
    /// The grip's bottom edge, which is the panel's.
    bottom: f32,
    below: f32,
    fraction: f32,
    floor: f32,
    epoch: u64,
}

impl Divider {
    /// The divider in the grip's `layout`, or `None` while there is none to
    /// drag: the panel hidden or empty.
    fn of(layout: Layout<'_>, panel: PanelHeight, epoch: u64) -> Option<Self> {
        let PanelHeight::Split { fraction, floor } = panel else {
            return None;
        };
        let bounds = layout.children().nth(DIVIDER_CHILD)?.bounds();
        let grip = layout.bounds();
        let below = grip.height - bounds.height;
        (below > 0.0).then_some(Self {
            bounds,
            bottom: grip.y + grip.height,
            below,
            fraction,
            floor,
            epoch,
        })
    }

    fn holds(&self, drag: Drag) -> bool {
        drag.epoch == self.epoch
    }

    /// Lit, and showing the resize cursor.
    fn engaged(&self, state: &State, cursor: mouse::Cursor) -> bool {
        state.drag.is_some_and(|drag| self.holds(drag)) || cursor.is_over(self.bounds)
    }
}

/// The panel height a drag to `y` draws: the one that puts the divider's
/// top `drag.grab` above the pointer in the split as it is laid out now,
/// as far as the fraction's bounds and the floors let the split draw it.
/// The seam stays under the pointer, and the fraction a release commits —
/// which the workspace clamps to the same bounds — draws the same split.
fn dragged_panel(divider: &Divider, drag: Drag, y: f32) -> f32 {
    let wanted = divider.bottom - divider.bounds.height - (y - drag.grab);
    shown_panel(
        (wanted / divider.below).clamp(MIN_FRACTION, MAX_FRACTION),
        divider.below,
        divider.floor,
    )
}

impl Gesture for Divider {
    type Drag = Drag;
    type Event = SplitEvent;

    fn press(&self, at: Point) -> Option<Drag> {
        self.bounds.contains(at).then_some(Drag {
            grab: at.y - self.bounds.y,
            epoch: self.epoch,
            published: None,
        })
    }

    fn moved(&self, drag: Drag, at: Point) -> Motion<SplitEvent> {
        // Published when it differs from the fraction the split was laid
        // out at — not the height it drew: a floor holds the drawn height
        // while the fraction under it goes stale (a sort that raises the
        // panel's, a window that shrinks under the list's) — or from the
        // last one this drag published, which a batch of moves drained
        // together has not been laid out at yet: out and back within one
        // frame would leave the panel showing the way out. Anything else
        // draws the same split and publishes nothing.
        // Clamped again after the pixel rounding, which can land a hair
        // outside the bounds: the workspace would round it back on commit
        // and move the stored share. Inside the bounds it draws the same.
        let fraction =
            (dragged_panel(self, drag, at.y) / self.below).clamp(MIN_FRACTION, MAX_FRACTION);
        let news = fraction != self.fraction || drag.published.is_some_and(|told| told != fraction);
        Motion::Continue(news.then_some(SplitEvent::Dragged { fraction }))
    }

    fn ended(&self) -> SplitEvent {
        SplitEvent::Ended
    }
}

/// [`resize_grip::owns_event`] over the divider, once a drag the app has
/// ended since its press is let go, and then noting on a drag what its
/// move published.
fn owns_event(
    state: &mut State,
    event: &Event,
    cursor: mouse::Cursor,
    divider: Option<&Divider>,
) -> resize_grip::Ownership<SplitEvent> {
    if let (Some(drag), Some(divider)) = (state.drag, divider) {
        if !divider.holds(drag) {
            state.drag = None;
        }
    }
    let ownership = resize_grip::owns_event(state, event, cursor, divider);
    if let (resize_grip::Ownership::Own(Some(SplitEvent::Dragged { fraction })), Some(drag)) =
        (ownership, state.drag.as_mut())
    {
        drag.published = Some(fraction);
    }
    ownership
}

fn split_message(event: SplitEvent, epoch: u64) -> Message {
    match event {
        SplitEvent::Dragged { fraction } => Message::AgentsSplitDragged { fraction, epoch },
        SplitEvent::Ended => Message::AgentsSplitEnded { epoch },
    }
}

pub(crate) struct AgentsSplitGrip<'a> {
    /// The project list, the panel.
    children: [Element<'a, Message>; 2],
    panel: PanelHeight,
    /// [`AgentsSplitDrag::epoch`].
    epoch: u64,
    chrome: &'a ChromePalette,
    /// Whether the divider was last drawn lit. iced redraws only on request,
    /// so a hover that changes it has to ask.
    lit: Option<bool>,
}

impl<'a> AgentsSplitGrip<'a> {
    pub(crate) fn new(
        chrome: &'a ChromePalette,
        projects: impl Into<Element<'a, Message>>,
        panel: impl Into<Element<'a, Message>>,
        height: PanelHeight,
        epoch: u64,
    ) -> Self {
        Self {
            children: [projects.into(), panel.into()],
            panel: height,
            epoch,
            chrome,
            lit: None,
        }
    }

    /// The divider's own fill with the handle nub centered on it, lit while
    /// it is hovered or dragged.
    fn draw_divider(
        &self,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        bounds: Rectangle,
        lit: bool,
    ) {
        container::draw_background(renderer, &chrome::divider(self.chrome)(theme), bounds);
        let nub = chrome::AGENTS_SPLIT_NUB;
        let at = Point::new(
            bounds.center_x() - nub.width / 2.0,
            bounds.center_y() - nub.height / 2.0,
        );
        container::draw_background(
            renderer,
            &chrome::split_handle(self.chrome, lit)(theme),
            Rectangle::new(at, nub),
        );
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for AgentsSplitGrip<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.max();
        let [projects, panel, divider] = part_bounds(size.width, heights(size.height, self.panel));
        let mut nodes: Vec<layout::Node> = [projects, panel]
            .into_iter()
            .zip(self.children.iter_mut().zip(&mut tree.children))
            .map(|(bounds, (child, tree))| {
                let exact = bounds.size();
                child
                    .as_widget_mut()
                    .layout(tree, renderer, &layout::Limits::new(exact, exact))
                    .move_to(bounds.position())
            })
            .collect();
        nodes.push(layout::Node::new(divider.size()).move_to(divider.position()));
        layout::Node::with_children(size, nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, tree), layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child
                    .as_widget_mut()
                    .operate(tree, layout, renderer, operation);
            }
        });
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
        let divider = Divider::of(layout, self.panel, self.epoch);
        let ownership = owns_event(state, event, cursor, divider.as_ref());
        let lit = divider.is_some_and(|divider| divider.engaged(state, cursor));
        if let Event::Window(window::Event::RedrawRequested(_)) = event {
            self.lit = Some(lit);
        } else if self.lit.is_some_and(|drawn| drawn != lit) {
            shell.request_redraw();
        }
        let epoch = self.epoch;
        if !resize_grip::claim(ownership, shell, |event| split_message(event, epoch)) {
            return;
        }

        for ((child, tree), layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                tree, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
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
        if Divider::of(layout, self.panel, self.epoch)
            .is_some_and(|divider| divider.engaged(tree.state.downcast_ref::<State>(), cursor))
        {
            return mouse::Interaction::ResizingVertically;
        }
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, tree), layout)| {
                child
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
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
        // A zero-height child is a hidden one; its content may not know it
        // was given no room.
        for ((child, tree), layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter(|(_, layout)| layout.bounds().height > 0.0)
        {
            child
                .as_widget()
                .draw(tree, renderer, theme, style, layout, cursor, viewport);
        }
        if let Some(divider) = Divider::of(layout, self.panel, self.epoch) {
            if divider.bounds.intersects(viewport) {
                let lit = divider.engaged(tree.state.downcast_ref::<State>(), cursor);
                self.draw_divider(renderer, theme, divider.bounds, lit);
            }
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<AgentsSplitGrip<'a>> for Element<'a, Message> {
    fn from(grip: AgentsSplitGrip<'a>) -> Self {
        Element::new(grip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_FRACTION: f32 = roost_engine::AGENTS_PANEL_DEFAULT_FRACTION as f32;
    const DIVIDER: f32 = chrome::AGENTS_SPLIT_DIVIDER_HEIGHT;
    const HEADER: f32 = chrome::AGENT_PANEL_HEADER_HEIGHT;
    const FLOOR: f32 = 98.0;

    fn split(fraction: f32) -> PanelHeight {
        PanelHeight::Split {
            fraction,
            floor: FLOOR,
        }
    }

    fn sum(heights: Heights) -> f32 {
        heights.projects + heights.divider + heights.panel
    }

    #[test]
    fn the_default_fraction_is_the_panels_share_of_the_height_below_the_divider() {
        let total = 624.0;
        let laid = heights(total, split(DEFAULT_FRACTION));
        assert_eq!(laid.divider, DIVIDER);
        assert_eq!(laid.panel, (DEFAULT_FRACTION * (total - DIVIDER)).round());
        assert_eq!(laid.panel, 278.0);
        assert_eq!(laid.projects, 340.0);
        assert_eq!(sum(laid), total);
    }

    #[test]
    fn the_split_lands_on_whole_pixels_whatever_the_height() {
        for total in [500.0, 623.5, 777.25, 1000.75] {
            for fraction in [0.2, DEFAULT_FRACTION, 0.6] {
                let laid = heights(total, split(fraction));
                assert_eq!(
                    laid.projects.fract(),
                    0.0,
                    "{total} at {fraction}: {laid:?}"
                );
                assert_eq!(laid.divider.fract(), 0.0, "{total} at {fraction}: {laid:?}");
                assert_eq!(sum(laid), total, "{total} at {fraction}: {laid:?}");
            }
        }
    }

    #[test]
    fn a_small_fraction_still_shows_the_panel_floor() {
        let laid = heights(600.0, split(0.01));
        assert_eq!(laid.panel, FLOOR);
        assert_eq!(laid.projects, 600.0 - DIVIDER - FLOOR);
    }

    #[test]
    fn a_large_fraction_stops_at_the_project_list_floor() {
        let laid = heights(600.0, split(0.99));
        assert_eq!(laid.projects, PROJECTS_FLOOR);
        assert_eq!(laid.panel, 600.0 - DIVIDER - PROJECTS_FLOOR);
    }

    #[test]
    fn the_project_list_floor_wins_when_both_cannot_fit() {
        let total = DIVIDER + PROJECTS_FLOOR + FLOOR - 40.0;
        for fraction in [0.01, DEFAULT_FRACTION, 0.99] {
            let laid = heights(total, split(fraction));
            assert_eq!(laid.projects, PROJECTS_FLOOR, "at {fraction}");
            assert_eq!(laid.panel, FLOOR - 40.0, "at {fraction}");
        }
    }

    #[test]
    fn the_panel_keeps_its_header_when_even_the_rest_is_less() {
        let total = DIVIDER + PROJECTS_FLOOR;
        let laid = heights(total, split(DEFAULT_FRACTION));
        assert_eq!(laid.panel, HEADER);
        assert_eq!(laid.projects, PROJECTS_FLOOR - HEADER);

        let tiny = heights(20.0, split(DEFAULT_FRACTION));
        assert_eq!(sum(tiny), 20.0);
        assert!(tiny.projects >= 0.0 && tiny.panel >= 0.0, "{tiny:?}");
    }

    #[test]
    fn a_hidden_panel_and_its_divider_take_no_height() {
        for total in [600.0, 623.5] {
            assert_eq!(
                heights(total, PanelHeight::Hidden),
                Heights {
                    projects: total,
                    divider: 0.0,
                    panel: 0.0,
                }
            );
        }
    }

    #[test]
    fn an_empty_panel_is_sized_to_its_content_with_no_divider() {
        let content = HEADER + chrome::AGENT_PANEL_EMPTY_HEIGHT;
        assert_eq!(
            heights(600.0, PanelHeight::Content(content)),
            Heights {
                projects: 600.0 - content,
                divider: 0.0,
                panel: content,
            }
        );
    }

    #[test]
    fn a_non_finite_fraction_falls_back_to_the_floor() {
        let laid = heights(600.0, split(f32::NAN));
        assert_eq!(laid.panel, FLOOR);
        assert_eq!(sum(laid), 600.0);
    }

    // ── The divider's drag (plan 080 D8) ────────────────────────────

    type Ownership = resize_grip::Ownership<SplitEvent>;

    /// The grip the sidebar lays out under a 220px sidebar: its 1px
    /// hairline is the sidebar's own.
    const WIDTH: f32 = 219.0;
    const TOTAL: f32 = 624.0;
    /// Where the divider starts at the default, and the panel it leaves —
    /// the first test pins both.
    const DIVIDER_TOP: f32 = 340.0;
    const PANEL: f32 = 278.0;
    const BELOW: f32 = TOTAL - DIVIDER;
    /// Mid-divider.
    const GRAB: f32 = DIVIDER_TOP + 3.0;

    /// The layout `AgentsSplitGrip::layout` builds, with plain nodes for
    /// the children.
    fn grip_in(total: f32, panel: PanelHeight) -> layout::Node {
        let parts = part_bounds(WIDTH, heights(total, panel));
        layout::Node::with_children(
            Size::new(WIDTH, total),
            parts
                .iter()
                .map(|bounds| layout::Node::new(bounds.size()).move_to(bounds.position()))
                .collect(),
        )
    }

    fn at_default() -> PanelHeight {
        split(DEFAULT_FRACTION)
    }

    /// The split a live drag lays out once its move has drawn `panel`.
    fn drawn_at(panel: f32) -> PanelHeight {
        split(panel / BELOW)
    }

    fn press() -> Event {
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
    }

    fn release() -> Event {
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
    }

    fn moved(y: f32) -> Event {
        Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(100.0, y),
        })
    }

    fn unfocused() -> Event {
        Event::Window(window::Event::Unfocused)
    }

    /// Grabbed mid-divider, at the first epoch.
    fn dragging() -> State {
        State {
            drag: Some(Drag {
                grab: GRAB - DIVIDER_TOP,
                epoch: 0,
                published: None,
            }),
            ..State::default()
        }
    }

    /// Drives the real ownership path over a grip `total` tall, drawn at
    /// `epoch`: the divider is found in the layout, never handed in.
    fn owns_drawn(
        total: f32,
        state: &mut State,
        event: &Event,
        panel: PanelHeight,
        cursor_y: Option<f32>,
        epoch: u64,
    ) -> Ownership {
        let node = grip_in(total, panel);
        let cursor = cursor_y.map_or(mouse::Cursor::Unavailable, |y| {
            mouse::Cursor::Available(Point::new(100.0, y))
        });
        let divider = Divider::of(Layout::new(&node), panel, epoch);
        owns_event(state, event, cursor, divider.as_ref())
    }

    fn owns_in(
        total: f32,
        state: &mut State,
        event: &Event,
        panel: PanelHeight,
        cursor_y: Option<f32>,
    ) -> Ownership {
        owns_drawn(total, state, event, panel, cursor_y, 0)
    }

    fn owns(
        state: &mut State,
        event: &Event,
        panel: PanelHeight,
        cursor_y: Option<f32>,
    ) -> Ownership {
        owns_in(TOTAL, state, event, panel, cursor_y)
    }

    fn dragged_to(state: &mut State, total: f32, y: f32) -> f32 {
        match owns_in(total, state, &moved(y), at_default(), Some(y)) {
            Ownership::Own(Some(SplitEvent::Dragged { fraction })) => fraction,
            other => panic!("a move to {y} published nothing: {other:?}"),
        }
    }

    #[test]
    fn the_divider_is_the_grab_zone_between_the_list_and_the_panel() {
        let node = grip_in(TOTAL, at_default());
        let layout = Layout::new(&node);
        let divider = Divider::of(layout, at_default(), 0).expect("a split has a divider");
        assert_eq!(
            divider.bounds,
            Rectangle::new(Point::new(0.0, DIVIDER_TOP), Size::new(WIDTH, DIVIDER))
        );
        let mut parts = layout.children().map(|part| part.bounds());
        let (projects, panel) = (parts.next().unwrap(), parts.next().unwrap());
        assert_eq!(projects.y + projects.height, DIVIDER_TOP);
        assert_eq!(panel.y, DIVIDER_TOP + DIVIDER);
        assert_eq!(panel.height, PANEL);

        for (y, owned) in [
            (DIVIDER_TOP - 0.1, false),
            (DIVIDER_TOP, true),
            (DIVIDER_TOP + DIVIDER - 0.1, true),
            (DIVIDER_TOP + DIVIDER, false),
        ] {
            let mut state = State::default();
            let expected = if owned {
                Ownership::Own(None)
            } else {
                Ownership::Delegate
            };
            assert_eq!(
                owns(&mut state, &press(), at_default(), Some(y)),
                expected,
                "a press at y {y}"
            );
        }
    }

    #[test]
    fn a_divider_press_is_owned_and_a_press_on_a_row_is_delegated() {
        let mut state = State::default();
        assert_eq!(
            owns_drawn(TOTAL, &mut state, &press(), at_default(), Some(GRAB), 7),
            Ownership::Own(None)
        );
        assert_eq!(
            state.drag,
            Some(Drag {
                grab: GRAB - DIVIDER_TOP,
                epoch: 7,
                published: None,
            }),
            "the press records where on the divider it landed, and the epoch it was drawn at"
        );

        // A project row above, an agent row below.
        for y in [20.0, 500.0] {
            let mut elsewhere = State::default();
            assert_eq!(
                owns(&mut elsewhere, &press(), at_default(), Some(y)),
                Ownership::Delegate,
                "a press at y {y}"
            );
            assert_eq!(elsewhere.drag, None);
        }
    }

    #[test]
    fn motion_and_release_are_owned_only_while_a_drag_is_live() {
        let mut idle = State::default();
        assert_eq!(
            owns(&mut idle, &moved(GRAB), at_default(), Some(GRAB)),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut idle, &release(), at_default(), Some(GRAB)),
            Ownership::Delegate
        );

        let mut live = dragging();
        // 22px up grows the panel by 22.
        assert_eq!(
            owns(
                &mut live,
                &moved(GRAB - 22.0),
                at_default(),
                Some(GRAB - 22.0)
            ),
            Ownership::Own(Some(SplitEvent::Dragged {
                fraction: (PANEL + 22.0) / BELOW,
            }))
        );
        // The pointer left the window: the drag still owns the motion.
        assert_eq!(
            owns(&mut live, &moved(GRAB + 10.0), at_default(), None),
            Ownership::Own(Some(SplitEvent::Dragged {
                fraction: (PANEL - 10.0) / BELOW,
            }))
        );
        assert_eq!(
            owns(&mut live, &release(), at_default(), None),
            Ownership::Own(Some(SplitEvent::Ended))
        );
        assert_eq!(live.drag, None);
        assert_eq!(
            owns(&mut live, &release(), at_default(), None),
            Ownership::Delegate
        );
    }

    #[test]
    fn a_move_that_keeps_the_drawn_split_owns_without_publishing() {
        let live_at = drawn_at(PANEL);
        assert_eq!(heights(TOTAL, live_at), heights(TOTAL, at_default()));
        let mut live = dragging();
        assert_eq!(
            owns(&mut live, &moved(GRAB), live_at, Some(GRAB)),
            Ownership::Own(None)
        );
        assert_eq!(
            owns(&mut live, &moved(GRAB + 0.4), live_at, Some(GRAB + 0.4)),
            Ownership::Own(None),
            "sub-pixel travel draws the same split"
        );

        // Already at the project list's floor, further up moves nothing.
        let at_most = drawn_at(BELOW - PROJECTS_FLOOR);
        assert_eq!(heights(TOTAL, at_most).projects, PROJECTS_FLOOR);
        let mut held = dragging();
        let up = PROJECTS_FLOOR + 3.0 - 50.0;
        assert_eq!(
            owns(&mut held, &moved(up), at_most, Some(up)),
            Ownership::Own(None)
        );
    }

    /// A stored share is seldom one the split can draw exactly, so a drag's
    /// first move publishes the one it does draw, even in place: the share
    /// a release commits is the one on screen.
    #[test]
    fn the_first_move_publishes_the_drawn_share_in_place_of_the_stored_one() {
        assert_ne!(DEFAULT_FRACTION, PANEL / BELOW);
        let mut live = dragging();
        assert_eq!(
            owns(&mut live, &moved(GRAB), at_default(), Some(GRAB)),
            Ownership::Own(Some(SplitEvent::Dragged {
                fraction: PANEL / BELOW,
            }))
        );
    }

    #[test]
    fn the_floors_and_the_bounds_clamp_the_published_fraction_to_what_is_drawn() {
        let drawn_after_commit = |total: f32, fraction: f32| {
            heights(total, split(fraction.clamp(MIN_FRACTION, MAX_FRACTION)))
        };

        // Up past the project list's floor: a band and two rows stay.
        let up = dragged_to(&mut dragging(), TOTAL, -500.0);
        assert_eq!(up, (BELOW - PROJECTS_FLOOR) / BELOW);
        assert_eq!(heights(TOTAL, split(up)).projects, PROJECTS_FLOOR);
        // Down past the panel's: one whole row stays.
        let down = dragged_to(&mut dragging(), TOTAL, 2000.0);
        assert_eq!(down, FLOOR / BELOW);
        assert_eq!(heights(TOTAL, split(down)).panel, FLOOR);
        for fraction in [up, down] {
            assert_eq!(
                drawn_after_commit(TOTAL, fraction),
                heights(TOTAL, split(fraction))
            );
        }

        // A taller grip reaches the fraction's own bounds first.
        let tall = 1000.0;
        let below = tall - DIVIDER;
        // Its rounded heights land just outside them (845/994, 149/994);
        // the published share is the bound itself, which draws the same.
        let up = dragged_to(&mut dragging(), tall, -500.0);
        assert_eq!(up, MAX_FRACTION);
        assert_eq!(
            heights(tall, split(up)).panel,
            (MAX_FRACTION * below).round()
        );
        let down = dragged_to(&mut dragging(), tall, 3000.0);
        assert_eq!(down, MIN_FRACTION);
        assert_eq!(
            heights(tall, split(down)).panel,
            (MIN_FRACTION * below).round()
        );
        for fraction in [up, down] {
            assert_eq!(
                drawn_after_commit(tall, fraction),
                heights(tall, split(fraction))
            );
        }
    }

    #[test]
    fn unfocus_commits_a_live_drag_and_still_reaches_the_rows() {
        let mut live = dragging();
        assert_eq!(
            owns(&mut live, &unfocused(), at_default(), None),
            Ownership::PublishAndDelegate(SplitEvent::Ended)
        );
        assert_eq!(live.drag, None);
        assert_eq!(
            owns(&mut live, &unfocused(), at_default(), None),
            Ownership::Delegate
        );
    }

    #[test]
    fn a_second_press_during_a_drag_is_owned_without_restarting_it() {
        for cursor_y in [Some(GRAB), Some(500.0), None] {
            let mut live = dragging();
            assert_eq!(
                owns(&mut live, &press(), at_default(), cursor_y),
                Ownership::Own(None),
                "a press at {cursor_y:?} during a drag must never reach a row"
            );
            assert_eq!(live.drag, dragging().drag, "the original anchor survives");
        }
    }

    #[test]
    fn a_press_is_hit_tested_where_the_last_move_landed_not_at_the_batch_cursor() {
        // A frame-behind UI drains the press with the move that followed it,
        // so the batch reports where the pointer went after the press.
        let mut state = State::default();
        assert_eq!(
            owns(&mut state, &moved(GRAB), at_default(), Some(500.0)),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut state, &press(), at_default(), Some(500.0)),
            Ownership::Own(None)
        );
        assert_eq!(state.drag.map(|drag| drag.grab), Some(GRAB - DIVIDER_TOP));

        let mut away = State::default();
        assert_eq!(
            owns(&mut away, &moved(500.0), at_default(), Some(GRAB)),
            Ownership::Delegate
        );
        assert_eq!(
            owns(&mut away, &press(), at_default(), Some(GRAB)),
            Ownership::Delegate,
            "a batch cursor that drifted onto the divider rescues nothing"
        );
        assert_eq!(away.drag, None);
    }

    #[test]
    fn a_pointer_that_left_the_window_drops_the_anchor() {
        let mut state = State::default();
        owns(&mut state, &moved(GRAB), at_default(), Some(GRAB));
        let left = Event::Mouse(mouse::Event::CursorLeft);
        assert_eq!(
            owns(&mut state, &left, at_default(), None),
            Ownership::Delegate
        );
        assert_eq!(state.last_cursor, None);
        assert_eq!(
            owns(&mut state, &press(), at_default(), Some(500.0)),
            Ownership::Delegate
        );
        assert_eq!(state.drag, None);
    }

    #[test]
    fn with_no_divider_the_grip_owns_nothing_and_drops_a_live_drag() {
        let empty = PanelHeight::Content(HEADER + chrome::AGENT_PANEL_EMPTY_HEIGHT);
        for panel in [PanelHeight::Hidden, empty] {
            let node = grip_in(TOTAL, panel);
            assert_eq!(Divider::of(Layout::new(&node), panel, 0), None, "{panel:?}");
            for y in [DIVIDER_TOP, GRAB, TOTAL - HEADER] {
                let mut state = State::default();
                assert_eq!(
                    owns(&mut state, &press(), panel, Some(y)),
                    Ownership::Delegate,
                    "{panel:?}: a press at y {y}"
                );
            }

            let mut live = dragging();
            assert_eq!(
                owns(&mut live, &moved(GRAB - 22.0), panel, Some(GRAB - 22.0)),
                Ownership::Delegate,
                "{panel:?}"
            );
            assert_eq!(live.drag, None, "{panel:?}: the drag goes with the divider");
            assert_eq!(
                owns(&mut live, &release(), panel, Some(GRAB)),
                Ownership::Delegate,
                "{panel:?}"
            );
            let mut live = dragging();
            assert_eq!(
                owns(&mut live, &unfocused(), panel, None),
                Ownership::Delegate,
                "{panel:?}"
            );
        }
    }

    #[test]
    fn the_divider_is_engaged_while_hovered_or_held() {
        let node = grip_in(TOTAL, at_default());
        let divider = Divider::of(Layout::new(&node), at_default(), 0).unwrap();
        let over = mouse::Cursor::Available(Point::new(100.0, GRAB));
        let off = mouse::Cursor::Available(Point::new(100.0, 500.0));
        let idle = State::default();
        assert!(divider.engaged(&idle, over));
        assert!(!divider.engaged(&idle, off));
        assert!(!divider.engaged(&idle, mouse::Cursor::Unavailable));
        assert!(divider.engaged(&dragging(), off), "held off the divider");
        assert!(divider.engaged(&dragging(), mouse::Cursor::Unavailable));

        let ended = Divider::of(Layout::new(&node), at_default(), 1).unwrap();
        assert!(
            !ended.engaged(&dragging(), off),
            "a drag the app has ended lights nothing"
        );
    }

    /// The grip sits inside the sidebar the seam grip wraps, and its
    /// divider runs to within the seam's reach. The seam grip sees every
    /// event first, so where both could take a press the seam's wins and
    /// the divider never hears of it.
    #[test]
    fn a_press_on_the_sidebar_seam_is_the_outer_grips_even_over_the_divider() {
        let sidebar_width = WIDTH + chrome::DIVIDER_WIDTH;
        let window = Size::new(800.0, TOTAL);
        let node = grip_in(TOTAL, at_default());
        let divider = Divider::of(Layout::new(&node), at_default(), 0).unwrap();

        let corner = Point::new(WIDTH - 1.0, GRAB);
        assert!(
            divider.press(corner).is_some(),
            "the divider alone would take it"
        );
        assert!(crate::sidebar_resize::keeps_press(
            sidebar_width,
            window,
            corner
        ));
        let over_the_panel = Point::new(WIDTH - 1.0, 500.0);
        assert!(crate::sidebar_resize::keeps_press(
            sidebar_width,
            window,
            over_the_panel
        ));

        let middle = Point::new(100.0, GRAB);
        assert!(!crate::sidebar_resize::keeps_press(
            sidebar_width,
            window,
            middle
        ));
        assert!(divider.press(middle).is_some());
    }

    // ── The grip and the app together ───────────────────────────────

    /// The grip and the app's half of its drag, wired as `update` wires
    /// them: the grip is laid out at the fraction the app shows and drawn
    /// at the app's epoch, and what it publishes reaches the app only when
    /// [`Wired::deliver`] hands it over — iced runs a whole batch of events
    /// through the tree before it drains their messages, so other messages
    /// can land in between.
    struct Wired {
        total: f32,
        floor: f32,
        /// The workspace's share.
        stored: f32,
        app: AgentsSplitDrag,
        grip: State,
    }

    impl Wired {
        fn new(total: f32, floor: f32) -> Self {
            Self {
                total,
                floor,
                stored: DEFAULT_FRACTION,
                app: AgentsSplitDrag::default(),
                grip: State::default(),
            }
        }

        fn panel(&self) -> PanelHeight {
            PanelHeight::Split {
                fraction: self.app.fraction(self.stored),
                floor: self.floor,
            }
        }

        fn laid(&self) -> Heights {
            heights(self.total, self.panel())
        }

        /// `event`, the pointer at `y`, through the grip as last drawn:
        /// what the grip takes, and the message it publishes.
        fn grip(&mut self, event: &Event, y: f32) -> (Ownership, Option<Message>) {
            let epoch = self.app.epoch();
            let panel = self.panel();
            let ownership = owns_drawn(self.total, &mut self.grip, event, panel, Some(y), epoch);
            let published = match ownership {
                Ownership::Own(publish) => publish,
                Ownership::PublishAndDelegate(event) => Some(event),
                Ownership::Delegate => None,
            };
            (
                ownership,
                published.map(|event| split_message(event, epoch)),
            )
        }

        /// What `update` does with a message the grip published.
        fn deliver(&mut self, message: &Message) {
            match *message {
                Message::AgentsSplitDragged { fraction, epoch } => {
                    self.app.dragged(epoch, fraction, true);
                }
                Message::AgentsSplitEnded { epoch } => {
                    if let Some(fraction) = self.app.ended(epoch) {
                        self.commit(fraction);
                    }
                }
                _ => unreachable!("the grip publishes only its own messages"),
            }
        }

        /// An event, with its message delivered straight away.
        fn step(&mut self, event: &Event, y: f32) -> Ownership {
            let (ownership, message) = self.grip(event, y);
            if let Some(message) = message {
                self.deliver(&message);
            }
            ownership
        }

        /// Moves to `y` and presses there.
        fn press_at(&mut self, y: f32) {
            self.step(&moved(y), y);
            assert_eq!(
                self.step(&press(), y),
                Ownership::Own(None),
                "a press at {y}"
            );
        }

        /// The workspace's setter.
        fn commit(&mut self, fraction: f32) {
            self.stored = fraction.clamp(MIN_FRACTION, MAX_FRACTION);
        }

        /// `App::commit_agents_split_drag`, which every end the app forces
        /// goes through.
        fn interrupt(&mut self) {
            if let Some(fraction) = self.app.interrupt() {
                self.commit(fraction);
            }
        }

        /// `sidebar.set_agents_fraction`'s handler.
        fn set_fraction(&mut self, fraction: f32) {
            self.interrupt();
            self.commit(fraction);
        }

        fn shown(&self) -> f32 {
            self.app.fraction(self.stored)
        }
    }

    /// `sidebar.set_agents_fraction` mid-drag: the op's share stands.
    /// Neither the grip, whose widget still holds the press, nor a move it
    /// published that is drained after the op, takes the drag back up.
    #[test]
    fn a_share_set_mid_drag_survives_the_rest_of_the_drag() {
        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        wired.step(&moved(GRAB - 22.0), GRAB - 22.0);
        assert_eq!(wired.shown(), (PANEL + 22.0) / BELOW);

        let (_, queued) = wired.grip(&moved(GRAB - 40.0), GRAB - 40.0);
        wired.set_fraction(0.6);
        wired.deliver(&queued.expect("the move publishes"));
        assert_eq!(wired.shown(), 0.6, "a move queued behind the op");

        for y in [GRAB - 60.0, GRAB + 30.0] {
            assert_eq!(
                wired.step(&moved(y), y),
                Ownership::Delegate,
                "a move to {y} after the op"
            );
        }
        assert_eq!(wired.grip.drag, None, "the grip let the drag go");
        assert_eq!(wired.step(&release(), GRAB + 30.0), Ownership::Delegate);
        assert_eq!((wired.stored, wired.shown()), (0.6, 0.6));

        // The next press starts a drag of its own.
        let top = wired.laid().projects;
        wired.press_at(top + 3.0);
        wired.step(&moved(top - 7.0), top - 7.0);
        wired.step(&release(), top - 7.0);
        let at_op = heights(TOTAL, split(0.6)).panel;
        assert_eq!(wired.laid().panel, at_op + 10.0);
        assert_eq!(wired.stored, (at_op + 10.0) / BELOW);
    }

    /// A press the seam grip takes during a divider drag — after the
    /// divider's release was lost, say — leaves the divider's widget
    /// holding its drag: the seam grip captures that press and its release,
    /// so the divider hears neither. The seam grip's drag ends the
    /// divider's, so the moves after it, button up, resize nothing.
    #[test]
    fn a_seam_drag_over_a_divider_drag_ends_the_divider_drag() {
        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        wired.step(&moved(GRAB - 22.0), GRAB - 22.0);
        let dragged = wired.shown();
        assert_eq!(dragged, (PANEL + 22.0) / BELOW);

        let seam = Point::new(WIDTH - 1.0, GRAB - 22.0);
        assert!(
            crate::sidebar_resize::keeps_press(
                WIDTH + chrome::DIVIDER_WIDTH,
                Size::new(800.0, TOTAL),
                seam
            ),
            "the second press is the seam grip's"
        );
        // `App::sidebar_resize_dragged` / `sidebar_resize_ended`.
        wired.interrupt();
        assert_eq!(
            wired.stored, dragged,
            "the divider drag's last share is kept"
        );

        for y in [GRAB - 60.0, GRAB + 40.0] {
            assert_eq!(
                wired.step(&moved(y), y),
                Ownership::Delegate,
                "a move to {y} after the seam drag"
            );
        }
        assert_eq!(wired.grip.drag, None);
        assert_eq!(wired.shown(), dragged);
    }

    /// An end stamped before the app's own is the old drag's, and moves
    /// nothing a later drag holds.
    #[test]
    fn an_end_from_before_the_apps_own_is_ignored() {
        let mut drag = AgentsSplitDrag::default();
        let old = drag.epoch();
        drag.dragged(old, 0.3, true);
        assert_eq!(drag.interrupt(), Some(0.3));
        let new = drag.epoch();
        assert_ne!(old, new);
        drag.dragged(new, 0.4, true);
        assert_eq!(drag.ended(old), None);
        assert_eq!(drag.fraction(DEFAULT_FRACTION), 0.4);
        assert_eq!(drag.ended(new), Some(0.4));
    }

    /// The grab offset holds across a change in the grip's height — a
    /// window resize mid-drag: each move places the divider from the split
    /// as it is laid out now, not from the height it had at the press.
    #[test]
    fn a_drag_keeps_its_grab_offset_across_a_window_resize() {
        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        wired.step(&moved(321.0), 321.0);
        assert_eq!(wired.laid().panel, 300.0);
        assert_eq!(wired.laid().projects, 318.0, "the divider's top, 3px up");

        wired.total = 724.0;
        wired.step(&moved(320.0), 320.0);
        assert_eq!(wired.laid().projects, 317.0, "still 3px above the pointer");
        assert_eq!(wired.laid().panel, 401.0);
        assert_eq!(wired.shown(), 401.0 / 718.0);
    }

    /// A floor that changes mid-drag, or a window that shrinks under the
    /// project list's, holds the drawn panel off the share the drag last
    /// published. The release commits that share as it is, and the drawing
    /// clamps the stored share as it did the live one: the divider stays
    /// where it was.
    #[test]
    fn a_release_after_a_floor_change_commits_the_dragged_share_in_place() {
        // `panel_height`'s floors (app/agents_panel.rs).
        let priority = HEADER + chrome::AGENT_PANEL_LIST_PADDING_Y + chrome::AGENT_PANEL_ROW_HEIGHT;
        let grouped = priority + chrome::AGENT_PANEL_GROUP_HEIGHT;
        assert_eq!((priority, grouped), (77.0, 101.0));
        let below = 300.0 - DIVIDER;
        let at = |panel: f32| 300.0 - DIVIDER - panel + 3.0;

        // Priority to grouped: the panel is held up at grouped's floor.
        let mut wired = Wired::new(300.0, priority);
        wired.press_at(wired.laid().projects + 3.0);
        wired.step(&moved(at(88.0)), at(88.0));
        assert_eq!(wired.shown(), 88.0 / below);
        wired.floor = grouped;
        let held = wired.laid();
        assert_eq!(held.panel, 101.0);
        wired.step(&release(), at(88.0));
        assert_eq!(wired.stored, 88.0 / below, "the share the drag published");
        assert_eq!(wired.laid(), held, "the release moved the divider");

        // Grouped to priority, the pointer asking for less than either
        // floor: the move published grouped's floor, and that stays.
        let mut wired = Wired::new(300.0, grouped);
        wired.press_at(wired.laid().projects + 3.0);
        wired.step(&moved(at(50.0)), at(50.0));
        assert_eq!(wired.shown(), 101.0 / below);
        wired.floor = priority;
        let held = wired.laid();
        assert_eq!(held.panel, 101.0);
        wired.step(&release(), at(50.0));
        assert_eq!(wired.stored, 101.0 / below);
        assert_eq!(wired.laid(), held, "the release moved the divider");

        // A window that shrinks under the project list's floor.
        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        let at_520 = TOTAL - DIVIDER - 520.0 + 3.0;
        wired.step(&moved(at_520), at_520);
        wired.total = 400.0;
        let held = wired.laid();
        assert_eq!(held.panel, 400.0 - DIVIDER - PROJECTS_FLOOR);
        wired.step(&release(), at_520);
        assert_eq!(wired.stored, 520.0 / BELOW);
        assert_eq!(wired.laid(), held, "the release moved the divider");
    }

    /// iced runs a whole batch of events through the tree before it drains
    /// their messages, so a move is measured against a split not yet laid
    /// out at the move before it. Out to 310px and back to 300 in one batch
    /// shows 300, and the release commits 300.
    #[test]
    fn a_batch_that_goes_out_and_back_lands_where_it_ended() {
        let at = |panel: f32| TOTAL - DIVIDER - panel + 3.0;
        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        wired.step(&moved(at(300.0)), at(300.0));
        assert_eq!(wired.laid().panel, 300.0);

        let out = wired.grip(&moved(at(310.0)), at(310.0)).1;
        let back = wired.grip(&moved(at(300.0)), at(300.0)).1;
        let end = wired.grip(&release(), at(300.0)).1;
        for message in [out, back].iter().flatten() {
            wired.deliver(message);
        }
        assert_eq!(
            wired.shown(),
            300.0 / BELOW,
            "the panel shows where it ended"
        );
        wired.deliver(&end.expect("the release publishes"));
        assert_eq!(wired.stored, 300.0 / BELOW);
    }

    /// A press that has not moved yet, then `sidebar.set_agents_fraction`,
    /// then the release: nothing was dragged, so the op's share stands,
    /// whether the release reaches the grip after the op or was queued
    /// behind it.
    #[test]
    fn a_release_with_nothing_dragged_commits_nothing_over_an_op() {
        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        wired.set_fraction(0.6);
        assert_eq!(wired.step(&release(), GRAB), Ownership::Delegate);
        assert_eq!((wired.stored, wired.shown()), (0.6, 0.6));

        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        let end = wired.grip(&release(), GRAB).1;
        wired.set_fraction(0.6);
        wired.deliver(&end.expect("the release publishes"));
        assert_eq!((wired.stored, wired.shown()), (0.6, 0.6));
    }

    /// An end the app forces after a press, drained before the press's
    /// first move — a toast expiring, the seam grip moving — lets the press
    /// go: the move is from before the end and is dropped, so nothing is
    /// left live for a grip the end rebuilt to strand. The next press is a
    /// drag of its own.
    #[test]
    fn an_end_forced_before_a_presss_first_move_lets_the_press_go() {
        let mut wired = Wired::new(TOTAL, FLOOR);
        wired.press_at(GRAB);
        let first = wired.grip(&moved(GRAB - 22.0), GRAB - 22.0).1;
        wired.interrupt();
        wired.deliver(&first.expect("the move publishes"));
        assert_eq!(wired.shown(), DEFAULT_FRACTION, "nothing live");
        assert_eq!(
            wired.step(&moved(GRAB - 30.0), GRAB - 30.0),
            Ownership::Delegate
        );
        assert_eq!(wired.grip.drag, None);

        wired.press_at(GRAB);
        wired.step(&moved(GRAB - 30.0), GRAB - 30.0);
        wired.step(&release(), GRAB - 30.0);
        assert_eq!(wired.stored, (PANEL + 30.0) / BELOW);
    }
}
