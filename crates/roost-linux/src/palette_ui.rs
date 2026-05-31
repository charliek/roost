//! Command palette — the GTK overlay (`Cmd+Shift+P` / `Alt+Shift+P`).
//!
//! The pure model lives in `palette.rs`; this renders it as a centered
//! dark card (search entry + list) floating over the content, with no
//! backdrop dim — the terminal stays fully visible behind it. Port of
//! `mac/Sources/Roost/PalettePanel.swift`.
//!
//! Built on the App's `gtk::Overlay`: a transparent full-area
//! click-catcher child (clicks outside the card dismiss) plus the card
//! child. Keyboard nav lives on an `EventControllerKey` attached to the
//! entry (Capture phase, so Up/Down/Enter/Escape are intercepted before
//! the entry's own text handling); dismissal also fires on focus-out
//! (Cmd+Tab away, click into the terminal).
//!
//! All widgets + closures are owned by `PaletteInner` behind an `Rc`;
//! signal closures capture a `Weak` and upgrade, so the reference held
//! by the executing handler keeps the inner alive even when `on_dismiss`
//! drops the App's handle mid-callback.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::palette::{PaletteFrame, PaletteItem, PaletteState};

/// Accent blue for fuzzy-matched characters (Zed-style — the hit pops
/// as you type). Matches the running-state stripe blue used elsewhere
/// in the chrome so the palette feels of-a-piece.
const MATCH_ACCENT: &str = "#5fa3f0";

/// Gap below the tab bar so the card floats under the tabs (~1cm),
/// mirroring the Swift panel's `topGap`. Added to the measured tab-bar
/// height by the caller.
pub const TOP_GAP: i32 = 30;

/// What confirming an item does. Built by the caller (`App`) as part of
/// each frame's behavior; kept out of the pure `PaletteState`.
pub enum PaletteOutcome {
    /// Action ran; dismiss the palette.
    Close,
    /// Drill into a sub-list.
    Push(PaletteFrame, PaletteBehavior),
    /// Ignore (e.g. a confirm that should neither close nor drill in).
    /// Kept for parity with the Swift `PaletteOutcome.none`; no current
    /// behavior constructs it (empty filters are handled earlier by
    /// `selected_item` returning `None`).
    #[allow(dead_code)]
    None,
}

type HighlightFn = dyn Fn(&PaletteItem);
type ConfirmFn = dyn Fn(&PaletteItem) -> PaletteOutcome;
type CancelFn = dyn Fn();

/// Side effects for one frame, looked up by frame id.
pub struct PaletteBehavior {
    /// Live preview as the highlight moves (the theme list applies a
    /// theme). Fires on every selection change + query change.
    pub on_highlight: Option<Box<HighlightFn>>,
    /// Fires on Enter.
    pub on_confirm: Box<ConfirmFn>,
    /// Fires exactly once when the frame is left without confirming
    /// (pop, or any dismissal) — the theme list reverts here.
    pub on_cancel: Option<Box<CancelFn>>,
}

impl PaletteBehavior {
    pub fn new(on_confirm: impl Fn(&PaletteItem) -> PaletteOutcome + 'static) -> Self {
        Self {
            on_highlight: None,
            on_confirm: Box::new(on_confirm),
            on_cancel: None,
        }
    }

    pub fn on_highlight(mut self, f: impl Fn(&PaletteItem) + 'static) -> Self {
        self.on_highlight = Some(Box::new(f));
        self
    }

    pub fn on_cancel(mut self, f: impl Fn() + 'static) -> Self {
        self.on_cancel = Some(Box::new(f));
        self
    }
}

/// Thin handle the App owns. Dropping it (`palette = None`) tears down
/// the inner + its widgets.
pub struct PaletteOverlay {
    inner: Rc<PaletteInner>,
}

struct PaletteInner {
    state: RefCell<PaletteState>,
    behaviors: RefCell<HashMap<String, PaletteBehavior>>,
    overlay: gtk4::Overlay,
    catcher: gtk4::Box,
    card: gtk4::Box,
    entry: gtk4::Entry,
    list: gtk4::ListBox,
    /// Set while we programmatically rewrite the entry text (query
    /// restore on push/pop) so the `changed` handler ignores the echo.
    suppress_changed: Cell<bool>,
    /// Guards the teardown so focus-out + the removal it triggers don't
    /// re-enter dismiss.
    closing: Cell<bool>,
    /// False until `present` finishes grabbing focus; gates the
    /// focus-out dismissal so the initial focus churn can't close us.
    armed: Cell<bool>,
    /// Called once on teardown: clears `App.palette` + refocuses the
    /// terminal. Taken so it fires at most once.
    on_dismiss: RefCell<Option<Box<dyn Fn()>>>,
}

impl PaletteOverlay {
    /// Build + present the palette over `overlay`. `top_margin` pins the
    /// card under the tab bar (tab-bar height + [`TOP_GAP`]).
    /// `on_dismiss` clears the App's handle + refocuses the terminal.
    pub fn present(
        overlay: &gtk4::Overlay,
        root: PaletteFrame,
        behavior: PaletteBehavior,
        top_margin: i32,
        on_dismiss: impl Fn() + 'static,
    ) -> Self {
        let entry = gtk4::Entry::builder()
            .placeholder_text(&root.placeholder)
            .css_classes(["palette-search"])
            .build();

        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .css_classes(["palette-list"])
            .build();

        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(420)
            .child(&list)
            .build();

        let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);

        let card = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .width_request(660)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Start)
            .margin_top(top_margin)
            .css_classes(["command-palette-card"])
            .build();
        card.append(&entry);
        card.append(&separator);
        card.append(&scroll);

        // Transparent click-catcher behind the card. No dim — the
        // terminal stays visible; a click anywhere outside the card
        // (which sits on top as a sibling overlay child) dismisses.
        let catcher = gtk4::Box::builder().hexpand(true).vexpand(true).build();

        let behaviors = {
            let mut m: HashMap<String, PaletteBehavior> = HashMap::new();
            m.insert(root.id.clone(), behavior);
            m
        };

        let inner = Rc::new(PaletteInner {
            state: RefCell::new(PaletteState::new(root)),
            behaviors: RefCell::new(behaviors),
            overlay: overlay.clone(),
            catcher: catcher.clone(),
            card: card.clone(),
            entry: entry.clone(),
            list: list.clone(),
            suppress_changed: Cell::new(false),
            closing: Cell::new(false),
            armed: Cell::new(false),
            on_dismiss: RefCell::new(Some(Box::new(on_dismiss))),
        });

        inner.wire_signals();

        overlay.add_overlay(&catcher);
        overlay.add_overlay(&card);

        inner.sync_ui();
        entry.grab_focus();
        // Arm focus-out dismissal only after the current main-loop
        // iteration settles, so the grab above can't trip it.
        {
            let weak = Rc::downgrade(&inner);
            glib::idle_add_local_once(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.armed.set(true);
                }
            });
        }

        Self { inner }
    }
}

impl Drop for PaletteOverlay {
    fn drop(&mut self) {
        // Defensive: if the App's handle is dropped without a dismiss
        // (shouldn't happen — every close path routes through
        // `dismiss`), still detach the widgets so they don't linger.
        // `dismiss` sets `closing` and removes both overlays *before*
        // clearing the App's handle (which drops us), so skip when it
        // already ran — removing an unparented child trips
        // `gtk_overlay_remove_overlay`'s parent assertion.
        if !self.inner.closing.get() {
            self.inner.overlay.remove_overlay(&self.inner.catcher);
            self.inner.overlay.remove_overlay(&self.inner.card);
        }
    }
}

impl PaletteInner {
    fn wire_signals(self: &Rc<Self>) {
        // Query changed → re-filter.
        self.entry.connect_changed({
            let weak = Rc::downgrade(self);
            move |entry| {
                let Some(inner) = weak.upgrade() else { return };
                if inner.suppress_changed.get() {
                    return;
                }
                inner.state.borrow_mut().set_query(entry.text().to_string());
                inner.rebuild_rows();
                inner.select_current_row();
                inner.fire_highlight();
            }
        });

        // Key nav. Capture phase so Up/Down/Enter/Escape are handled
        // before the entry's GtkText edits the buffer / moves the
        // cursor. Printable keys Proceed → the entry inserts them and
        // `changed` fires.
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let weak = Rc::downgrade(self);
            move |_, key, _, _| {
                let Some(inner) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                use gtk4::gdk::Key;
                match key {
                    Key::Down => {
                        inner.move_selection(1);
                        glib::Propagation::Stop
                    }
                    Key::Up => {
                        inner.move_selection(-1);
                        glib::Propagation::Stop
                    }
                    Key::Return | Key::KP_Enter => {
                        inner.confirm();
                        glib::Propagation::Stop
                    }
                    Key::Escape => {
                        inner.escape();
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            }
        });
        self.entry.add_controller(keys);

        // Row click → select + confirm.
        self.list.connect_row_activated({
            let weak = Rc::downgrade(self);
            move |_, row| {
                let Some(inner) = weak.upgrade() else { return };
                inner.state.borrow_mut().set_selection(row.index() as usize);
                inner.confirm();
            }
        });

        // Click-catcher → dismiss (revert).
        let click = gtk4::GestureClick::new();
        click.connect_pressed({
            let weak = Rc::downgrade(self);
            move |_, _, _, _| {
                if let Some(inner) = weak.upgrade() {
                    inner.dismiss(false);
                }
            }
        });
        self.catcher.add_controller(click);

        // Focus-out on the card (and all descendants) → dismiss. Fires
        // when focus leaves for the terminal or another window; staying
        // within the entry/list keeps `contains_focus` true.
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    if inner.armed.get() && !inner.closing.get() {
                        inner.dismiss(false);
                    }
                }
            }
        });
        self.card.add_controller(focus);
    }

    /// Re-render the field placeholder + rows for the current frame and
    /// fire the highlight preview for the selected row.
    fn sync_ui(self: &Rc<Self>) {
        let (placeholder, query) = {
            let state = self.state.borrow();
            let f = state.current();
            (f.placeholder.clone(), f.query.clone())
        };
        self.entry.set_placeholder_text(Some(&placeholder));
        if self.entry.text() != query.as_str() {
            self.suppress_changed.set(true);
            self.entry.set_text(&query);
            self.entry.set_position(-1);
            self.suppress_changed.set(false);
        }
        self.rebuild_rows();
        self.select_current_row();
        self.fire_highlight();
    }

    /// Rebuild the list rows from `state.matches()`.
    fn rebuild_rows(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let matches = self.state.borrow().matches();
        for m in &matches {
            self.list.append(&build_row(&m.item, &m.ranges));
        }
    }

    fn select_current_row(self: &Rc<Self>) {
        let (count, selection) = {
            let state = self.state.borrow();
            (state.matches().len(), state.current().selection)
        };
        if count == 0 {
            self.list.unselect_all();
            return;
        }
        let row = selection.min(count - 1) as i32;
        if let Some(r) = self.list.row_at_index(row) {
            self.list.select_row(Some(&r));
        }
    }

    fn fire_highlight(self: &Rc<Self>) {
        let Some(item) = self.state.borrow().selected_item() else {
            return;
        };
        let id = self.state.borrow().current().id.clone();
        let behaviors = self.behaviors.borrow();
        if let Some(behavior) = behaviors.get(&id) {
            if let Some(highlight) = &behavior.on_highlight {
                highlight(&item);
            }
        }
    }

    fn move_selection(self: &Rc<Self>, delta: isize) {
        self.state.borrow_mut().move_selection(delta);
        self.select_current_row();
        self.fire_highlight();
    }

    fn confirm(self: &Rc<Self>) {
        let Some(item) = self.state.borrow().selected_item() else {
            return; // empty filter → no-op
        };
        let id = self.state.borrow().current().id.clone();
        let outcome = {
            let behaviors = self.behaviors.borrow();
            match behaviors.get(&id) {
                Some(behavior) => (behavior.on_confirm)(&item),
                None => return,
            }
        };
        match outcome {
            PaletteOutcome::Close => self.dismiss(true),
            PaletteOutcome::Push(frame, behavior) => {
                self.behaviors
                    .borrow_mut()
                    .insert(frame.id.clone(), behavior);
                self.state.borrow_mut().push(frame);
                self.sync_ui();
                self.entry.grab_focus();
            }
            PaletteOutcome::None => {}
        }
    }

    /// Escape: pop a sub-frame (firing its cancel/revert) or dismiss at
    /// the root.
    fn escape(self: &Rc<Self>) {
        let popped = self.state.borrow_mut().pop();
        match popped {
            Some(frame) => {
                {
                    let behaviors = self.behaviors.borrow();
                    if let Some(behavior) = behaviors.get(&frame.id) {
                        if let Some(cancel) = &behavior.on_cancel {
                            cancel();
                        }
                    }
                }
                self.behaviors.borrow_mut().remove(&frame.id);
                self.sync_ui();
                self.entry.grab_focus();
            }
            None => self.dismiss(false),
        }
    }

    /// Tear down. When not confirmed, fire `on_cancel` for every frame
    /// still on the stack (top-down) so an in-flight preview reverts.
    fn dismiss(self: &Rc<Self>, confirmed: bool) {
        if self.closing.get() {
            return;
        }
        self.closing.set(true);
        if !confirmed {
            let frame_ids: Vec<String> = self
                .state
                .borrow()
                .frames()
                .iter()
                .rev()
                .map(|f| f.id.clone())
                .collect();
            let behaviors = self.behaviors.borrow();
            for id in &frame_ids {
                if let Some(behavior) = behaviors.get(id) {
                    if let Some(cancel) = &behavior.on_cancel {
                        cancel();
                    }
                }
            }
        }
        self.overlay.remove_overlay(&self.catcher);
        self.overlay.remove_overlay(&self.card);
        if let Some(cb) = self.on_dismiss.borrow_mut().take() {
            cb();
        }
    }

    // MARK: IPC drive surface (palette.* ops)
    //
    // The IPC bridge reaches the live palette through these so the same
    // navigation a user drives by keyboard/mouse is exercisable over the
    // socket. They go through `confirm` / `set_query` / `dismiss`, not a
    // parallel path, so a test drives exactly what a person does.

    /// Current frame id + filter + selection + visible rows (display
    /// order), for `palette.state`.
    fn snapshot(&self) -> PaletteSnapshot {
        let state = self.state.borrow();
        let frame = state.current();
        PaletteSnapshot {
            frame: frame.id.clone(),
            query: frame.query.clone(),
            selection: frame.selection,
            items: state
                .matches()
                .into_iter()
                .map(|m| (m.item.id, m.item.title, m.item.subtitle))
                .collect(),
        }
    }

    /// Set the filter as if typed: re-filter, re-select the top match,
    /// fire the highlight. Also rewrites the entry text so the visible
    /// query matches (guarded by `suppress_changed` so the echo is
    /// ignored).
    fn drive_query(self: &Rc<Self>, query: &str) {
        self.state.borrow_mut().set_query(query.to_string());
        self.suppress_changed.set(true);
        self.entry.set_text(query);
        self.suppress_changed.set(false);
        self.rebuild_rows();
        self.select_current_row();
        self.fire_highlight();
    }

    /// Select the visible row whose item id matches, then confirm it —
    /// the same `confirm` a click/Enter runs (so it pushes a sub-frame or
    /// dispatches the command). False if no visible row has that id.
    fn drive_activate(self: &Rc<Self>, id: &str) -> bool {
        let index = self
            .state
            .borrow()
            .matches()
            .iter()
            .position(|m| m.item.id == id);
        let Some(index) = index else { return false };
        self.state.borrow_mut().set_selection(index);
        self.confirm();
        true
    }
}

/// A read of the live palette frame for the IPC bridge: current frame
/// id, its filter + highlighted row, and the visible rows as
/// `(id, title, subtitle)` in display order. GTK-free; `app.rs` maps it
/// to `roost_ipc::messages::PaletteStateResult`.
pub struct PaletteSnapshot {
    pub frame: String,
    pub query: String,
    pub selection: usize,
    pub items: Vec<(String, String, Option<String>)>,
}

impl PaletteOverlay {
    /// A handle that drives this palette without holding a borrow of
    /// `App.palette`. The IPC bridge clones it out, drops the borrow,
    /// *then* activates/dismisses — `confirm`'s dismiss path re-borrows
    /// `App.palette` (to clear the handle), so the caller must not still
    /// hold it. Used only by the `palette.*` ops.
    pub fn driver(&self) -> PaletteDriver {
        PaletteDriver {
            inner: self.inner.clone(),
        }
    }
}

/// Drives a live palette over a cloned `Rc<PaletteInner>`. See
/// [`PaletteOverlay::driver`] for the borrow-safety rationale.
pub struct PaletteDriver {
    inner: Rc<PaletteInner>,
}

impl PaletteDriver {
    pub fn snapshot(&self) -> PaletteSnapshot {
        self.inner.snapshot()
    }
    pub fn set_query(&self, query: &str) {
        self.inner.drive_query(query);
    }
    pub fn activate(&self, id: &str) -> bool {
        self.inner.drive_activate(id)
    }
    pub fn dismiss(&self) {
        self.inner.dismiss(false);
    }
}

/// Build one list row: a title label with Pango markup for the matched
/// ranges, an optional second line (subtitle — the notification message
/// body), and an optional right-aligned shortcut/time hint.
fn build_row(item: &PaletteItem, ranges: &[Range<usize>]) -> gtk4::ListBoxRow {
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);

    // Title + optional subtitle stacked vertically so a two-line
    // notification row (message under "<project> · <tab>") reads
    // cleanly while plain command rows stay single-line.
    let text_col = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text_col.set_hexpand(true);

    let title = gtk4::Label::builder()
        .use_markup(true)
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["palette-title"])
        .build();
    title.set_markup(&markup_for(&item.title, ranges));
    text_col.append(&title);

    if let Some(subtitle) = &item.subtitle {
        let sub = gtk4::Label::builder()
            .label(subtitle)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["palette-subtitle"])
            .build();
        text_col.append(&sub);
    }
    hbox.append(&text_col);

    if let Some(trailing) = &item.trailing_text {
        let shortcut = gtk4::Label::builder()
            .label(trailing)
            .xalign(1.0)
            .valign(gtk4::Align::Center)
            .css_classes(["palette-shortcut"])
            .build();
        hbox.append(&shortcut);
    }

    gtk4::ListBoxRow::builder()
        .child(&hbox)
        .css_classes(["palette-row"])
        .build()
}

/// Build Pango markup for `title`, wrapping the matched character
/// ranges in an accent-blue bold span. Offsets are character indices;
/// each segment is markup-escaped. Empty `ranges` → the whole title,
/// escaped, no spans.
fn markup_for(title: &str, ranges: &[Range<usize>]) -> String {
    let chars: Vec<char> = title.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    let mut ri = 0usize;
    while i < chars.len() {
        if ri < ranges.len() && ranges[ri].start == i {
            let end = ranges[ri].end.min(chars.len());
            let segment: String = chars[i..end].iter().collect();
            out.push_str(&format!(
                "<span foreground=\"{MATCH_ACCENT}\" weight=\"bold\">{}</span>",
                glib::markup_escape_text(&segment)
            ));
            i = end;
            ri += 1;
        } else {
            let next = ranges
                .get(ri)
                .map(|r| r.start.min(chars.len()))
                .unwrap_or(chars.len());
            let segment: String = chars[i..next].iter().collect();
            out.push_str(&glib::markup_escape_text(&segment));
            i = next;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_plain_when_no_ranges() {
        assert_eq!(markup_for("New Tab", &[]), "New Tab");
    }

    #[test]
    fn markup_escapes_special_chars() {
        // Ampersand + angle brackets must be entity-escaped for Pango.
        assert_eq!(markup_for("a & b", &[]), "a &amp; b");
    }

    #[test]
    fn markup_wraps_matched_runs() {
        // Match the first two chars of "New".
        let m = markup_for("New", &[0..2]);
        assert_eq!(
            m,
            format!("<span foreground=\"{MATCH_ACCENT}\" weight=\"bold\">Ne</span>w")
        );
    }

    #[test]
    fn markup_multiple_runs() {
        // "New Tab" with runs [0..1] and [4..5] → N … T highlighted.
        let m = markup_for("New Tab", &[0..1, 4..5]);
        assert_eq!(
            m,
            format!(
                "<span foreground=\"{c}\" weight=\"bold\">N</span>ew <span foreground=\"{c}\" weight=\"bold\">T</span>ab",
                c = MATCH_ACCENT
            )
        );
    }
}
