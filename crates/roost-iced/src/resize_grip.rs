//! What the sidebar's two resize grips share — the sidebar/terminal seam
//! (`sidebar_resize`) and the project list / Agents panel divider
//! (`agents_split`): which pointer events a grip takes from the content it
//! wraps, and the press anchor that decides it (#295). Each grip brings
//! only its own [`Gesture`]: where it can be grabbed, and what a move does.

use iced::advanced::{mouse, Shell};
use iced::{window, Event, Point};

#[derive(Debug)]
pub(crate) struct GripState<D> {
    pub(crate) drag: Option<D>,
    pub(crate) last_cursor: Option<Point>,
}

/// Not derived: that would demand `D: Default` of every drag.
impl<D> Default for GripState<D> {
    fn default() -> Self {
        Self {
            drag: None,
            last_cursor: None,
        }
    }
}

/// `capture_event` alone does not stop delegation, so ownership has to be
/// decided before the content is offered the event at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Ownership<E> {
    Delegate,
    /// Captured by the grip; the content never sees the event.
    Own(Option<E>),
    /// The grip reacts, but the event still belongs to the whole tree.
    PublishAndDelegate(E),
}

/// What a move does to a live drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Motion<E> {
    /// The drag goes on, publishing `E` when the move changed what it shows.
    Continue(Option<E>),
    /// The move ends the drag, publishing `E`.
    End(E),
}

/// One grip's geometry and arithmetic; the event rules are [`owns_event`]'s.
pub(crate) trait Gesture {
    type Drag: Copy;
    type Event;

    /// The drag a press at `at` starts, or `None` when `at` is off the grip.
    fn press(&self, at: Point) -> Option<Self::Drag>;

    /// What a move to `at` does to `drag`.
    fn moved(&self, drag: Self::Drag, at: Point) -> Motion<Self::Event>;

    /// What a release, or the window losing focus, ends the drag with.
    fn ended(&self) -> Self::Event;
}

/// Whether the grip takes `event`. With no `gesture` there is nothing to
/// grab: any drag in flight is dropped, and every event goes to the
/// content.
pub(crate) fn owns_event<G: Gesture>(
    state: &mut GripState<G::Drag>,
    event: &Event,
    cursor: mouse::Cursor,
    gesture: Option<&G>,
) -> Ownership<G::Event> {
    // Recorded before every early return: `ButtonPressed` carries no position
    // of its own, and iced hit-tests it against the newest cursor of the batch
    // it was drained with. A frame behind, that is wherever the pointer
    // travelled *after* the button went down, so the last move the grip
    // actually saw is the honest press anchor (issue #295). A pointer that
    // left the window invalidates it — the next entry can land anywhere, and
    // no move need be observed before the press. Moves something above the
    // grip consumed — an iced overlay, or an enclosing grip's drag — never
    // reach it and fire no CursorLeft, so the anchor can sit stale across
    // such a session; accepted — the first move after it refreshes the
    // anchor, and a press before that is a narrow window.
    match event {
        Event::Mouse(mouse::Event::CursorMoved { position, .. }) => {
            state.last_cursor = Some(*position);
        }
        Event::Mouse(mouse::Event::CursorLeft) => state.last_cursor = None,
        _ => {}
    }
    let Some(gesture) = gesture else {
        state.drag = None;
        return Ownership::Delegate;
    };
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
            // starts a gesture there (a terminal selection, a row drag) that
            // can never be completed.
            if state.drag.is_some() {
                return Ownership::Own(None);
            }
            let Some(drag) = anchored.and_then(|position| gesture.press(position)) else {
                return Ownership::Delegate;
            };
            state.drag = Some(drag);
            Ownership::Own(None)
        }
        // The move's own position keeps the drag live once the pointer leaves
        // the window, where `mouse::Cursor` reports nothing.
        Event::Mouse(mouse::Event::CursorMoved { position, .. }) => match state.drag {
            Some(drag) => match gesture.moved(drag, *position) {
                Motion::Continue(publish) => Ownership::Own(publish),
                // Dropped in the same arm, so no later move in this batch (or
                // the next) publishes for a drag that is already over.
                Motion::End(event) => {
                    state.drag = None;
                    Ownership::Own(Some(event))
                }
            },
            None => Ownership::Delegate,
        },
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            if state.drag.take().is_none() {
                return Ownership::Delegate;
            }
            Ownership::Own(Some(gesture.ended()))
        }
        // A half-finished resize is a valid size, so losing focus commits it
        // rather than reverting. Unfocus is broadcast to the whole tree, so it
        // still has to reach the content — a focused text input needs it.
        Event::Window(window::Event::Unfocused) => {
            if state.drag.take().is_none() {
                return Ownership::Delegate;
            }
            Ownership::PublishAndDelegate(gesture.ended())
        }
        _ => Ownership::Delegate,
    }
}

/// Acts on `ownership` — publishing what the grip has to say, capturing what
/// it took — and answers whether the content still gets the event.
pub(crate) fn claim<E, M>(
    ownership: Ownership<E>,
    shell: &mut Shell<'_, M>,
    message: impl Fn(E) -> M,
) -> bool {
    match ownership {
        Ownership::Delegate => true,
        Ownership::PublishAndDelegate(event) => {
            shell.publish(message(event));
            true
        }
        Ownership::Own(publish) => {
            if let Some(event) = publish {
                shell.publish(message(event));
            }
            shell.capture_event();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(ownership: Ownership<&'static str>) -> (bool, Vec<&'static str>, iced::event::Status) {
        let mut messages = Vec::new();
        let (reached, status) = {
            let mut shell = Shell::new(&mut messages);
            let reached = claim(ownership, &mut shell, |note| note);
            (reached, shell.event_status())
        };
        (reached, messages, status)
    }

    #[test]
    fn a_claim_captures_only_what_the_grip_owns() {
        use iced::event::Status;

        assert_eq!(route(Ownership::Delegate), (true, vec![], Status::Ignored));
        assert_eq!(
            route(Ownership::Own(None)),
            (false, vec![], Status::Captured)
        );
        assert_eq!(
            route(Ownership::Own(Some("moved"))),
            (false, vec!["moved"], Status::Captured)
        );
        assert_eq!(
            route(Ownership::PublishAndDelegate("ended")),
            (true, vec!["ended"], Status::Ignored),
            "an unfocus ends the drag and still reaches the content"
        );
    }
}
