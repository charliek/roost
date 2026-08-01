use iced::advanced::widget::operation::{self, Outcome};
use iced::advanced::widget::{Id, Operation};
use iced::{Rectangle, Vector};

const GEOMETRY_EPSILON: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Missing,
    Visible(bool),
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

pub fn ensure_visible(scroll_id: Id, row_id: Id) -> impl Operation<Visibility> {
    Locate::new(scroll_id, row_id, true)
}

pub fn measure_visible(scroll_id: Id, row_id: Id) -> impl Operation<Visibility> {
    Locate::new(scroll_id, row_id, false)
}

struct Locate {
    scroll_id: Id,
    row_id: Id,
    reveal: bool,
    viewport: Option<Rectangle>,
    content: Option<Rectangle>,
    translation: Option<Vector>,
    row: Option<Rectangle>,
}

impl Locate {
    fn new(scroll_id: Id, row_id: Id, reveal: bool) -> Self {
        Self {
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
        let decision = decide(geometry, self.reveal);
        tracing::debug!(
            ?geometry,
            ?decision,
            reveal = self.reveal,
            "palette geometry pass"
        );
        match decision {
            Decision::Missing => Outcome::Some(Visibility::Missing),
            Decision::Visible => Outcome::Some(Visibility::Visible(true)),
            Decision::Clipped => Outcome::Some(Visibility::Visible(false)),
            Decision::ScrollTo(y) => Outcome::Chain(Box::new(ApplyScroll {
                scroll_id: self.scroll_id.clone(),
                row_id: self.row_id.clone(),
                y,
            })),
        }
    }
}

struct ApplyScroll {
    scroll_id: Id,
    row_id: Id,
    y: f32,
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
            state.scroll_to(operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(self.y),
            });
        }
    }

    fn finish(&self) -> Outcome<Visibility> {
        // Re-enter the widget tree after mutating scroll state. The fresh
        // traversal observes the new translation instead of reporting from
        // the pre-scroll geometry gathered by `Locate`.
        Outcome::Chain(Box::new(Locate::new(
            self.scroll_id.clone(),
            self.row_id.clone(),
            false,
        )))
    }
}

fn decide(geometry: Geometry, reveal: bool) -> Decision {
    if geometry.viewport.height <= 0.0
        || geometry.content.height <= 0.0
        || geometry.row.height <= 0.0
    {
        return Decision::Missing;
    }
    if is_visible(geometry) {
        return Decision::Visible;
    }
    if !reveal {
        return Decision::Clipped;
    }

    let row_top = geometry.row.y - geometry.content.y;
    let row_bottom = row_top + geometry.row.height;
    let visible_top = geometry.row.y - geometry.translation.y;
    let visible_bottom = visible_top + geometry.row.height;
    let viewport_top = geometry.viewport.y;
    let viewport_bottom = viewport_top + geometry.viewport.height;
    let desired = if visible_top < viewport_top - GEOMETRY_EPSILON {
        row_top
    } else if visible_bottom > viewport_bottom + GEOMETRY_EPSILON {
        row_bottom - geometry.viewport.height
    } else {
        return Decision::Visible;
    };
    let max_offset = (geometry.content.height - geometry.viewport.height).max(0.0);
    Decision::ScrollTo(desired.clamp(0.0, max_offset))
}

fn is_visible(geometry: Geometry) -> bool {
    let row_top = geometry.row.y - geometry.translation.y;
    let row_bottom = row_top + geometry.row.height;
    let viewport_top = geometry.viewport.y;
    let viewport_bottom = viewport_top + geometry.viewport.height;
    row_top >= viewport_top - GEOMETRY_EPSILON && row_bottom <= viewport_bottom + GEOMETRY_EPSILON
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

    #[test]
    fn already_visible_row_does_not_scroll() {
        assert_eq!(
            decide(geometry(100.0, 200.0, 36.0), true),
            Decision::Visible
        );
    }

    #[test]
    fn reveal_above_uses_the_rows_top_edge() {
        assert_eq!(
            decide(geometry(200.0, 120.0, 36.0), true),
            Decision::ScrollTo(120.0)
        );
    }

    #[test]
    fn reveal_below_uses_the_minimum_bottom_edge_offset() {
        assert_eq!(
            decide(geometry(0.0, 500.0, 36.0), true),
            Decision::ScrollTo(116.0)
        );
    }

    #[test]
    fn fractional_rounding_within_half_a_pixel_is_visible() {
        assert!(is_visible(geometry(0.0, 384.4, 36.0)));
        assert!(!is_visible(geometry(0.0, 384.6, 36.0)));
    }

    #[test]
    fn measure_only_reports_clipping_without_a_reveal_chain() {
        assert_eq!(decide(geometry(0.0, 500.0, 36.0), false), Decision::Clipped);
    }

    #[test]
    fn zero_height_row_is_unavailable_not_visible() {
        assert_eq!(decide(geometry(0.0, 20.0, 0.0), true), Decision::Missing);
    }
}
