//! Keyboard-focus helper.
//!
//! Terminal focus grabs funnel through [`safe_grab_focus`] so a grab on a
//! widget that isn't currently in a toplevel (mid attach / tab transition) is
//! a no-op instead of a GTK focus-chain walk into a dead widget — the #234
//! `gtk_widget_get_parent: GTK_IS_WIDGET` family. Raw `grab_focus` is forbidden
//! by clippy (`disallowed-methods` in `clippy.toml`); the few sites that must
//! always grab (the palette / rename entries, freshly shown and rooted, where
//! a *missed* focus is the bug) opt out explicitly with a reason.

use gtk4::prelude::*;

/// Grab keyboard focus only when `widget` is attached to a toplevel.
///
/// During a tab switch or attach the target can be momentarily un-rooted;
/// grabbing it then transitions focus through an ancestor chain that may
/// contain a half-destroyed widget and trips `gtk_widget_get_parent:
/// GTK_IS_WIDGET` (#234). Skipping the grab in that window is correct — the
/// legitimate focus lands once the widget is rooted (callers that need it
/// defer to an idle tick so the tree has settled).
pub fn safe_grab_focus(widget: &impl IsA<gtk4::Widget>) {
    if widget.root().is_some() {
        #[allow(clippy::disallowed_methods)] // the one sanctioned grab path
        widget.grab_focus();
    }
}
