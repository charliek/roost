//! The sidebar's project list / Agents panel split (plan 080 D8).
//!
//! One widget owns the three children — the project list, the divider and
//! the panel — and lays them out itself, in whole pixels, from one
//! `fraction`, so the divider is drawn exactly where the panel starts. The
//! three are always present: hiding the panel gives it and the divider
//! zero height, and emptying it the divider, rather than a different tree,
//! so the project list keeps its scroll position across both.

use iced::advanced::layout;
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{mouse, Clipboard, Layout, Shell, Widget};
use iced::widget::{container, Space};
use iced::{Element, Event, Fill, Length, Point, Rectangle, Size, Vector};

use crate::chrome::{self, ChromePalette};
use crate::Message;

/// The panel's share of the split until a drag sets one.
pub(crate) const DEFAULT_FRACTION: f32 = 0.45;

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

/// The three children's heights, top to bottom, for a grip `total` tall.
///
/// The project list and divider are whole pixels, so the panel starts on
/// one; a fractional `total` leaves its fraction at the panel's bottom
/// edge. When both floors cannot fit, the project list's wins and the
/// panel keeps what is left — down to its header, which stays on screen
/// so the sort control does. A hidden panel takes none of the fraction:
/// a sliver of height would pass `draw`'s zero-height filter.
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
            let below = total - divider;
            let most = (below - PROJECTS_FLOOR)
                .max(chrome::AGENT_PANEL_HEADER_HEIGHT)
                .min(below);
            (divider, (fraction * below).round().max(floor).min(most))
        }
    };
    let projects = (total - divider - panel).floor().max(0.0);
    Heights {
        projects,
        divider,
        panel: total - divider - projects,
    }
}

pub(crate) struct AgentsSplitGrip<'a> {
    /// The project list, the divider, the panel.
    children: [Element<'a, Message>; 3],
    panel: PanelHeight,
}

impl<'a> AgentsSplitGrip<'a> {
    pub(crate) fn new(
        chrome: &ChromePalette,
        projects: impl Into<Element<'a, Message>>,
        panel: impl Into<Element<'a, Message>>,
        height: PanelHeight,
    ) -> Self {
        Self {
            children: [projects.into(), divider(chrome, false), panel.into()],
            panel: height,
        }
    }
}

/// The divider's own fill with the handle nub centered on it.
fn divider<'a>(chrome: &ChromePalette, hovered: bool) -> Element<'a, Message> {
    let nub = container(
        Space::new()
            .width(chrome::AGENTS_SPLIT_NUB.width)
            .height(chrome::AGENTS_SPLIT_NUB.height),
    )
    .style(chrome::split_handle(chrome, hovered));
    container(nub)
        .center_x(Fill)
        .center_y(chrome::AGENTS_SPLIT_DIVIDER_HEIGHT)
        .style(chrome::divider(chrome))
        .into()
}

impl Widget<Message, iced::Theme, iced::Renderer> for AgentsSplitGrip<'_> {
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
        let Heights {
            projects,
            divider,
            panel,
        } = heights(size.height, self.panel);
        let mut top = 0.0;
        let nodes = [projects, divider, panel]
            .into_iter()
            .zip(self.children.iter_mut().zip(&mut tree.children))
            .map(|(height, (child, tree))| {
                let exact = Size::new(size.width, height);
                let node = child
                    .as_widget_mut()
                    .layout(tree, renderer, &layout::Limits::new(exact, exact))
                    .move_to(Point::new(0.0, top));
                top += height;
                node
            })
            .collect();
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
}
