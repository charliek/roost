//! Ask the widget tree which widget holds keyboard focus — including
//! when the answer is "none".
//!
//! iced ships `operation::focusable::find_focused`, whose shape this
//! copies, but it finishes with `Outcome::None` when nothing is focused
//! and `operate` only produces a message for an `Outcome::Some`. An
//! unfocused tree would therefore answer with silence, which is the one
//! answer the Add Host ring cannot use: "nothing is focused" is a state
//! it has to step *from* (it is what a ringed button looks like, buttons
//! not being `Focusable` in iced 0.14), and a probe that sometimes
//! produces no message would leave the caller's pending flag latched with
//! nothing to clear it.

use iced::advanced::widget::operation::{Focusable, Outcome};
use iced::advanced::widget::{Id, Operation};
use iced::Rectangle;

pub fn find_focused_or_none() -> impl Operation<Option<Id>> {
    struct FindFocusedOrNone {
        focused: Option<Id>,
    }

    impl Operation<Option<Id>> for FindFocusedOrNone {
        fn focusable(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
            if state.is_focused() && id.is_some() {
                self.focused = id.cloned();
            }
        }

        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Option<Id>>)) {
            operate(self);
        }

        fn finish(&self) -> Outcome<Option<Id>> {
            Outcome::Some(self.focused.clone())
        }
    }

    FindFocusedOrNone { focused: None }
}
