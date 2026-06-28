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
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// The scroller wrapping `list`. Retained so selection changes can
    /// scroll the highlighted row into view (GtkListBox doesn't do this
    /// itself when focus stays on the search entry).
    scroll: gtk4::ScrolledWindow,
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
    /// Monotonic session id (vs. the `Rc` address, which the allocator can
    /// reuse after free — an ABA hazard for the stale-provider guard).
    session_id: u64,
}

/// Next palette-session id. Monotonic for the process lifetime, so a
/// dismissed-then-reopened palette never reuses an id.
fn next_session_id() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
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
            scroll: scroll.clone(),
            suppress_changed: Cell::new(false),
            closing: Cell::new(false),
            armed: Cell::new(false),
            on_dismiss: RefCell::new(Some(Box::new(on_dismiss))),
            session_id: next_session_id(),
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

        // Keep the highlighted row visible across layout changes. The
        // scroller's vertical adjustment emits `changed` when its range
        // (content/viewport size) updates — i.e. after the rows for a
        // freshly opened or re-filtered frame are laid out. That's when
        // the pre-positioned selection (e.g. the theme list opening on
        // the active theme partway down) can finally be scrolled into
        // view; arrow nav handles itself synchronously since layout is
        // already settled.
        //
        // The scroll is deferred to an idle rather than applied inline:
        // `changed` fires *during* the ScrolledWindow's size-allocate, and
        // a `set_value` made now is overwritten when that allocation
        // finishes (it re-clamps scroll to the top). Running one main-loop
        // iteration later — after the allocation completes — makes the
        // scroll stick.
        self.scroll.vadjustment().connect_changed({
            let weak = Rc::downgrade(self);
            move |_| {
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.scroll_selection_into_view();
                    }
                });
            }
        });

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
            self.scroll_selection_into_view();
        }
    }

    /// The selected row's `(top, height)` in the list's (= scroll
    /// content) coordinate space, or `None` when there's no selection or
    /// the row isn't laid out yet (open / re-filter rebuilt the rows this
    /// turn, so sizes aren't known). `compute_bounds` is the non-deprecated
    /// successor to `allocation()` and reports the row's content position
    /// regardless of the current scroll offset.
    fn selected_row_extent(&self) -> Option<(f64, f64)> {
        let row = self.list.selected_row()?;
        let bounds = row.compute_bounds(&self.list)?;
        let h = bounds.height() as f64;
        if h <= 0.0 {
            return None;
        }
        Some((bounds.y() as f64, h))
    }

    /// Scroll so the selected row is fully visible. GtkListBox only
    /// auto-scrolls to a row when that row holds keyboard focus, but the
    /// palette keeps focus on the search entry and drives selection from
    /// its key controller — so without this the highlight rides off the
    /// top/bottom edge (worst on the theme list, which opens
    /// pre-positioned on the active theme partway down).
    ///
    /// On arrow nav layout is already settled, so this scrolls
    /// immediately. On open / re-filter the rows are rebuilt this turn and
    /// the viewport isn't laid out yet (`selected_row_extent` returns
    /// `None`); the `vadjustment::changed` handler wired in `wire_signals`
    /// re-runs this once layout settles, covering the pre-positioned open
    /// case.
    fn scroll_selection_into_view(self: &Rc<Self>) {
        let Some((y, h)) = self.selected_row_extent() else {
            return;
        };
        let adj = self.scroll.vadjustment();
        if adj.page_size() <= 0.0 {
            return;
        }
        adj.set_value(reveal_offset(y, h, adj.value(), adj.page_size()));
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
        if !item.actionable {
            return; // non-actionable row (e.g. a "No results" sentinel) — stay open
        }
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
        // Sever the toplevel focus from the card subtree while it is still
        // parented. Removing an overlay child that still holds the window
        // focus leaves a dangling focus pointer, and the next focus
        // transition then walks that dead widget → the #234 `GTK_IS_WIDGET`
        // storm. Clearing here is the always-safe floor; `on_dismiss`
        // re-grabs the terminal right after, so focus still lands where the
        // user expects.
        if let Some(root) = self.overlay.root() {
            root.set_focus(gtk4::Widget::NONE);
        }
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
        // Refocus (via on_dismiss) BEFORE tearing down the overlay. The
        // card owns the focused entry, so removing it first leaves the
        // entry a dangling focus widget: the callback's grab_focus then
        // transitions focus off a destroyed widget and GTK aborts
        // (`gtk_widget_unset_state_flags: GTK_IS_WIDGET (widget)`). Moving
        // focus while the entry is still alive, then removing the now-
        // unfocused card, is safe. The `closing` guard above absorbs the
        // re-entrant dismiss that the entry's focus-out fires.
        if let Some(cb) = self.on_dismiss.borrow_mut().take() {
            cb();
        }
        self.overlay.remove_overlay(&self.catcher);
        self.overlay.remove_overlay(&self.card);
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
            selected_in_view: self.selected_row_in_view(),
        }
    }

    /// Whether the highlighted row sits fully inside the scrolled
    /// viewport. `None` before layout (sizes unknown) or with no
    /// selection — callers treat `None` as "can't tell", so only `Some(
    /// false)` flags a genuinely clipped highlight (the bug this guards).
    ///
    /// Measured relative to the scroller (the viewport), NOT via the
    /// adjustment value: that reflects the *applied* scroll position, so a
    /// value that was set but not yet translated onto screen (e.g. clobbered
    /// by an in-flight allocation) still reads as out-of-view here. The
    /// page size bounds the visible band.
    fn selected_row_in_view(&self) -> Option<bool> {
        let row = self.list.selected_row()?;
        let bounds = row.compute_bounds(&self.scroll)?;
        let height = bounds.height() as f64;
        let page = self.scroll.vadjustment().page_size();
        if height <= 0.0 || page <= 0.0 {
            return None;
        }
        let top = bounds.y() as f64;
        let bottom = top + height;
        // Half-pixel slack absorbs fractional layout rounding.
        Some(top >= -0.5 && bottom <= page + 0.5)
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

    /// Push a sub-frame from outside `confirm` (async provider results).
    /// Mirrors the `PaletteOutcome::Push` arm: register the behavior,
    /// push the frame, re-render, and restore focus to the entry.
    fn drive_push(self: &Rc<Self>, frame: PaletteFrame, behavior: PaletteBehavior) {
        self.behaviors
            .borrow_mut()
            .insert(frame.id.clone(), behavior);
        self.state.borrow_mut().push(frame);
        self.sync_ui();
        self.entry.grab_focus();
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
    /// Whether the highlighted row is fully within the scrolled viewport
    /// (`None` = can't tell yet — pre-layout or no selection).
    pub selected_in_view: Option<bool>,
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

    /// Stable identity of this palette *session* (the `Rc` backing it).
    /// An async provider result compares this against the live palette's
    /// id to tell whether the palette it targeted is still on screen —
    /// vs. dismissed and replaced by a different one while the script ran.
    pub fn id(&self) -> u64 {
        self.inner.session_id
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
    /// Drill into a sub-frame programmatically — the same transition
    /// `PaletteOutcome::Push` performs in `confirm`, but driven from
    /// outside (a provider's async `list` result populating the palette
    /// after the spawn returns). The caller must have dropped any borrow
    /// of `App.palette` first (see [`PaletteOverlay::driver`]).
    pub fn push(&self, frame: PaletteFrame, behavior: PaletteBehavior) {
        self.inner.drive_push(frame, behavior);
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

/// Minimal vertical scroll offset that brings a row spanning
/// `[y, y + height)` fully into a viewport of height `page` currently
/// scrolled to `offset`: scroll up if the row is above the viewport,
/// down if it's below, otherwise leave `offset` unchanged.
fn reveal_offset(y: f64, height: f64, offset: f64, page: f64) -> f64 {
    if y < offset {
        y
    } else if y + height > offset + page {
        y + height - page
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_offset_keeps_offset_when_row_fully_visible() {
        assert_eq!(reveal_offset(100.0, 32.0, 50.0, 420.0), 50.0);
    }

    #[test]
    fn reveal_offset_scrolls_up_to_row_above_viewport() {
        // Row at y=50 while scrolled to 100 → bring the row top to the top.
        assert_eq!(reveal_offset(50.0, 32.0, 100.0, 420.0), 50.0);
    }

    #[test]
    fn reveal_offset_scrolls_down_to_row_below_viewport() {
        // Row at y=400..432 with viewport 0..420 → align row bottom to
        // viewport bottom (432 - 420 = 12).
        assert_eq!(reveal_offset(400.0, 32.0, 0.0, 420.0), 12.0);
    }

    #[test]
    fn reveal_offset_reveals_last_row_off_the_bottom() {
        // The reported bug: arrowing to the final row (here y=736..768) of
        // a list taller than the viewport must scroll it into view rather
        // than clip it.
        assert_eq!(reveal_offset(736.0, 32.0, 0.0, 420.0), 348.0);
    }

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
