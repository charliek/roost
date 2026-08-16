use iced::advanced::widget::operation::{self, Outcome};
use iced::advanced::widget::{Id, Operation};
use iced::{Rectangle, Vector};

const GEOMETRY_EPSILON: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Missing,
    Visible(bool),
}

/// Which way the scrollable this operation walks actually scrolls. The
/// geometry pass is one-dimensional either way — the palette list reveals
/// rows down its column, the tab strip reveals pills along its row — so the
/// axis is the only thing that differs between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

impl Axis {
    /// A rectangle's leading edge and extent along this axis.
    fn span(self, rect: Rectangle) -> (f32, f32) {
        match self {
            Self::Vertical => (rect.y, rect.height),
            Self::Horizontal => (rect.x, rect.width),
        }
    }

    fn translation(self, translation: Vector) -> f32 {
        match self {
            Self::Vertical => translation.y,
            Self::Horizontal => translation.x,
        }
    }

    fn absolute_offset(self, offset: f32) -> operation::scrollable::AbsoluteOffset<Option<f32>> {
        match self {
            Self::Vertical => operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(offset),
            },
            Self::Horizontal => operation::scrollable::AbsoluteOffset {
                x: Some(offset),
                y: None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    viewport: Rectangle,
    content: Rectangle,
    translation: Vector,
    row: Rectangle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Decision {
    Missing,
    Visible,
    Clipped,
    ScrollTo(f32),
}

pub fn ensure_visible(axis: Axis, scroll_id: Id, row_id: Id) -> impl Operation<Visibility> {
    Locate::new(axis, scroll_id, row_id, true)
}

pub fn measure_visible(axis: Axis, scroll_id: Id, row_id: Id) -> impl Operation<Visibility> {
    Locate::new(axis, scroll_id, row_id, false)
}

struct Locate {
    axis: Axis,
    scroll_id: Id,
    row_id: Id,
    reveal: bool,
    viewport: Option<Rectangle>,
    content: Option<Rectangle>,
    translation: Option<Vector>,
    row: Option<Rectangle>,
}

impl Locate {
    fn new(axis: Axis, scroll_id: Id, row_id: Id, reveal: bool) -> Self {
        Self {
            axis,
            scroll_id,
            row_id,
            reveal,
            viewport: None,
            content: None,
            translation: None,
            row: None,
        }
    }

    fn geometry(&self) -> Option<Geometry> {
        Some(Geometry {
            viewport: self.viewport?,
            content: self.content?,
            translation: self.translation?,
            row: self.row?,
        })
    }
}

impl Operation<Visibility> for Locate {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Visibility>)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&self.scroll_id) {
            self.viewport = Some(bounds);
            self.content = Some(content_bounds);
            self.translation = Some(translation);
        }
    }

    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if id == Some(&self.row_id) {
            self.row = Some(bounds);
        }
    }

    fn finish(&self) -> Outcome<Visibility> {
        let Some(geometry) = self.geometry() else {
            return Outcome::Some(Visibility::Missing);
        };
        let decision = decide(geometry, self.axis, self.reveal);
        tracing::debug!(
            ?geometry,
            ?decision,
            axis = ?self.axis,
            reveal = self.reveal,
            "scroll reveal geometry pass"
        );
        match decision {
            Decision::Missing => Outcome::Some(Visibility::Missing),
            Decision::Visible => Outcome::Some(Visibility::Visible(true)),
            Decision::Clipped => Outcome::Some(Visibility::Visible(false)),
            Decision::ScrollTo(offset) => Outcome::Chain(Box::new(ApplyScroll {
                axis: self.axis,
                scroll_id: self.scroll_id.clone(),
                row_id: self.row_id.clone(),
                offset,
            })),
        }
    }
}

struct ApplyScroll {
    axis: Axis,
    scroll_id: Id,
    row_id: Id,
    offset: f32,
}

impl Operation<Visibility> for ApplyScroll {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Visibility>)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        _translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&self.scroll_id) {
            state.scroll_to(self.axis.absolute_offset(self.offset));
        }
    }

    fn finish(&self) -> Outcome<Visibility> {
        // Re-enter the widget tree after mutating scroll state. The fresh
        // traversal observes the new translation instead of reporting from
        // the pre-scroll geometry gathered by `Locate`.
        Outcome::Chain(Box::new(Locate::new(
            self.axis,
            self.scroll_id.clone(),
            self.row_id.clone(),
            false,
        )))
    }
}

fn decide(geometry: Geometry, axis: Axis, reveal: bool) -> Decision {
    let (viewport_start, viewport_extent) = axis.span(geometry.viewport);
    let (content_start, content_extent) = axis.span(geometry.content);
    let (row_start, row_extent) = axis.span(geometry.row);
    if viewport_extent <= 0.0 || content_extent <= 0.0 || row_extent <= 0.0 {
        return Decision::Missing;
    }
    if is_visible(geometry, axis) {
        return Decision::Visible;
    }
    if !reveal {
        return Decision::Clipped;
    }

    let row_leading = row_start - content_start;
    let row_trailing = row_leading + row_extent;
    let visible_leading = row_start - axis.translation(geometry.translation);
    let visible_trailing = visible_leading + row_extent;
    let viewport_trailing = viewport_start + viewport_extent;
    let desired = if visible_leading < viewport_start - GEOMETRY_EPSILON {
        row_leading
    } else if visible_trailing > viewport_trailing + GEOMETRY_EPSILON {
        row_trailing - viewport_extent
    } else {
        return Decision::Visible;
    };
    let max_offset = (content_extent - viewport_extent).max(0.0);
    Decision::ScrollTo(desired.clamp(0.0, max_offset))
}

fn is_visible(geometry: Geometry, axis: Axis) -> bool {
    let (viewport_start, viewport_extent) = axis.span(geometry.viewport);
    let (row_start, row_extent) = axis.span(geometry.row);
    let leading = row_start - axis.translation(geometry.translation);
    let trailing = leading + row_extent;
    let viewport_trailing = viewport_start + viewport_extent;
    leading >= viewport_start - GEOMETRY_EPSILON && trailing <= viewport_trailing + GEOMETRY_EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(offset: f32, row_y: f32, row_height: f32) -> Geometry {
        Geometry {
            viewport: Rectangle::new([0.0, 100.0].into(), [500.0, 420.0].into()),
            content: Rectangle::new([0.0, 100.0].into(), [500.0, 900.0].into()),
            translation: Vector::new(0.0, offset),
            row: Rectangle::new([0.0, 100.0 + row_y].into(), [500.0, row_height].into()),
        }
    }

    /// The vertical fixture transposed: same numbers, x for y and width for
    /// height, so every horizontal expectation below is the vertical one
    /// read sideways — a generalization that only worked one way would
    /// show up as a diverging pair.
    fn geometry_x(offset: f32, row_x: f32, row_width: f32) -> Geometry {
        Geometry {
            viewport: Rectangle::new([100.0, 0.0].into(), [420.0, 500.0].into()),
            content: Rectangle::new([100.0, 0.0].into(), [900.0, 500.0].into()),
            translation: Vector::new(offset, 0.0),
            row: Rectangle::new([100.0 + row_x, 0.0].into(), [row_width, 500.0].into()),
        }
    }

    #[test]
    fn already_visible_row_does_not_scroll() {
        assert_eq!(
            decide(geometry(100.0, 200.0, 36.0), Axis::Vertical, true),
            Decision::Visible
        );
    }

    #[test]
    fn reveal_above_uses_the_rows_top_edge() {
        assert_eq!(
            decide(geometry(200.0, 120.0, 36.0), Axis::Vertical, true),
            Decision::ScrollTo(120.0)
        );
    }

    #[test]
    fn reveal_below_uses_the_minimum_bottom_edge_offset() {
        assert_eq!(
            decide(geometry(0.0, 500.0, 36.0), Axis::Vertical, true),
            Decision::ScrollTo(116.0)
        );
    }

    #[test]
    fn fractional_rounding_within_half_a_pixel_is_visible() {
        assert!(is_visible(geometry(0.0, 384.4, 36.0), Axis::Vertical));
        assert!(!is_visible(geometry(0.0, 384.6, 36.0), Axis::Vertical));
    }

    #[test]
    fn measure_only_reports_clipping_without_a_reveal_chain() {
        assert_eq!(
            decide(geometry(0.0, 500.0, 36.0), Axis::Vertical, false),
            Decision::Clipped
        );
    }

    #[test]
    fn zero_height_row_is_unavailable_not_visible() {
        assert_eq!(
            decide(geometry(0.0, 20.0, 0.0), Axis::Vertical, true),
            Decision::Missing
        );
    }

    #[test]
    fn already_visible_pill_does_not_scroll_the_strip() {
        assert_eq!(
            decide(geometry_x(100.0, 200.0, 36.0), Axis::Horizontal, true),
            Decision::Visible
        );
    }

    #[test]
    fn reveal_before_the_viewport_uses_the_pills_leading_edge() {
        assert_eq!(
            decide(geometry_x(200.0, 120.0, 36.0), Axis::Horizontal, true),
            Decision::ScrollTo(120.0)
        );
    }

    #[test]
    fn reveal_past_the_viewport_uses_the_minimum_trailing_edge_offset() {
        assert_eq!(
            decide(geometry_x(0.0, 500.0, 36.0), Axis::Horizontal, true),
            Decision::ScrollTo(116.0)
        );
    }

    #[test]
    fn horizontal_fractional_rounding_within_half_a_pixel_is_visible() {
        assert!(is_visible(geometry_x(0.0, 384.4, 36.0), Axis::Horizontal));
        assert!(!is_visible(geometry_x(0.0, 384.6, 36.0), Axis::Horizontal));
    }

    #[test]
    fn horizontal_measure_only_reports_clipping_without_a_reveal_chain() {
        assert_eq!(
            decide(geometry_x(0.0, 500.0, 36.0), Axis::Horizontal, false),
            Decision::Clipped
        );
    }

    #[test]
    fn zero_width_pill_is_unavailable_not_visible() {
        assert_eq!(
            decide(geometry_x(0.0, 20.0, 0.0), Axis::Horizontal, true),
            Decision::Missing
        );
    }

    #[test]
    fn each_axis_ignores_the_other_axis_overflow() {
        // A pill taller than the band it sits in is still fully revealed
        // horizontally, and a row wider than the palette is still fully
        // revealed vertically: each decision reads one axis only.
        let overflowing = Geometry {
            viewport: Rectangle::new([100.0, 0.0].into(), [420.0, 24.0].into()),
            content: Rectangle::new([100.0, 0.0].into(), [900.0, 24.0].into()),
            translation: Vector::new(0.0, 0.0),
            row: Rectangle::new([100.0, 0.0].into(), [36.0, 900.0].into()),
        };
        assert_eq!(
            decide(overflowing, Axis::Horizontal, true),
            Decision::Visible
        );
        let transposed = Geometry {
            viewport: Rectangle::new([0.0, 100.0].into(), [24.0, 420.0].into()),
            content: Rectangle::new([0.0, 100.0].into(), [24.0, 900.0].into()),
            translation: Vector::new(0.0, 0.0),
            row: Rectangle::new([0.0, 100.0].into(), [900.0, 36.0].into()),
        };
        assert_eq!(decide(transposed, Axis::Vertical, true), Decision::Visible);
    }
}
