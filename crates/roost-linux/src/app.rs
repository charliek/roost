//! Top-level App: window, sidebar, per-project tab views.
//!
//! Holds the shared `RoostClient`, the WatchEvents subscription, and
//! per-project / per-tab UI state. Sidebar = `gtk::ListBox` of project
//! rows on the left. Right pane = `gtk::Stack` of `adw::TabView`s,
//! one per project, swapped when the sidebar selection changes.
//!
//! M10: drag-to-reorder. Sidebar uses `gtk::DragSource` per row +
//! a single `gtk::DropTarget` on the listbox; motion live-shuffles
//! rows under the cursor, drop fires `ReorderProjects`. Tab pills
//! ride AdwTabBar's built-in drag — we just observe
//! `connect_page_reordered` and fire `ReorderTabs`. All ordering is
//! daemon-authoritative: drop persists, then the WatchEvents
//! `Projects/TabsReordered` arm re-applies the canonical order so
//! cross-client UIs converge.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use anyhow::Context;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita::prelude::*;
use libadwaita::{ApplicationWindow, TabView, WindowTitle};
use roost_ipc::messages::{PaletteItemView, PaletteStateResult, Project, Tab};
use tokio::runtime::Handle;

use roost_linux::daemon::{RestoreTab, WorkspaceEvent};
use roost_linux::local_client::LocalClient;
use roost_linux::reconcile;

use crate::cell_metrics::DEFAULT_FONT_SIZE_PT;
use crate::clipboard;
use crate::config;
use crate::config::{ClipboardWrite, CopyOnSelect, RoostConfig};
use crate::custom_command::{self, CustomCommand};
use crate::events;
use crate::focus::safe_grab_focus;
use crate::keybind::{
    canonicalize_bindings, default_bindings, resolve_link_modifier, Accel, AccelMods, KeybindAction,
};
use crate::notification_inbox::{
    compose_title, relative_time, NotificationInbox, NotificationRecord,
};
use crate::palette::{command_items, PaletteCommands, PaletteFrame, PaletteItem};
use crate::palette_ui::{
    PaletteBehavior, PaletteOutcome, PaletteOverlay, PaletteSnapshot, TOP_GAP,
};
use crate::provider;
use crate::rollup::{project_rollup, RollupState, TabState};
use crate::tab_session::{TabOutput, TabSession};
use crate::terminal_view::TerminalView;
use crate::theme::Theme;

/// One per project: sidebar row + tab strip + tab content stack.
struct ProjectUi {
    name: String,
    sidebar_row: gtk4::ListBoxRow,
    /// M9: sidebar row's swap-target. Two children — a `gtk::Label`
    /// (visible by default) and a `gtk::Entry` (rename-mode). M8's
    /// context-menu Rename / `KeybindAction::RenameProject` /
    /// double-click flip the visible child to the entry; Enter
    /// commits via `RenameProject` RPC, Escape cancels.
    sidebar_name_stack: gtk4::Stack,
    /// Label child of `sidebar_name_stack`. Updated on
    /// `ProjectRenamedEvent` and at project rename commit.
    sidebar_label: gtk4::Label,
    /// Entry child of `sidebar_name_stack`. Visible only during
    /// inline rename. Populated with the current name when rename
    /// starts; cleared on cancel/commit.
    sidebar_entry: gtk4::Entry,
    tab_view: TabView,
    /// Custom Mac-style tab strip (a GtkBox of TabPill widgets) shown in the
    /// top-bar `bar_stack`. The AdwTabView above is the model; this is a view
    /// of it — pills react to its page-attached/selected signals and drive it
    /// back on click/close.
    tab_strip: gtk4::Box,
    /// Tab id → pill widget, parallel to `tabs`. Built on tab attach, removed
    /// on close, restyled on selection.
    pills: RefCell<HashMap<i64, TabPill>>,
    /// Tab id → (TerminalView, TabSession).
    tabs: RefCell<HashMap<i64, TabUi>>,
    /// M9.5: tab ids whose `attach_existing_tab` is in flight (the
    /// async `TabSession::spawn` hasn't yet resolved, so the entry
    /// isn't in `tabs` yet). Checked alongside `tabs` in the dedupe
    /// guard so a Cmd+T's optimistic attach can't race against the
    /// WatchEvents `TabOpenedEvent` arm into a double-spawn. Each
    /// entry is inserted synchronously at the top of
    /// `attach_existing_tab` and removed once the session resolves
    /// (success or failure), so a failed attach doesn't leave the
    /// id permanently marked.
    pending_attaches: RefCell<HashSet<i64>>,
}

#[allow(dead_code)] // view + session held to keep the TerminalView + StreamPty alive for the tab's lifetime.
struct TabUi {
    view: Rc<TerminalView>,
    session: Rc<TabSession>,
    page: libadwaita::TabPage,
    /// Tracked so the headerbar subtitle can reflect the active
    /// tab's working directory. Mirrors the Mac UI's
    /// `TabSession.liveCwd` field. Updated at attach time and on
    /// every `TabCwdChangedEvent` (OSC 7).
    cwd: RefCell<String>,
    /// Latest agent state from `TabStateChangedEvent`. Drives the
    /// per-tab indicator icon (M7) and feeds the project rollup CSS
    /// stripe via `crate::rollup::project_rollup`.
    state: RefCell<TabState>,
    /// Whether the daemon-side Claude hook owns this tab's
    /// notification surface. Toggled by `HookActiveChangedEvent`;
    /// when true, the rollup aggregation suppresses this tab's
    /// state so the hook's own UI surface isn't duplicated.
    hook_active: RefCell<bool>,
}

/// One Mac-style pill in the custom tab strip: `[status dot | label↔entry
/// stack | notification badge | close ×]`, a composed `GtkBox` (no GObject
/// subclass) mirroring the Mac `TabPillView`. Held per tab in
/// `ProjectUi.pills`; a pure view of the project's `AdwTabView`.
struct TabPill {
    root: gtk4::Box,
    dot: gtk4::Box,
    name_stack: gtk4::Stack,
    label: gtk4::Label,
    entry: gtk4::Entry,
    close: gtk4::Button,
    badge: gtk4::Box,
}

pub struct App {
    window: ApplicationWindow,
    /// `None` before `bootstrap()` connects; closures that need the
    /// client must read this and bail (no-op) if `None`.
    client: RefCell<Option<LocalClient>>,
    rt: Handle,
    sidebar: gtk4::ListBox,
    /// The whole sidebar container — header label + scrolled list +
    /// footer `+ Project` button. Hidden as a unit by
    /// `toggle_sidebar`; otherwise the header / button would remain
    /// visible while only the list collapses.
    sidebar_box: gtk4::Box,
    /// `gtk::Stack` of TabView widgets, one entry per project id.
    /// Switching the sidebar selection flips the visible child.
    tab_stack: gtk4::Stack,
    /// Per-project tab strips, stacked in the tab-strip band beside the
    /// sidebar (in the content column, above the terminal); only the active
    /// project's is shown (switched on project change). One shared AdwTabBar
    /// rebound via `set_view` crashes libadwaita on switch (g_object_unref),
    /// so each project keeps its own bar bound to its own view for life and we
    /// flip the visible stack child instead.
    bar_stack: gtk4::Stack,
    /// Horizontal scroller wrapping the visible tab strip (`bar_stack` + the
    /// new-tab "+"), living in the tab band atop the content column. Stored so
    /// the palette can pin its card just under the band using the band's live
    /// height (the band == this scroller, no extra padding).
    tab_scroller: gtk4::ScrolledWindow,
    /// `adw::WindowTitle` shown as the AdwHeaderBar title widget (title =
    /// active project name, subtitle = active tab's cwd) — Mac titlebar
    /// parity. `set_active_project` / `refresh_window_subtitle` keep it
    /// current; `window.set_title` mirrors the same name into the OS window
    /// title (taskbar / alt-tab).
    window_title: WindowTitle,
    projects: RefCell<HashMap<i64, ProjectUi>>,
    /// Currently focused project (sidebar selection). 0 = no
    /// selection yet (e.g. workspace is empty).
    active_project_id: RefCell<i64>,
    /// Resolved theme from `~/.config/roost/config.conf` (or the
    /// bundled `roost-dark` fallback). Passed to each new
    /// TerminalView so cells use the same palette. `RefCell` because
    /// the command palette swaps it live (Select Theme…); new tabs
    /// read the current value at spawn so confirm + revert propagate
    /// forward. Not persisted — `config.conf` wins on relaunch.
    theme: RefCell<Theme>,
    /// Name of the live theme, kept alongside `theme` so the palette
    /// can pre-highlight the active row and express revert-by-name.
    /// Seeded from `cfg.theme_name` (or `roost-dark`).
    active_theme_name: RefCell<String>,
    /// Theme name captured when the palette opened, restored on
    /// dismiss-without-confirm so an in-flight live preview reverts.
    /// `None` while the palette is closed.
    theme_name_at_open: RefCell<Option<String>>,
    /// `gtk::Overlay` wrapping the content below the header, so the
    /// command palette card can float centered over the whole window.
    content_overlay: gtk4::Overlay,
    /// Vertical SizeGroup tying the sidebar "PROJECTS" header band and the
    /// tab-strip band to one height (Mac parity). Held only to keep the
    /// constraint alive: a SizeGroup is freed when its last owner drops and
    /// its members don't ref it back, so without this field the bands drift on
    /// resize. Never read.
    _band_size_group: gtk4::SizeGroup,
    /// The open command palette overlay, or `None` when closed.
    palette: RefCell<Option<crate::palette_ui::PaletteOverlay>>,
    /// Monotonic token bumped per provider run, so a superseded run's late
    /// result (provider A in flight, user picks B) is dropped rather than
    /// pushing a stale frame onto the current one.
    provider_req: std::cell::Cell<u64>,
    /// Live inbox of pending agent notifications, surfaced through the
    /// command palette ("View Notifications") + the HeaderBar button.
    /// Membership is driven off the `has_notification` edges in
    /// `handle_event`; see `notification_inbox::NotificationInbox`.
    notification_inbox: RefCell<NotificationInbox>,
    /// Count badge overlaid on the HeaderBar notifications bell. Hidden
    /// at zero; `refresh_notif_badge` keeps it in sync with the inbox.
    notif_badge: gtk4::Label,
    /// Optional font-family override from config. `RefCell` because
    /// the command palette swaps it live (Select Font…); new tabs
    /// read the current value at spawn so confirm + revert propagate
    /// forward, matching the theme story.
    font_family: RefCell<Option<String>>,
    /// Font family captured when the palette opened, restored on
    /// dismiss-without-confirm so an in-flight live preview reverts.
    /// `None` while the palette is closed or before the first open.
    font_family_at_open: RefCell<Option<Option<String>>>,
    /// Optional font-size override from config (points). Snapshot
    /// of the value read at boot; the live size (with FontIncrease /
    /// FontDecrease / FontReset adjustments) lives in
    /// `current_font_size_pt`.
    font_size_pt: Option<f64>,
    /// Live font size for the active session. Starts at
    /// `font_size_pt.unwrap_or(DEFAULT_FONT_SIZE_PT)` and shifts by
    /// ±1 on each FontIncrease / FontDecrease; FontReset snaps back
    /// to that baseline. Applied to every TerminalView via
    /// `apply_font_size_to_all`.
    current_font_size_pt: RefCell<f64>,
    /// `copy-on-select` from `~/.config/roost/config.conf` (default
    /// `True`). `RefCell` so a future config-reload path can update it
    /// without rebuilding the App; new tabs read the current value
    /// when constructed.
    copy_on_select: RefCell<CopyOnSelect>,
    /// `clipboard-write` policy from the config. Checked in
    /// `report_osc_event` to gate OSC 52 writes. Default `Allow`
    /// (matches Ghostty's default); `Deny` silently drops + logs.
    /// `RefCell` for the same future-reload reason as
    /// `copy_on_select`.
    clipboard_write_policy: RefCell<ClipboardWrite>,
    /// `word-break-chars` from the config — extra word-char set for
    /// double-click word expansion. Passed to every new TerminalView
    /// at construction. `RefCell` for the same future-reload reason
    /// as `copy_on_select`.
    word_break_chars: RefCell<String>,
    /// Resolved `link-modifier` (Cmd on macOS / Alt on Linux by
    /// default; `link-modifier` config overrides). Held during a
    /// hover/click over a URL to reveal + open it. Passed to every new
    /// TerminalView at construction.
    link_modifier: AccelMods,
    /// Tab ids whose close was triggered by the daemon (the user
    /// typed `exit`, or a CLI `tab close`, or another client's
    /// CloseTab RPC). The connect_close_page handler installed on
    /// every AdwTabView would otherwise fire `close_tab_async` here
    /// — a redundant RPC that returns `NotFound` because the daemon
    /// already deleted the tab. M9.5: mark in the set before
    /// calling `close_page`, check + remove in the close-page
    /// handler to skip the redundant RPC.
    server_driven_closes: RefCell<HashSet<i64>>,
    /// M10: project id of the row currently being dragged (`None`
    /// when no drag is in progress). The project row being
    /// reorder-dragged; doubles as the "armed past threshold" marker for
    /// the row's `GtkGestureDrag` and tells the live-shuffle which row to
    /// move. Set when the gesture arms, cleared on drag-end / cancel.
    dragged_project_id: RefCell<Option<i64>>,
    /// Tab id of the pill currently being drag-reordered (None when no
    /// pill drag is in progress); doubles as the "armed past threshold"
    /// marker for the pill's `GtkGestureDrag`. Set when the gesture arms,
    /// cleared on drag-end.
    dragged_tab_id: RefCell<Option<i64>>,
    /// M10: snapshot of the sidebar order taken when a row drag arms.
    /// Drag-end rolls back to this if the `ReorderProjects` RPC fails; a
    /// cancelled drag rolls back to it directly.
    drag_original_order: RefCell<Vec<i64>>,
    /// True while `reconcile_to_snapshot` is applying programmatic
    /// tab reorders. The `connect_page_reordered` handler checks this
    /// and skips firing a `ReorderTabs` RPC, so a resync that
    /// re-sorts pills doesn't echo spurious mutations back to the
    /// workspace. (The same primitive will gate applying a remote
    /// reorder when cross-client convergence lands.)
    suppress_tab_reorder_echo: RefCell<bool>,
    /// True while we programmatically set the AdwTabView selection (or
    /// append/remove a page). Every selection change WE make is a UI
    /// reaction to the core; only a genuine in-widget user gesture (pill
    /// click, AdwTabView Ctrl-nav) should sync the core back. The
    /// `selected-page` notify checks this to tell the two apart. Same
    /// shape as `suppress_tab_reorder_echo`; the notify fires
    /// synchronously inside the set/append/close, so the bracket covers
    /// it.
    suppress_selected_page_sync: RefCell<bool>,
    /// `ROOST_TEST_MODE=1` was set in the UI's environment at
    /// launch. Read ONCE in `App::new` and stashed here so per-op
    /// dispatch is a cheap bool check rather than a syscall, and so a
    /// tester can't toggle the gate mid-session. When false, the
    /// gated `tab.feed_pty_bytes` / `tab.capture_pty_input` ops
    /// return `not-enabled` and the capture buffer + feed sender
    /// maps below are never populated (zero overhead in production).
    test_mode: bool,
    /// Per-tab clones of the same `mpsc::UnboundedSender<TabOutput>`
    /// the real `TabSession` writes to. Populated only when
    /// `test_mode` is true; `tab.feed_pty_bytes` looks the tab id up
    /// here and pushes `TabOutput::Bytes`, which the existing OSC
    /// drain loop processes identically to real PTY output. The
    /// channel is multi-producer mpsc, so the test sender races
    /// safely with the live `TabSession`'s producer.
    feed_senders: RefCell<HashMap<i64, tokio::sync::mpsc::UnboundedSender<TabOutput>>>,
    /// Per-tab capture buffers for the `tab.capture_pty_input`
    /// op. Populated only when `test_mode` is true; the buffer is
    /// shared with `TabSession::send_input` so every outbound byte
    /// (keystrokes, paste, OSC replies) is mirrored here as it's
    /// enqueued.
    input_captures: RefCell<HashMap<i64, crate::tab_session::InputCapture>>,
}

/// Bundled chrome stylesheet, kept verbatim in sync with the Mac UI
/// so the two UIs feel identical. Loaded once at App::new and applied
/// via the display's shared style context so it composes with the
/// user's libadwaita theme rather than replacing it.
const STYLE_CSS: &str = include_str!("resources/style.css");

/// Bundled chrome icons, vendored from upstream Adwaita so the GTK
/// app doesn't depend on the user's system `adwaita-icon-theme` being
/// installed. The SVGs are embedded directly into the binary via
/// `include_bytes!`, avoiding any gresource compilation step (one less
/// build-tool dependency).
const ICON_SIDEBAR_SHOW_SYMBOLIC: &[u8] =
    include_bytes!("resources/icons/sidebar-show-symbolic.svg");
const ICON_TAB_NEW_SYMBOLIC: &[u8] = include_bytes!("resources/icons/tab-new-symbolic.svg");
const ICON_BELL_SYMBOLIC: &[u8] = include_bytes!("resources/icons/bell-symbolic.svg");

/// Per-tab status indicator icons (M7). Vendored alongside the chrome
/// icons so the GTK app doesn't depend on any system icon-theme search
/// path for these — same trade-off as the M5 chrome icons.
const ICON_RUNNING: &[u8] = include_bytes!("resources/icons/icon_running.svg");
const ICON_NEEDS_INPUT: &[u8] = include_bytes!("resources/icons/icon_needs_input.svg");
const ICON_IDLE: &[u8] = include_bytes!("resources/icons/icon_idle.svg");

/// Wrap an embedded SVG in a `gio::BytesIcon` so it can drive a
/// `gtk::Image` regardless of the user's icon-theme search path.
/// libadwaita's symbolic icons are tiny SVGs (~1 KB each), so the
/// `glib::Bytes::from_static` allocation is free at runtime.
fn embedded_icon(bytes: &'static [u8]) -> gtk4::gio::BytesIcon {
    let glib_bytes = glib::Bytes::from_static(bytes);
    gtk4::gio::BytesIcon::new(&glib_bytes)
}

/// Apply the per-tab status indicator icon to `page`. `TabState::None`
/// clears the indicator (`set_indicator_icon(None)`); the other
/// three pick the matching SVG.
fn apply_indicator_icon(page: &libadwaita::TabPage, state: TabState) {
    let icon: Option<gtk4::gio::BytesIcon> = match state {
        TabState::None => None,
        TabState::Running => Some(embedded_icon(ICON_RUNNING)),
        TabState::NeedsInput => Some(embedded_icon(ICON_NEEDS_INPUT)),
        TabState::Idle => Some(embedded_icon(ICON_IDLE)),
    };
    page.set_indicator_icon(icon.as_ref().map(|i| i.upcast_ref::<gtk4::gio::Icon>()));
}

/// Mirror the per-tab agent state onto a pill's leading dot (the colours
/// match the indicator SVGs + the rollup stripe). `None` leaves the dot
/// transparent (it keeps its slot so pills don't reflow).
fn apply_pill_dot(dot: &gtk4::Box, state: TabState) {
    for c in ["running", "needs-input", "idle"] {
        dot.remove_css_class(c);
    }
    match state {
        TabState::None => {}
        TabState::Running => dot.add_css_class("running"),
        TabState::NeedsInput => dot.add_css_class("needs-input"),
        TabState::Idle => dot.add_css_class("idle"),
    }
}

/// Render `window` (sidebar + tabs + active terminal) to PNG bytes, at
/// `scale`x pixels. Returns `(png, width, height)`. Called on the GTK
/// main thread from the `app.screenshot` drain loop. Renders through the
/// window's own `GskRenderer`, so the bundled dark theme + CSS apply
/// exactly as on screen — and, because it re-renders the widget tree
/// rather than the display, it works even when the window is unfocused
/// or occluded.
fn render_window_png(
    window: &ApplicationWindow,
    scale: u32,
) -> Result<(Vec<u8>, u32, u32), String> {
    let logical_w = window.width();
    let logical_h = window.height();
    if logical_w <= 0 || logical_h <= 0 {
        return Err("window not realized (zero size)".into());
    }
    let scale_f = scale as f32;

    // `renderer()` is `None` until the surface is realized — never
    // unwrap; an early/unrealized window is a graceful error, not a panic.
    let renderer = window
        .native()
        .and_then(|n| n.renderer())
        .ok_or_else(|| "window renderer not ready".to_string())?;

    let paintable = gtk4::WidgetPaintable::new(Some(window));
    let snapshot = gtk4::Snapshot::new();
    snapshot.scale(scale_f, scale_f);
    paintable.snapshot(&snapshot, logical_w as f64, logical_h as f64);
    let node = snapshot
        .to_node()
        .ok_or_else(|| "empty snapshot (nothing to render)".to_string())?;

    // Explicit viewport at the scaled bounds. Passing `None` would render
    // at the node's natural (1x) bounds, ignoring the scale transform.
    let viewport = gtk4::graphene::Rect::new(
        0.0,
        0.0,
        logical_w as f32 * scale_f,
        logical_h as f32 * scale_f,
    );
    let texture = renderer.render_texture(&node, Some(&viewport));

    // `glib::Bytes` is not `Send`; flatten to `Vec<u8>` here on the main
    // thread before it crosses the reply channel back to the IPC handler.
    let png = texture.save_to_png_bytes().to_vec();
    Ok((png, texture.width() as u32, texture.height() as u32))
}

/// Map a GTK [`PaletteSnapshot`] to the wire [`PaletteStateResult`] for
/// an open palette. The closed state (`open: false`) is built directly
/// as `PaletteStateResult::default()` by the caller.
fn palette_state_from(s: &PaletteSnapshot) -> PaletteStateResult {
    PaletteStateResult {
        open: true,
        frame: Some(s.frame.clone()),
        query: s.query.clone(),
        selection: s.selection as u32,
        items: s
            .items
            .iter()
            .map(|(id, title, subtitle)| PaletteItemView {
                id: id.clone(),
                title: title.clone(),
                subtitle: subtitle.clone(),
            })
            .collect(),
        selected_in_view: s.selected_in_view,
    }
}

impl App {
    /// Build the window + start the daemon bootstrap. Returns an
    /// `Rc<App>` so closures can hold references back into the App
    /// for event dispatch.
    pub fn new(
        app: &libadwaita::Application,
        rt: Handle,
        client: LocalClient,
        ui_rx: Option<tokio::sync::mpsc::UnboundedReceiver<roost_linux::ipc::UiRequest>>,
    ) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .default_width(1100)
            .default_height(700)
            .title("Roost (Linux)")
            .build();

        // M9.5: force the libadwaita color scheme to dark (a
        // terminal-app convention: the chrome around the terminal
        // looks weird in light mode because the terminal area itself
        // is always dark from the bundled roost-dark theme).
        // `ForceDark` (rather than `PreferDark`) means the user's
        // system pref doesn't flip us back to light on a
        // light-mode-system Mac dev host.
        libadwaita::StyleManager::default().set_color_scheme(libadwaita::ColorScheme::ForceDark);

        // Install the bundled chrome stylesheet on the default
        // display. `load_from_string` is infallible in current
        // gtk4-rs (parse warnings go through the GLib log writer,
        // not a Rust Result), so the only failure mode is a missing
        // `gdk::Display::default()` — vanishingly unlikely outside
        // GTK-not-initialised contexts and we fall back to the
        // unstyled defaults if it ever does happen.
        if let Some(display) = gtk4::gdk::Display::default() {
            let provider = gtk4::CssProvider::new();
            provider.load_from_string(STYLE_CSS);
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        } else {
            tracing::warn!("no default GDK display; skipping chrome CSS load");
        }

        // `adw::WindowTitle` shown centered in the AdwHeaderBar (built below)
        // as the Mac-parity titlebar text — title = active project, subtitle =
        // active tab cwd. `set_active_project` keeps it current and also
        // mirrors the name into the OS window title (taskbar / alt-tab).
        let window_title = WindowTitle::new("Roost", "connecting…");

        // Chrome buttons (sidebar toggle, new tab, notifications). The toggle
        // and bell are packed into the AdwHeaderBar; the new-tab "+" rides the
        // tab strip. Wired below after `app_struct` exists so the handlers can
        // capture an `Rc<App>` clone.
        let sidebar_toggle_button = gtk4::Button::builder()
            .child(&gtk4::Image::from_gicon(&embedded_icon(
                ICON_SIDEBAR_SHOW_SYMBOLIC,
            )))
            .css_classes(["flat"])
            .tooltip_text("Toggle sidebar")
            .build();
        let new_tab_button = gtk4::Button::builder()
            .child(&gtk4::Image::from_gicon(&embedded_icon(
                ICON_TAB_NEW_SYMBOLIC,
            )))
            .css_classes(["flat"])
            .tooltip_text("New tab")
            .build();
        // Notifications bell with an overlaid count badge. GTK has no
        // cross-project ambient signal today (the rollup is
        // `TabState`-driven + hook_active-suppressed, and
        // `set_needs_attention` only shows in the active project's tab
        // bar) — this button fills that gap. Click opens the palette
        // directly on the notifications list.
        let notif_badge = gtk4::Label::builder()
            .css_classes(["roost-notif-badge"])
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Start)
            .visible(false)
            .build();
        let notif_overlay = gtk4::Overlay::builder()
            .child(&gtk4::Image::from_gicon(&embedded_icon(ICON_BELL_SYMBOLIC)))
            .build();
        notif_overlay.add_overlay(&notif_badge);
        let notif_button = gtk4::Button::builder()
            .child(&notif_overlay)
            .css_classes(["flat"])
            .tooltip_text("Notifications")
            .build();

        // Sidebar: vertical Box of [section header] / [scrolled project
        // list] / [`+ Project` footer button]. Matches the Mac UI
        // sidebar layout verbatim — header label, list, button — so
        // users moving between the two UIs find the same affordances in
        // the same places.
        let sidebar_header = gtk4::Label::builder()
            .label("Projects")
            .halign(gtk4::Align::Start)
            .css_classes(["sidebar-section-header"])
            .build();
        // Opaque header band so the "PROJECTS" label area stays solid when
        // the sidebar is translucent — mirrors the Mac UI's solid header
        // (and the footer band below). The band fills the sidebar width;
        // the label keeps its left alignment inside it.
        let sidebar_header_band = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["roost-sidebar-header"])
            .build();
        sidebar_header_band.append(&sidebar_header);

        let sidebar = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Browse)
            .css_classes(["navigation-sidebar"])
            .vexpand(true)
            .build();
        let sidebar_scroll = gtk4::ScrolledWindow::builder()
            .child(&sidebar)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .build();

        // Flat rounded `.roost-add-project` chip (styled in style.css) to
        // match the Mac UI's subtle bezel button: a compact, centered chip,
        // not full-width. No libadwaita `.pill` (its gradient/bevel reads
        // heavier than the Mac affordance). Sits in an opaque footer band
        // (below) so the button area stays solid even when the sidebar is
        // translucent — mirrors the Mac UI's solid footer.
        let new_project_button = gtk4::Button::builder()
            .label("+ New Project")
            .css_classes(["roost-add-project"])
            .halign(gtk4::Align::Center)
            .build();
        let sidebar_footer = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["roost-sidebar-footer"])
            .build();
        sidebar_footer.append(&new_project_button);

        let sidebar_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            // Min width matches the Mac UI's sidebar floor (160) so the
            // panel can be dragged narrower; the Paned position below sets
            // the wider default.
            .width_request(160)
            .css_classes(["roost-sidebar"])
            .build();
        sidebar_box.append(&sidebar_header_band);
        sidebar_box.append(&sidebar_scroll);
        sidebar_box.append(&sidebar_footer);
        // Restore the persisted sidebar hide/show choice now, before the
        // window maps — synchronous (not in the async `bootstrap`) so the
        // first `window.metrics` query can't race the restore. Nothing
        // re-reveals it later (only `toggle_sidebar` flips visibility).
        if client.workspace.sidebar_collapsed() {
            sidebar_box.set_visible(false);
        }

        // Right pane: a Stack of per-project AdwTabView widgets. The
        // `.roost-tab-stack` class lets style.css re-opaque this pane when
        // the sidebar is translucent.
        let tab_stack = gtk4::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .css_classes(["roost-tab-stack"])
            .build();

        // Right pane = a vertical column: the per-project tab strip is a band
        // ABOVE the terminal stack and to the RIGHT of the sidebar — Mac parity
        // (tabs beside the project list, not over it). Collapsing the sidebar
        // lets this whole column take the full window width.
        //
        // `bar_stack` holds the per-project tab strips (built in
        // add_project_ui); only the active project's is shown, switched on
        // project change. `hhomogeneous(false)` lets the stack size to the
        // visible strip.
        let bar_stack = gtk4::Stack::builder()
            .hhomogeneous(false)
            .vhomogeneous(false)
            .build();
        // The per-project tab strips + the trailing new-tab "+" live in ONE
        // horizontal scroller so a project with many tabs SCROLLS instead of
        // widening the whole window, and "+" always hugs the last tab (scrolls
        // with them). propagate-natural-width=false stops the tab count from
        // forcing the toplevel wider; hexpand hands the scroller the column's
        // slack so ~10 tabs stay visible before it scrolls. External hscrollbar
        // = no scrollbar widget cramping the 24px strip; a vertical wheel over
        // the strip scrolls it horizontally (vscroll off). Mirrors the Mac,
        // which keeps the strip width-bounded.
        let tab_strip_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        tab_strip_box.append(&bar_stack);
        tab_strip_box.append(&new_tab_button);
        let tab_scroller = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::External)
            .vscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_width(false)
            .hexpand(true)
            .child(&tab_strip_box)
            .build();
        // A vertical wheel over the strip scrolls it horizontally. GTK doesn't
        // redirect wheel→hscroll under an External h-policy, so translate it
        // explicitly onto the scroller's hadjustment — otherwise tabs scrolled
        // past the viewport (and the trailing "+") are unreachable by mouse.
        let strip_scroll =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        strip_scroll.connect_scroll({
            let adj = tab_scroller.hadjustment();
            move |_, dx, dy| {
                let delta = if dx.abs() > dy.abs() { dx } else { dy };
                adj.set_value(adj.value() + delta * 48.0);
                glib::Propagation::Stop
            }
        });
        tab_scroller.add_controller(strip_scroll);
        // The strip's own band: a solid chrome-gray row carrying the hairline
        // under the header (`.roost-tab-band` in style.css). Holds only the
        // scroller; its height is matched to the sidebar header band below.
        let tab_strip_row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .css_classes(["roost-tab-band"])
            .build();
        tab_strip_row.append(&tab_scroller);

        let content_column = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        content_column.append(&tab_strip_row);
        content_column.append(&tab_stack);

        let paned = gtk4::Paned::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .resize_start_child(false)
            .shrink_start_child(false)
            .position(220)
            .start_child(&sidebar_box)
            .end_child(&content_column)
            .build();

        // Line up the sidebar "PROJECTS" header band and the tab-strip band to
        // the same height so the two top bands meet flush across the paned seam
        // (Mac parity). A vertical SizeGroup is robust where a CSS min-height
        // would drift with theme + scale factor. A SizeGroup holds refs to its
        // members but they don't ref it back, so it's stashed on `App`
        // (`_band_size_group`) to keep the height constraint alive across
        // resizes — dropping it here would silently let the bands drift.
        let band_size_group = gtk4::SizeGroup::new(gtk4::SizeGroupMode::Vertical);
        band_size_group.add_widget(&sidebar_header_band);
        band_size_group.add_widget(&tab_strip_row);

        // Wrap the content below the header in a `gtk::Overlay` so the command
        // palette card can float centered over the whole window (sidebar +
        // tabs), pinned just under the tab bar.
        let content_overlay = gtk4::Overlay::builder().child(&paned).build();

        // Header bar = Mac titlebar parity: the project/cwd title centered over
        // the full window width, the sidebar toggle on the leading edge, the
        // notifications bell on the trailing edge. AdwHeaderBar gives true
        // window-width centering of the title, automatic window-drag, and
        // WM-correct window controls (so a server-side-decoration setup doesn't
        // double them) — the affordances the old hand-rolled tab row had to
        // fake with GtkWindowControls + a GtkWindowHandle spacer.
        let header_row = libadwaita::HeaderBar::builder()
            .css_classes(["roost-headerbar"])
            .build();
        header_row.set_title_widget(Some(&window_title));
        header_row.pack_start(&sidebar_toggle_button);
        header_row.pack_end(&notif_button);

        let toolbar_view = libadwaita::ToolbarView::new();
        toolbar_view.add_top_bar(&header_row);
        toolbar_view.set_content(Some(&content_overlay));
        window.set_content(Some(&toolbar_view));

        // Projects-sidebar translucency: tint only when the display
        // supports an alpha visual AND a compositor is present. GDK
        // documents is_rgba()/is_composited() as complementary; Wayland
        // (the primary target) reports both true. Read once at startup —
        // no live re-toggle on compositor change. Style lives in the
        // `.roost-translucent` rules in style.css; absent the class the
        // stock opaque background stands.
        let translucent =
            gtk4::gdk::Display::default().is_some_and(|d| d.is_rgba() && d.is_composited());
        if translucent {
            window.add_css_class("roost-translucent");
        }

        // Load + apply user config now so the first TerminalView
        // gets the right theme + font.
        let cfg = RoostConfig::load_default();
        crate::cell_metrics::warn_if_primary_family_missing(
            &window.pango_context(),
            cfg.font_family.as_deref(),
        );
        let active_theme_name = cfg
            .theme_name
            .clone()
            .unwrap_or_else(|| "roost-dark".into());
        let theme = match cfg.theme_name.as_deref() {
            Some(name) => Theme::load_bundled(name),
            None => Theme::roost_dark(),
        };

        let app_struct = Rc::new(App {
            window,
            client: RefCell::new(Some(client)),
            rt: rt.clone(),
            sidebar: sidebar.clone(),
            sidebar_box: sidebar_box.clone(),
            tab_stack: tab_stack.clone(),
            bar_stack: bar_stack.clone(),
            tab_scroller: tab_scroller.clone(),
            window_title: window_title.clone(),
            projects: RefCell::new(HashMap::new()),
            active_project_id: RefCell::new(0),
            theme: RefCell::new(theme),
            active_theme_name: RefCell::new(active_theme_name),
            theme_name_at_open: RefCell::new(None),
            content_overlay: content_overlay.clone(),
            _band_size_group: band_size_group,
            palette: RefCell::new(None),
            provider_req: std::cell::Cell::new(0),
            notification_inbox: RefCell::new(NotificationInbox::new()),
            notif_badge: notif_badge.clone(),
            font_family: RefCell::new(cfg.font_family.clone()),
            font_family_at_open: RefCell::new(None),
            font_size_pt: cfg.font_size,
            current_font_size_pt: RefCell::new(cfg.font_size.unwrap_or(DEFAULT_FONT_SIZE_PT)),
            copy_on_select: RefCell::new(cfg.copy_on_select),
            clipboard_write_policy: RefCell::new(cfg.clipboard_write),
            word_break_chars: RefCell::new(cfg.word_break_chars.clone()),
            link_modifier: resolve_link_modifier(cfg.link_modifier),
            server_driven_closes: RefCell::new(HashSet::new()),
            dragged_project_id: RefCell::new(None),
            dragged_tab_id: RefCell::new(None),
            drag_original_order: RefCell::new(Vec::new()),
            suppress_tab_reorder_echo: RefCell::new(false),
            suppress_selected_page_sync: RefCell::new(false),
            // Read the env var exactly once at boot so per-op
            // dispatch is deterministic for the life of the process
            // — a tester can't toggle the gate mid-session. Present-
            // with-empty-value still counts (matches the Python
            // truthiness `os.environ.get("ROOST_TEST_MODE") == "1"`
            // pattern in `tools/roosttest/conftest.py`).
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            feed_senders: RefCell::new(HashMap::new()),
            input_captures: RefCell::new(HashMap::new()),
        });
        // M10: a single GtkGestureDrag on the sidebar listbox drives project
        // reorder (pointer-drag, not DnD — see `install_sidebar_reorder_gesture`
        // for why it lives on the listbox rather than per-row).
        app_struct.install_sidebar_reorder_gesture();

        // Sidebar row selection → switch the active project (UI
        // reaction). This fires for ANY selection change — user click,
        // the programmatic `select_row` in `set_active_project`, the
        // auto-select when a deleted project's row is removed — so it must
        // NOT sync the core (that would race/echo the active tab). The
        // core sync is driven by `row-activated` below, which only a
        // genuine user click/Enter fires.
        sidebar.connect_row_selected({
            let app = app_struct.clone();
            move |_, row| {
                if let Some(row) = row {
                    // Sidebar rows carry their project id in the `name`
                    // GObject property (set when we build the row).
                    if let Some(name) = row.widget_name().to_string().strip_prefix("project-") {
                        if let Ok(id) = name.parse::<i64>() {
                            app.set_active_project(id);
                        }
                    }
                }
            }
        });

        // Sidebar row *activation* (genuine user click / Enter — never a
        // programmatic selection or a structural change) → sync the core's
        // active selection to the activated project's active tab.
        sidebar.connect_row_activated({
            let app = app_struct.clone();
            move |_, row| {
                if let Some(name) = row.widget_name().to_string().strip_prefix("project-") {
                    if let Ok(id) = name.parse::<i64>() {
                        if let Some(tab_id) = app.active_tab_id(id) {
                            app.sync_core_active_tab(tab_id);
                        }
                    }
                }
            }
        });

        // Sidebar footer button → fire the same `create_new_project`
        // path the `Ctrl+Shift+N` keybind uses. Mac UI parity: both
        // ⌘N and the sidebar `+ Project` button route through the
        // identical RPC sequence (CreateProject → OpenTab → attach).
        new_project_button.connect_clicked({
            let app = app_struct.clone();
            move |_| {
                let app = app.clone();
                glib::spawn_future_local(async move {
                    if let Err(err) = app.create_new_project().await {
                        tracing::warn!(?err, "new_project (sidebar button) failed");
                    }
                });
            }
        });

        // Chrome buttons.
        // - Sidebar toggle: route through the existing ToggleSidebar
        //   action so both the keybind and the button share one
        //   code path.
        sidebar_toggle_button.connect_clicked({
            let app = app_struct.clone();
            move |_| app.toggle_sidebar()
        });
        // - `+ Tab`: open a tab in the currently active project.
        new_tab_button.connect_clicked({
            let app = app_struct.clone();
            move |_| {
                let pid = *app.active_project_id.borrow();
                if pid == 0 {
                    return;
                }
                let app = app.clone();
                glib::spawn_future_local(async move {
                    if let Err(err) = app.open_new_tab_in(pid).await {
                        tracing::warn!(?err, project_id = pid, "new_tab (headerbar) failed");
                    }
                });
            }
        });
        // - Bell: open the palette directly on the notifications list.
        notif_button.connect_clicked({
            let app = app_struct.clone();
            move |_| app.show_notifications_palette()
        });

        // Install keybinds at window scope so shortcuts fire even
        // when the terminal view doesn't have keyboard focus (e.g.
        // user clicked on the sidebar).
        app_struct.install_keybinds();

        // Register the focus-tab application action so notification
        // clicks bring the originating tab forward (Phase 6b P8
        // equivalent for the Linux UI). The action's payload is the
        // tab id; the App locates the project + tab page + flips
        // them to active.
        {
            use gtk4::gio::prelude::*;
            let focus_action =
                gtk4::gio::SimpleAction::new("focus-tab", Some(&i64::static_variant_type()));
            let app_for_focus = app_struct.clone();
            focus_action.connect_activate(move |_, target| {
                if let Some(target) = target.and_then(|t| t.get::<i64>()) {
                    app_for_focus.focus_tab_by_id(target);
                }
            });
            app.add_action(&focus_action);

            // M8: context-menu actions for sidebar rows. Detailed
            // action syntax `app.rename-project(42)` carries the
            // project id as the action target; the popover menu
            // model constructs the detailed-name string.
            let rename_action =
                gtk4::gio::SimpleAction::new("rename-project", Some(&i64::static_variant_type()));
            let app_for_rename = app_struct.clone();
            rename_action.connect_activate(move |_, target| {
                if let Some(pid) = target.and_then(|t| t.get::<i64>()) {
                    app_for_rename.begin_rename_project(pid);
                }
            });
            app.add_action(&rename_action);

            let delete_action =
                gtk4::gio::SimpleAction::new("delete-project", Some(&i64::static_variant_type()));
            let app_for_delete = app_struct.clone();
            delete_action.connect_activate(move |_, target| {
                if let Some(pid) = target.and_then(|t| t.get::<i64>()) {
                    app_for_delete.confirm_and_delete_project(pid);
                }
            });
            app.add_action(&delete_action);

            // (Per-tab Rename / Close live on the per-pill right-click popover
            // in build_tab_pill now — they call begin_rename_tab / close_page
            // directly, so the old context_menu_tab_id-based app actions are
            // gone.)
        }

        app_struct.window.present();

        // UI bridge: the IPC handler (a tokio worker) forwards every
        // main-thread-only op here as a `UiRequest`. Drain them on the
        // GTK main thread and service each — raise the window, render a
        // screenshot, or walk a tab's render state — replying over the
        // request's oneshot for the request-reply variants. One loop
        // replaces the former per-op activate/screenshot/dump drains.
        if let Some(mut ui_rx) = ui_rx {
            let app = app_struct.clone();
            let window = app_struct.window.clone();
            glib::spawn_future_local(async move {
                use roost_linux::ipc::UiRequest;
                while let Some(req) = ui_rx.recv().await {
                    match req {
                        UiRequest::Activate => window.present(),
                        UiRequest::Screenshot { scale, reply } => {
                            let _ = reply.send(render_window_png(&window, scale));
                        }
                        UiRequest::Dump { tab_id, reply } => {
                            let _ = reply.send(app.dump_tab(tab_id));
                        }
                        UiRequest::PaletteOpen { kind, reply } => {
                            let _ = reply.send(Ok(app.ipc_palette_open(&kind)));
                        }
                        UiRequest::PaletteState { reply } => {
                            let _ = reply.send(Ok(app.ipc_palette_state()));
                        }
                        UiRequest::PaletteQuery { query, reply } => {
                            let _ = reply.send(Ok(app.ipc_palette_query(&query)));
                        }
                        UiRequest::PaletteActivate { id, reply } => {
                            let _ = reply.send(app.ipc_palette_activate(&id));
                        }
                        UiRequest::PaletteDismiss { reply } => {
                            let _ = reply.send(Ok(app.ipc_palette_dismiss()));
                        }
                        UiRequest::PalettePresent {
                            title,
                            placeholder,
                            items,
                            reply,
                        } => {
                            app.ipc_palette_present(title, placeholder, items, reply);
                        }
                        UiRequest::SelectionSet {
                            tab_id,
                            anchor,
                            cursor,
                            reply,
                        } => {
                            let _ = reply.send(app.ipc_selection_set(tab_id, anchor, cursor));
                        }
                        UiRequest::SelectionClear { tab_id, reply } => {
                            let _ = reply.send(app.ipc_selection_clear(tab_id));
                        }
                        UiRequest::SelectionDump { tab_id, reply } => {
                            let _ = reply.send(app.ipc_selection_dump(tab_id));
                        }
                        UiRequest::ClipboardDump { target, reply } => {
                            app.ipc_clipboard_dump(target, reply);
                        }
                        UiRequest::ClipboardWrite { target, text } => {
                            app.ipc_clipboard_write(target, text);
                        }
                        UiRequest::TabFeedPtyBytes {
                            tab_id,
                            data,
                            reply,
                        } => {
                            let _ = reply.send(app.ipc_tab_feed_pty_bytes(tab_id, data));
                        }
                        UiRequest::TabCapturePtyInput {
                            tab_id,
                            drain,
                            reply,
                        } => {
                            let _ = reply.send(app.ipc_tab_capture_pty_input(tab_id, drain));
                        }
                        UiRequest::TabDumpResolved { tab_id, reply } => {
                            let _ = reply.send(app.ipc_tab_dump_resolved(tab_id));
                        }
                        UiRequest::TabExpandSelectionAt {
                            tab_id,
                            col,
                            row,
                            click_count,
                            reply,
                        } => {
                            let _ = reply.send(app.ipc_tab_expand_selection_at(
                                tab_id,
                                col,
                                row,
                                click_count,
                            ));
                        }
                        UiRequest::WindowMetrics { reply } => {
                            let _ = reply.send(app.ipc_window_metrics());
                        }
                        UiRequest::WindowResize {
                            width,
                            height,
                            reply,
                        } => {
                            let _ = reply.send(app.ipc_window_resize(width, height));
                        }
                        UiRequest::TabDispatchMouseEvent {
                            tab_id,
                            kind,
                            button,
                            cell_x,
                            cell_y,
                            mods,
                            reply,
                        } => {
                            let _ = reply.send(app.ipc_tab_dispatch_mouse_event(
                                tab_id, kind, button, cell_x, cell_y, mods,
                            ));
                        }
                        UiRequest::AppSetWindowFocus { focused, reply } => {
                            let _ = reply.send(app.ipc_app_set_window_focus(focused));
                        }
                        UiRequest::AppCursorShape { reply } => {
                            let _ = reply.send(app.ipc_app_cursor_shape());
                        }
                        UiRequest::AppActiveTerminalFocused { reply } => {
                            let _ = reply.send(app.ipc_app_active_terminal_focused());
                        }
                        UiRequest::AppSelectedTabId { reply } => {
                            let _ = reply.send(app.ipc_app_selected_tab_id());
                        }
                    }
                }
            });
        }

        // Boot the daemon round-trip + WatchEvents subscription on
        // the GTK main loop's async executor.
        let app_for_boot = app_struct.clone();
        glib::spawn_future_local(async move {
            if let Err(err) = app_for_boot.bootstrap().await {
                app_for_boot.window_title.set_title("Roost");
                app_for_boot.window_title.set_subtitle(&format!("{err}"));
            }
        });

        app_struct
    }

    /// One-shot bootstrap: build initial project list from the
    /// in-process workspace, subscribe to its event broadcast, open
    /// a tab in the first project if none exist.
    ///
    /// M3b: no daemon round-trip. The workspace was opened at
    /// `main()` time before the UI thread booted; we just snapshot
    /// it and subscribe.
    async fn bootstrap(self: &Rc<Self>) -> anyhow::Result<()> {
        let client = {
            let borrow = self.client.borrow();
            borrow
                .as_ref()
                .cloned()
                .context("LocalClient missing from App construction")?
        };

        let rt = self.rt.clone();
        let projects = rt
            .spawn({
                let client = client.clone();
                async move { client.list_projects().await }
            })
            .await
            .context("list_projects join")??;

        self.window_title.set_title("Roost");
        self.window_title.set_subtitle("");

        // Materialize the project list. If empty, create a default
        // "roost-linux" project so the user has something to look at.
        let projects = if projects.is_empty() {
            let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let project = rt
                .spawn({
                    let client = client.clone();
                    async move { client.create_project("roost-linux", &cwd).await }
                })
                .await
                .context("create_project join")??;
            vec![project]
        } else {
            projects
        };

        for project in &projects {
            self.add_project_ui(project);
        }

        // Session-layout restore: re-open each project's persisted
        // tabs as fresh shells in their saved directories.
        // `take_restore_layout` is one-shot (the descriptors are NOT
        // live tabs — `list_projects` above returns no tabs at boot).
        // A project with no saved tabs — or a `state.json` from a
        // build predating tab persistence — seeds one default tab.
        let restore = client.workspace.take_restore_layout();
        for project in &projects {
            let saved: &[RestoreTab] = restore
                .as_ref()
                .and_then(|r| r.projects.iter().find(|p| p.project_id == project.id))
                .map(|p| p.tabs.as_slice())
                .unwrap_or(&[]);
            for (cwd, title, user_titled) in restore_open_specs(saved) {
                // Handle a failed tab open per-tab rather than `?`-ing
                // out: one stale cwd / PTY spawn failure must not abort
                // the whole bootstrap (and the workspace subscription
                // isn't installed yet, so a bubbled error would leave
                // startup half-built). Log + continue. #95 review.
                match self.open_tab_in_with(project.id, &cwd, &title, &[]).await {
                    Ok(tab_id) if user_titled && !title.is_empty() => {
                        // Re-assert the manual-rename lock: `open_tab`
                        // always starts with `user_titled=false` (the
                        // supplied title is treated as a placeholder).
                        // `set_tab_title` flips it back to true and
                        // emits a TabTitleChanged. Without this, the
                        // first post-relaunch `set_tab_cwd` would
                        // re-derive the title (issue #196 model fix).
                        if let Err(err) = client.workspace.set_tab_title(tab_id, &title) {
                            tracing::warn!(
                                project_id = project.id,
                                tab_id,
                                ?err,
                                "restore: failed to re-lock manual title; continuing"
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!(
                        project_id = project.id,
                        cwd = %cwd,
                        ?err,
                        "restore: failed to open a saved tab; continuing"
                    ),
                }
            }
        }

        // Restore the active project + active tab selection. Fall
        // back to the first project when the saved active id is gone
        // (or unset, e.g. a legacy state.json).
        let active_project = restore
            .as_ref()
            .map(|r| r.active_project_id)
            .filter(|pid| projects.iter().any(|p| p.id == *pid))
            .or_else(|| projects.first().map(|p| p.id));
        if let Some(pid) = active_project {
            self.set_active_project(pid);
            let active_pos = restore.as_ref().map(|r| r.active_tab_position).unwrap_or(0);
            self.select_tab_by_position(pid, active_pos);
        }

        // Subscribe to the workspace event broadcast; drain on the
        // GTK main loop.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let workspace = client.workspace.clone();
            rt.spawn(async move {
                if let Err(err) = events::subscribe(workspace, tx).await {
                    tracing::warn!(?err, "workspace event subscription ended with error");
                }
            });
        }
        let app_for_drain = self.clone();
        glib::spawn_future_local(async move {
            while let Some(event) = rx.recv().await {
                app_for_drain.handle_event(event);
            }
        });
        Ok(())
    }

    /// Append a sidebar row + an `adw::TabView` for `project`.
    fn add_project_ui(self: &Rc<Self>, project: &Project) {
        let mut projects = self.projects.borrow_mut();
        if projects.contains_key(&project.id) {
            return;
        }
        // M9: row child is a `gtk::Stack` of [Label, Entry] so the
        // M8 context menu / KeybindAction::RenameProject /
        // double-click can flip to the entry without reparenting
        // widgets at click time. Default visible child = label.
        let label = gtk4::Label::builder()
            .label(&project.name)
            .halign(gtk4::Align::Start)
            .margin_top(2)
            .margin_bottom(2)
            .margin_start(10)
            .margin_end(10)
            .build();
        let entry = gtk4::Entry::builder()
            .margin_top(2)
            .margin_bottom(2)
            .margin_start(8)
            .margin_end(8)
            .build();
        let name_stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(120)
            // Size to the visible child (the short label), not the tallest
            // child — otherwise the hidden rename Entry's min-height makes
            // every row tall. The pill height is then set by the CSS
            // `min-height` on the row child.
            .vhomogeneous(false)
            .build();
        name_stack.add_named(&label, Some("label"));
        name_stack.add_named(&entry, Some("entry"));
        name_stack.set_visible_child_name("label");

        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&name_stack));
        row.set_widget_name(&format!("project-{}", project.id));
        self.sidebar.append(&row);
        // M10: project reorder is driven by the single listbox-level
        // GtkGestureDrag (`install_sidebar_reorder_gesture`), keyed off this
        // row's `project-<id>` widget name — no per-row controller needed.

        // M9: commit the rename on Enter. WatchEvents
        // `ProjectRenamedEvent` is the authoritative state — we
        // fire the RPC and let the event update the label, so a
        // concurrent CLI rename can't lose data.
        entry.connect_activate({
            let app = self.clone();
            let project_id = project.id;
            move |entry| {
                let new_name = entry.text().to_string();
                app.commit_rename_project(project_id, new_name);
            }
        });
        // M9: Escape cancels inline rename. AdwTabView's Escape
        // doesn't reach us; install a key controller on the entry.
        let entry_keys = gtk4::EventControllerKey::new();
        entry_keys.connect_key_pressed({
            let app = self.clone();
            let project_id = project.id;
            move |_, key, _, _| {
                if key == gtk4::gdk::Key::Escape {
                    app.cancel_rename_project(project_id);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        });
        entry.add_controller(entry_keys);

        // M8: right-click → context menu (Rename / Delete). Plain
        // `connect_button_press_event` isn't a thing in gtk4-rs;
        // use a GestureClick configured for button 3.
        let row_click = gtk4::GestureClick::builder()
            .button(gtk4::gdk::BUTTON_SECONDARY)
            .build();
        row_click.connect_pressed({
            let app = self.clone();
            let project_id = project.id;
            let row_weak = row.downgrade();
            move |gesture, _n, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                let Some(row) = row_weak.upgrade() else {
                    return;
                };
                app.show_project_context_menu(project_id, &row, x, y);
            }
        });
        row.add_controller(row_click);

        // M9: double-click also enters rename mode. Button 1 (primary)
        // double-press.
        let row_dblclick = gtk4::GestureClick::builder()
            .button(gtk4::gdk::BUTTON_PRIMARY)
            .build();
        row_dblclick.connect_pressed({
            let app = self.clone();
            let project_id = project.id;
            move |_, n, _, _| {
                if n == 2 {
                    app.begin_rename_project(project_id);
                }
            }
        });
        row.add_controller(row_dblclick);

        let tab_view = TabView::new();
        // Drop AdwTabView's built-in Alt+1..9 / Alt+0 tab shortcuts: on Linux
        // our project modifier is Alt, so they collide with Alt+digit =
        // SwitchProject / FontReset (and bypass the core-synced switch path).
        tab_view.remove_shortcuts(
            libadwaita::TabViewShortcuts::ALT_DIGITS | libadwaita::TabViewShortcuts::ALT_ZERO,
        );
        // Hook "close-page" so the daemon learns about the close,
        // even when the user clicks the [×] on a tab pill.
        tab_view.connect_close_page({
            let app = self.clone();
            let project_id = project.id;
            move |tv, page| {
                let tab_id = parse_tab_id_from_page(page);
                tracing::debug!(project_id, ?tab_id, "close-page signal");
                // Removing a page makes AdwTabView auto-select a survivor,
                // firing `selected-page`. That's not a user tab-switch — the
                // daemon's `close_tab` reassigns the active tab and the
                // `ActiveChanged` reaction selects it — so guard the survivor
                // notify out of the core-sync path (every close, user or
                // server-driven, funnels through here).
                let prev_sps = app.suppress_selected_page_sync.replace(true);
                tv.close_page_finish(page, true);
                app.suppress_selected_page_sync.replace(prev_sps);
                // Drop the local TabUi entry so cwd / state tracking
                // for the now-dead tab is freed and the headerbar
                // subtitle / rollup recompute don't try to look it
                // up. Snapshot the project ref out of the borrow
                // before touching `tabs` so we don't deadlock with
                // any subsequent borrow inside `close_tab_async`.
                if let Some(tab_id) = tab_id {
                    if let Some(ui) = app.projects.borrow().get(&project_id) {
                        ui.tabs.borrow_mut().remove(&tab_id);
                        // Drop this tab's pill from the custom strip.
                        if let Some(pill) = ui.pills.borrow_mut().remove(&tab_id) {
                            ui.tab_strip.remove(&pill.root);
                        }
                    }
                    let already_server_driven =
                        drain_server_driven_marker(&app.server_driven_closes, tab_id);
                    if !already_server_driven {
                        // User-initiated close (× click or Cmd+W)
                        // — tell the daemon. Server-driven closes
                        // (shell exit, CLI tab close from another
                        // client) skip this RPC since the daemon
                        // already deleted the tab.
                        app.close_tab_async(project_id, tab_id);
                    }
                }
                glib::Propagation::Stop
            }
        });
        // M10: AdwTabBar handles the drag UX itself (built-in
        // GTK4 capability). We just listen for the completed
        // reorder and fire `ReorderTabs` RPC so the daemon
        // persists the new order; the WatchEvents `TabsReordered`
        // arm handles cross-client convergence. See
        // `reorder_tabs_async`.
        tab_view.connect_page_reordered({
            let app = self.clone();
            let project_id = project.id;
            move |tv, _moved_page, _new_idx| {
                // Keep the strip's pills ordered like the AdwTabView pages on
                // every reorder (user drag, keybind, or daemon resync).
                app.resync_pill_order(project_id);
                // Skip the RPC echo when the reorder is our own
                // programmatic resync, not a user drag.
                if *app.suppress_tab_reorder_echo.borrow() {
                    return;
                }
                let n = tv.n_pages();
                let mut ordered = Vec::with_capacity(n as usize);
                for i in 0..n {
                    let page = tv.nth_page(i);
                    if let Some(tab_id) = parse_tab_id_from_page(&page) {
                        ordered.push(tab_id);
                    }
                }
                app.reorder_tabs_async(project_id, ordered);
            }
        });
        // Switching tabs within a project updates the headerbar
        // subtitle (cwd of the now-active tab). Cheap idempotent
        // refresh; no-op if this project isn't the active one.
        tab_view.connect_selected_page_notify({
            let app = self.clone();
            let project_id = project.id;
            move |tab_view| {
                // Repaint pill highlight on every selection change (incl.
                // programmatic: close-survivor, Ctrl-nav, bootstrap restore).
                app.restyle_pills(project_id);
                if *app.active_project_id.borrow() != project_id {
                    return;
                }
                app.refresh_window_subtitle();
                // Sync the core ONLY for genuine in-widget user gestures —
                // a tab-pill click (#228) or AdwTabView's Ctrl-nav (#229).
                // Every selection change WE make (set_selected_page, append,
                // close survivor) is wrapped in `suppress_selected_page_sync`
                // so it doesn't echo back: those are UI reactions to the
                // core, and syncing them would race/overwrite (e.g. revert
                // `active()` to the bootstrap tab — the PR #227 hazard). The
                // suppress flag, not this `active_project_id` gate, is what
                // provides that safety. `sync_core_active_tab` is idempotent;
                // its `focus_tab` delivers `ActiveChanged` asynchronously, so
                // the reaction's (guarded) `set_selected_page` can't loop.
                if *app.suppress_selected_page_sync.borrow() {
                    return;
                }
                if let Some(tab_id) = tab_view
                    .selected_page()
                    .and_then(|p| parse_tab_id_from_page(&p))
                {
                    app.sync_core_active_tab(tab_id);
                }
            }
        });
        // (The per-tab context menu is now a per-pill right-click popover in
        // build_tab_pill — the AdwTabView setup-menu had no trigger without the
        // AdwTabBar.)

        // Per-project custom tab strip (Mac-style pills) shown in the top-bar
        // `bar_stack`; only the active project's is visible. Pills are built on
        // tab attach (attach_existing_tab) and react to this project's
        // AdwTabView. Each project keeps its own strip — flipping the visible
        // stack child switches the shown tabs (a single shared AdwTabBar
        // rebound via set_view crashed libadwaita).
        let tab_strip = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["roost-tab-strip"])
            .build();
        // Reorder is driven by each pill's GtkGestureDrag (see build_tab_pill),
        // not GTK DnD — so the strip needs no DropTarget. The old DragSource +
        // DropTarget pair crashed the whole process on Wayland: GTK's drag-icon
        // surface tripped a `gdksurface-wayland.c:348:frame_callback` assertion
        // (a frame callback delivered to an already-destroyed surface), and the
        // press routinely lost the race to the window-move handle.
        self.bar_stack
            .add_named(&tab_strip, Some(&stack_name(project.id)));

        // The AdwTabView is the page container (terminal pages), held in the
        // content stack.
        let project_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        project_box.append(&tab_view);

        self.tab_stack
            .add_named(&project_box, Some(&stack_name(project.id)));

        projects.insert(
            project.id,
            ProjectUi {
                name: project.name.clone(),
                sidebar_row: row,
                sidebar_name_stack: name_stack,
                sidebar_label: label,
                sidebar_entry: entry,
                tab_view,
                tab_strip,
                pills: RefCell::new(HashMap::new()),
                tabs: RefCell::new(HashMap::new()),
                pending_attaches: RefCell::new(HashSet::new()),
            },
        );
    }

    // ----- Phase 3: custom tab pills (Mac-style strip) -------------

    /// The project's tab view, cloned out so the caller can drop the projects
    /// borrow before driving it (set/close/reorder fire signals that re-enter
    /// the borrow). Used by the pill click / close / context-menu handlers.
    fn project_tab_view(&self, project_id: i64) -> Option<TabView> {
        self.projects
            .borrow()
            .get(&project_id)
            .map(|ui| ui.tab_view.clone())
    }

    /// Build a Mac-style tab pill for `tab_id` and wire its interactions. The
    /// pill is a view of `page` in `project_id`'s AdwTabView: clicking selects
    /// the page (the existing `selected-page` handler syncs the core), double-
    /// click renames, and the close × closes (the existing `close-page`
    /// handler removes the pill + fires the RPC). The `name_stack`
    /// (label↔entry) reuses the sidebar's inline-rename pattern.
    fn build_tab_pill(
        self: &Rc<Self>,
        project_id: i64,
        tab_id: i64,
        page: &libadwaita::TabPage,
        title: &str,
    ) -> TabPill {
        let dot = gtk4::Box::builder()
            .css_classes(["roost-tab-pill-dot"])
            .valign(gtk4::Align::Center)
            .build();
        let label = gtk4::Label::builder()
            .label(title)
            .css_classes(["roost-tab-pill-label"])
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .max_width_chars(20)
            .single_line_mode(true)
            .build();
        let entry = gtk4::Entry::builder()
            .css_classes(["roost-tab-pill-entry"])
            .max_width_chars(20)
            .build();
        let name_stack = gtk4::Stack::builder().hhomogeneous(false).build();
        name_stack.add_named(&label, Some("label"));
        name_stack.add_named(&entry, Some("entry"));
        name_stack.set_visible_child_name("label");
        let badge = gtk4::Box::builder()
            .css_classes(["roost-tab-pill-badge"])
            .valign(gtk4::Align::Center)
            .visible(false)
            .build();
        let close = gtk4::Button::builder()
            .css_classes(["roost-tab-pill-close", "flat"])
            .child(&gtk4::Label::new(Some("×")))
            .valign(gtk4::Align::Center)
            .visible(false)
            .build();
        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(2)
            .css_classes(["roost-tab-pill"])
            .build();
        root.append(&dot);
        root.append(&name_stack);
        root.append(&badge);
        root.append(&close);

        // Click selects the page. We do NOT suppress the selection, so the
        // existing `selected-page` handler runs and syncs the core (#228). The
        // projects borrow is dropped before `set_selected_page` so the notify
        // handler (which may borrow projects) can't double-borrow. Re-focus the
        // terminal after, since the click lands GTK focus on the pill.
        let click = gtk4::GestureClick::new();
        click.connect_released({
            let app = self.clone();
            let page = page.clone();
            move |_, n, _, _| {
                if let Some(tv) = app.project_tab_view(project_id) {
                    tv.set_selected_page(&page);
                    // Box pills aren't focusable, so the terminal keeps GTK
                    // focus across the switch; re-grab the now-active tab's
                    // terminal so typing goes to it. (A double-click's rename
                    // below grabs the entry afterwards, so it wins.) Deferred to
                    // an idle tick — parity with the sidebar selection path — so
                    // the grab lands after the page switch settles (a synchronous
                    // grab can be stranded mid-transition). The #234 crash itself
                    // is prevented by the focus-ownership guards in
                    // `focus_active_terminal` / palette dismiss; this defer only
                    // makes focus reliably stick on the new page.
                    let app = app.clone();
                    glib::idle_add_local_once(move || app.focus_active_terminal());
                }
                if n == 2 {
                    app.begin_rename_tab(project_id, tab_id);
                }
            }
        });
        root.add_controller(click);

        // Close × → close the page (the existing close-page handler removes the
        // pill entry + fires the RPC). Borrow dropped before `close_page`.
        close.connect_clicked({
            let app = self.clone();
            let page = page.clone();
            move |_| {
                if let Some(tv) = app.project_tab_view(project_id) {
                    tv.close_page(&page);
                }
            }
        });

        // Inline rename: begin_rename_tab flips the name_stack to the entry +
        // focuses it; Enter commits (the SetTabTitle RPC's TabTitle event
        // updates the label text), Esc or focus-out cancels — back to label.
        entry.connect_activate({
            let app = self.clone();
            let ns = name_stack.clone();
            move |entry| {
                ns.set_visible_child_name("label");
                app.commit_rename_tab(tab_id, entry.text().to_string());
                // Hand focus back to the terminal — the entry just hid.
                app.focus_active_terminal();
            }
        });
        let esc = gtk4::EventControllerKey::new();
        esc.connect_key_pressed({
            let app = self.clone();
            let ns = name_stack.clone();
            move |_, key, _, _| {
                if key == gtk4::gdk::Key::Escape {
                    ns.set_visible_child_name("label");
                    app.focus_active_terminal();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        });
        entry.add_controller(esc);
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave({
            let app = self.clone();
            let ns = name_stack.clone();
            // Cancel on focus-out, and restore focus to the active terminal
            // like the Enter/Escape paths — otherwise clicking away from the
            // entry hides it but leaves GTK focus stranded on the now-hidden
            // entry. Idempotent with the explicit handlers (their child-switch
            // also fires leave).
            move |_| {
                ns.set_visible_child_name("label");
                app.focus_active_terminal();
            }
        });
        entry.add_controller(focus);

        // Pointer-drag reorder (NOT GTK DnD). A GtkGestureDrag arms once the
        // pointer crosses a small threshold, claims the event sequence (so the
        // click gesture is cancelled and the window-move handle never sees the
        // press — fixing "dragging a pill moves the whole window"), then on
        // release moves the page via the proven-stable `reorder_page`
        // (drop_reorder_tab). This replaces the DragSource/DropTarget, whose
        // Wayland drag-icon surface aborted the process in
        // `gdksurface-wayland.c:348:frame_callback`. Plain pointer events mean
        // no drag surface — and unlike DnD it is reproducible with synthetic
        // input (xdotool/uinput), so CI can gate it. `dragged_tab_id` doubles
        // as the "armed past threshold" marker: a sub-threshold press stays
        // unclaimed and falls through to the click gesture (select / rename).
        let reorder = gtk4::GestureDrag::new();
        reorder.set_button(gtk4::gdk::BUTTON_PRIMARY);
        reorder.connect_drag_update({
            let app = self.clone();
            let ns = name_stack.clone();
            let root = root.clone();
            move |g, off_x, off_y| {
                // Arm once past the threshold; until then it stays a click.
                if *app.dragged_tab_id.borrow() != Some(tab_id) {
                    if app.dragged_tab_id.borrow().is_some() {
                        return; // a different pill is mid-drag
                    }
                    // Don't hijack an inline-rename — the entry owns the pointer.
                    if ns.visible_child_name().as_deref() == Some("entry") {
                        return;
                    }
                    // Stay a click until past the drag threshold (~8px slop).
                    if off_x.hypot(off_y) < 8.0 {
                        return;
                    }
                    g.set_state(gtk4::EventSequenceState::Claimed);
                    *app.dragged_tab_id.borrow_mut() = Some(tab_id);
                    root.set_opacity(0.4);
                }
                // Armed: live-shuffle the strip under the pointer so the result
                // is visible before release (matching the sidebar's feel).
                app.live_reorder_pill(g, &root, project_id, tab_id);
            }
        });
        reorder.connect_drag_end({
            let app = self.clone();
            let root = root.clone();
            move |g, _off_x, _off_y| {
                // Only act if this pill armed a drag; otherwise it was a click
                // (handled by the click gesture) and we must not reorder.
                if *app.dragged_tab_id.borrow() != Some(tab_id) {
                    return;
                }
                root.set_opacity(1.0);
                *app.dragged_tab_id.borrow_mut() = None;
                // Settle the final slot (covers a release the last motion didn't
                // reach), then persist the resulting order once.
                app.live_reorder_pill(g, &root, project_id, tab_id);
                app.persist_tab_order(project_id);
            }
        });
        root.add_controller(reorder);

        // Right-click context menu — Rename / Close. Replaces the AdwTabBar's
        // setup-menu (gone with the bar); a small flat popover anchored at the
        // click, mirroring the Mac pill's right-click affordance.
        let secondary = gtk4::GestureClick::builder()
            .button(gtk4::gdk::BUTTON_SECONDARY)
            .build();
        secondary.connect_released({
            let app = self.clone();
            let root = root.clone();
            let page = page.clone();
            move |_, _, x, y| {
                let pop = gtk4::Popover::builder()
                    .has_arrow(false)
                    .position(gtk4::PositionType::Bottom)
                    .build();
                pop.set_parent(&root);
                pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                let rename = gtk4::Button::builder()
                    .label("Rename")
                    .css_classes(["flat"])
                    .build();
                let close_item = gtk4::Button::builder()
                    .label("Close")
                    .css_classes(["flat"])
                    .build();
                vbox.append(&rename);
                vbox.append(&close_item);
                pop.set_child(Some(&vbox));
                rename.connect_clicked({
                    let app = app.clone();
                    let pop = pop.clone();
                    move |_| {
                        pop.popdown();
                        app.begin_rename_tab(project_id, tab_id);
                    }
                });
                close_item.connect_clicked({
                    let app = app.clone();
                    let pop = pop.clone();
                    let page = page.clone();
                    move |_| {
                        pop.popdown();
                        if let Some(tv) = app.project_tab_view(project_id) {
                            tv.close_page(&page);
                        }
                    }
                });
                pop.connect_closed(|p| p.unparent());
                pop.popup();
            }
        });
        root.add_controller(secondary);

        TabPill {
            root,
            dot,
            name_stack,
            label,
            entry,
            close,
            badge,
        }
    }

    /// Mark the pill of the project's selected tab `.active` (revealing its
    /// close ×) and clear the others — mirrors the AdwTabBar checked state.
    /// Called on every selection change, so it tracks programmatic selects
    /// (close-survivor, Ctrl-nav, bootstrap) too. Cheap (a handful of tabs).
    fn restyle_pills(self: &Rc<Self>, project_id: i64) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        let selected = ui
            .tab_view
            .selected_page()
            .and_then(|p| parse_tab_id_from_page(&p));
        let tabs = ui.tabs.borrow();
        for (tid, pill) in ui.pills.borrow().iter() {
            let active = Some(*tid) == selected;
            if active {
                pill.root.add_css_class("active");
            } else {
                pill.root.remove_css_class("active");
            }
            pill.close.set_visible(active);
            // Notification badge: Mac shows it on inactive notified tabs.
            let notified = tabs.get(tid).is_some_and(|t| t.page.needs_attention());
            pill.badge.set_visible(notified && !active);
        }
    }

    /// Refresh one pill's leading dot (agent state) + notification badge from
    /// its tab's current state. Called wherever the indicator icon or
    /// needs-attention is updated, so the pill tracks the AdwTabPage.
    fn refresh_pill_indicators(self: &Rc<Self>, project_id: i64, tab_id: i64) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        let pills = ui.pills.borrow();
        let tabs = ui.tabs.borrow();
        if let (Some(pill), Some(tab_ui)) = (pills.get(&tab_id), tabs.get(&tab_id)) {
            apply_pill_dot(&pill.dot, *tab_ui.state.borrow());
            let active = ui
                .tab_view
                .selected_page()
                .and_then(|p| parse_tab_id_from_page(&p))
                == Some(tab_id);
            pill.badge
                .set_visible(tab_ui.page.needs_attention() && !active);
        }
    }

    /// Reorder the strip's pills to match the AdwTabView's page order. Called
    /// from page-reordered, so pills follow the model on any reorder — a user
    /// drag, a keybind, or a daemon-driven resync.
    fn resync_pill_order(self: &Rc<Self>, project_id: i64) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        let pills = ui.pills.borrow();
        let mut prev: Option<gtk4::Widget> = None;
        for i in 0..ui.tab_view.n_pages() {
            let page = ui.tab_view.nth_page(i);
            if let Some(pill) = parse_tab_id_from_page(&page).and_then(|t| pills.get(&t)) {
                ui.tab_strip.reorder_child_after(&pill.root, prev.as_ref());
                prev = Some(pill.root.clone().upcast());
            }
        }
    }

    /// Handle a pill drop at strip-x `x`: move the dragged tab's page to the
    /// index implied by the pointer (its insertion point among the other
    /// pills), then page-reordered resyncs the pills + fires the RPC.
    fn drop_reorder_tab(self: &Rc<Self>, project_id: i64, src_tab: i64, x: f64) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        let pills = ui.pills.borrow();
        // Insertion index = number of OTHER pills whose centre is left of x,
        // in the strip's visual (= AdwTabView) order.
        let mut idx = 0i32;
        for i in 0..ui.tab_view.n_pages() {
            let page = ui.tab_view.nth_page(i);
            let Some(tid) = parse_tab_id_from_page(&page) else {
                continue;
            };
            if tid == src_tab {
                continue;
            }
            if let Some(pill) = pills.get(&tid) {
                if let Some(b) = pill.root.compute_bounds(&ui.tab_strip) {
                    if x > (b.x() + b.width() / 2.0) as f64 {
                        idx += 1;
                    }
                }
            }
        }
        let src_page = ui.tabs.borrow().get(&src_tab).map(|t| t.page.clone());
        if let Some(page) = src_page {
            ui.tab_view.reorder_page(&page, idx);
        }
    }

    /// Live-shuffle during a pill drag: move the dragged page to the slot under
    /// the gesture's *current* pointer, with the reorder RPC echo suppressed so
    /// the strip reshuffles continuously and the write happens once on drop.
    /// Reads the gesture's live point (not start+offset) so the index stays
    /// correct as the dragged pill itself snaps between slots under the cursor —
    /// start+offset would lag by a pill width after each move and oscillate.
    fn live_reorder_pill(
        self: &Rc<Self>,
        g: &gtk4::GestureDrag,
        root: &gtk4::Box,
        project_id: i64,
        tab_id: i64,
    ) {
        let Some((px, py)) = g.point(None) else {
            return;
        };
        let Some(parent) = root.parent() else {
            return;
        };
        let pt = gtk4::graphene::Point::new(px as f32, py as f32);
        let Some(p) = root.compute_point(&parent, &pt) else {
            return;
        };
        *self.suppress_tab_reorder_echo.borrow_mut() = true;
        self.drop_reorder_tab(project_id, tab_id, p.x() as f64);
        *self.suppress_tab_reorder_echo.borrow_mut() = false;
    }

    /// Persist `project_id`'s current AdwTabView page order via the reorder RPC.
    /// The pill drag live-shuffles with the echo suppressed, so it calls this
    /// once on drop to write the settled order.
    fn persist_tab_order(self: &Rc<Self>, project_id: i64) {
        let Some(tv) = self.project_tab_view(project_id) else {
            return;
        };
        let n = tv.n_pages();
        let mut ordered = Vec::with_capacity(n as usize);
        for i in 0..n {
            if let Some(tab_id) = parse_tab_id_from_page(&tv.nth_page(i)) {
                ordered.push(tab_id);
            }
        }
        self.reorder_tabs_async(project_id, ordered);
    }

    // ----- M8 + M9: rename / delete project flow -------------------

    /// Open the inline rename entry on `project_id`'s sidebar row.
    /// Idempotent — calling while already in rename mode keeps the
    /// entry visible and re-focuses it.
    fn begin_rename_project(self: &Rc<Self>, project_id: i64) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        ui.sidebar_entry.set_text(&ui.name);
        ui.sidebar_name_stack.set_visible_child_name("entry");
        #[allow(clippy::disallowed_methods)] // rename entry must always focus, not skip
        ui.sidebar_entry.grab_focus();
        ui.sidebar_entry.select_region(0, -1);
    }

    /// Cancel an in-progress inline rename — flip the Stack back to
    /// the label, drop the entry text.
    fn cancel_rename_project(self: &Rc<Self>, project_id: i64) {
        let projects = self.projects.borrow();
        if let Some(ui) = projects.get(&project_id) {
            ui.sidebar_name_stack.set_visible_child_name("label");
            ui.sidebar_entry.set_text("");
        }
        drop(projects);
        // M9.5: post-cancel, focus goes back to the terminal so the
        // user can resume typing instead of being stranded on the
        // sidebar row.
        self.focus_active_terminal();
    }

    /// Commit a rename: fire `RenameProject` RPC and flip the Stack
    /// back to the label. The label text updates via the
    /// WatchEvents `ProjectRenamedEvent` arm, not optimistically —
    /// keeps a single source of truth and matches the
    /// cross-cutting "WatchEvents-only mutation" invariant in
    /// `goal-linux-gtk-parity-2026-05-17.md`.
    fn commit_rename_project(self: &Rc<Self>, project_id: i64, new_name: String) {
        // Always flip back to label first so the user gets feedback
        // even when the RPC is in flight.
        let projects = self.projects.borrow();
        if let Some(ui) = projects.get(&project_id) {
            ui.sidebar_name_stack.set_visible_child_name("label");
        }
        drop(projects);
        // M9.5: post-commit, focus goes back to the terminal even
        // when the rename was a no-op (empty string) or the RPC is
        // still in flight. Avoids the dead-end where the entry has
        // disappeared but the row still owns keyboard focus.
        self.focus_active_terminal();
        let trimmed = new_name.trim().to_string();
        if trimmed.is_empty() {
            return; // empty rename = no-op
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Err(err) = client.rename_project(project_id, &trimmed).await {
                tracing::warn!(?err, project_id, "rename_project failed");
            }
        });
    }

    /// Show the right-click context menu for a sidebar row at the
    /// click coordinates. Two items: Rename (flips Stack to entry)
    /// and Delete (pops `adw::AlertDialog`, then fires
    /// `DeleteProject` on confirm).
    fn show_project_context_menu(
        self: &Rc<Self>,
        project_id: i64,
        row: &gtk4::ListBoxRow,
        x: f64,
        y: f64,
    ) {
        let menu = gtk4::gio::Menu::new();
        menu.append(
            Some("Rename"),
            Some(&format!("app.rename-project({project_id})")),
        );
        menu.append(
            Some("Delete"),
            Some(&format!("app.delete-project({project_id})")),
        );
        let popover = gtk4::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(row);
        popover.set_has_arrow(false);
        let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        popover.set_pointing_to(Some(&rect));
        // Ensure the popover is freed when dismissed (default popovers
        // can leak references if their parent outlives them).
        popover.connect_closed(|p| p.unparent());
        popover.popup();
    }

    /// Confirm + delete a project via `gtk::AlertDialog`. Confirmation
    /// is required because DeleteProject cascades to delete every
    /// tab in the project. `adw::AlertDialog` doesn't ship in
    /// libadwaita 0.8 (added in 0.10); the gtk-4.10 alert dialog
    /// covers the same surface area and lands in the same pixel
    /// position.
    fn confirm_and_delete_project(self: &Rc<Self>, project_id: i64) {
        let name = self
            .projects
            .borrow()
            .get(&project_id)
            .map(|ui| ui.name.clone())
            .unwrap_or_else(|| format!("project {project_id}"));
        let dialog = gtk4::AlertDialog::builder()
            .modal(true)
            .message("Delete project?")
            .detail(format!(
                "“{name}” and all of its tabs will be deleted. This cannot be undone."
            ))
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let app = self.clone();
        let parent = self.window.clone();
        dialog.choose(
            Some(&parent),
            None::<&gtk4::gio::Cancellable>,
            move |result| {
                match result {
                    Ok(1) => {
                        let Some(client) = app.client.borrow().clone() else {
                            return;
                        };
                        let rt = app.rt.clone();
                        rt.spawn(async move {
                            if let Err(err) = client.delete_project(project_id).await {
                                tracing::warn!(?err, project_id, "delete_project failed");
                            }
                        });
                    }
                    // Cancel (0), Dismissed, or any other result is
                    // the happy abort path.
                    _ => {}
                }
            },
        );
    }

    // ----- M8 + M9: rename / close tab flow ------------------------

    /// Begin an inline rename of `(project_id, tab_id)` in its pill: flip the
    /// pill's `name_stack` to the entry, seed + focus it. Enter commits, Esc /
    /// focus-out cancels (wired once in `build_tab_pill`). Mirrors the Mac
    /// `TabPillView` inline rename — the entry edits in place on the pill, not
    /// a popover.
    fn begin_rename_tab(self: &Rc<Self>, project_id: i64, tab_id: i64) {
        // Renaming a tab in a background project: activate it first so its strip
        // (and pill) is the visible bar_stack child. No-op for the common
        // active-project case.
        if *self.active_project_id.borrow() != project_id {
            self.set_active_project(project_id);
        }
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        // Select the target tab so the strip shows it + the core follows. The
        // guarded select suppresses the notify auto-sync, so sync explicitly;
        // no-op when it's already active (the common Alt+R case).
        let page = ui.tabs.borrow().get(&tab_id).map(|t| t.page.clone());
        let Some(page) = page else {
            return;
        };
        self.select_page_programmatic(&ui.tab_view, &page);
        self.sync_core_active_tab(tab_id);
        // Flip the pill into inline-edit mode.
        let pills = ui.pills.borrow();
        if let Some(pill) = pills.get(&tab_id) {
            pill.entry.set_text(&page.title());
            pill.name_stack.set_visible_child_name("entry");
            // Defer the focus to an idle tick: when this rename also activated
            // the project (the non-active case), set_active_project scheduled
            // an idle `focus_active_terminal`; ours runs after it (FIFO) so the
            // entry keeps focus instead of the terminal stealing it back.
            let entry = pill.entry.clone();
            glib::idle_add_local_once(move || {
                #[allow(clippy::disallowed_methods)] // rename entry must always focus, not skip
                entry.grab_focus();
                entry.select_region(0, -1);
            });
        }
    }

    /// Commit a tab rename. Same one-way-data-flow rule as
    /// `commit_rename_project`: fire `SetTabTitle` RPC, let the
    /// WatchEvents `TabTitle` event update the page's title.
    fn commit_rename_tab(self: &Rc<Self>, tab_id: i64, new_title: String) {
        let trimmed = new_title.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Err(err) = client.set_tab_title(tab_id, &trimmed).await {
                tracing::warn!(?err, tab_id, "set_tab_title failed");
            }
        });
    }

    /// Set the active project — show its TabView in the stack and
    /// keep the sidebar selection in sync (idempotent so the
    /// `connect_row_selected` handler can call this without looping).
    fn set_active_project(self: &Rc<Self>, project_id: i64) {
        if *self.active_project_id.borrow() == project_id {
            return;
        }
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        self.tab_stack
            .set_visible_child_name(&stack_name(project_id));
        // Show this project's tab strip in the top-bar stack.
        self.bar_stack
            .set_visible_child_name(&stack_name(project_id));
        self.window_title.set_title(&ui.name);
        // Surface the active project in the OS window title (taskbar / alt-tab);
        // the in-chrome title widget was removed for Mac-parity minimal chrome.
        self.window.set_title(Some(&ui.name));
        let subtitle = active_tab_cwd(ui);
        self.window_title.set_subtitle(&subtitle);
        // Sync sidebar selection without re-firing the handler.
        self.sidebar.select_row(Some(&ui.sidebar_row));
        drop(projects);
        *self.active_project_id.borrow_mut() = project_id;
        // M9.5: clicking a project shouldn't leave focus on the sidebar
        // row — hand focus to the active tab's TerminalView (matches the
        // Mac `selectProject` ending in `makeFirstResponder`).
        //
        // Deferred to an idle tick: a real sidebar-row *click* focuses
        // the `GtkListBoxRow` as part of click handling, and that focus
        // settles *after* this handler returns — a synchronous grab is
        // immediately stolen back by the row, stranding focus (cursor
        // hollow). The next main-loop iteration runs after the click
        // settles, so the grab sticks. (IPC/keyboard switches don't focus
        // a row, so only real clicks showed the bug.)
        let app = self.clone();
        glib::idle_add_local_once(move || {
            app.focus_active_terminal();
        });
    }

    /// Move keyboard focus to the active project's active tab's
    /// TerminalView. No-op if there is no active tab (e.g. the
    /// workspace just emptied). Used by `set_active_project`,
    /// `commit_rename_project`, `cancel_rename_project` and the
    /// post-attach hook so post-chrome-interaction the user can
    /// resume typing without an extra mouse click.
    fn focus_active_terminal(self: &Rc<Self>) {
        // While a palette is open it owns the keyboard (mirroring the
        // shortcut suppression in `dispatch_action`). Grabbing the terminal
        // here would transition focus off the palette's entry; if that entry
        // is mid-teardown the focus walk hits a dead widget → the #234
        // `gtk_widget_get_parent: GTK_IS_WIDGET` storm/crash. `dismiss_palette`
        // clears `self.palette` *before* it refocuses, so the legitimate
        // post-dismiss grab still runs.
        if self.palette.borrow().is_some() {
            return;
        }
        if let Some(view) = self.active_terminal_view() {
            safe_grab_focus(view.widget());
        }
    }

    /// Recompute the headerbar subtitle from the active project's
    /// active tab's cwd. Called whenever the active tab changes
    /// (sidebar selection switch, tab pill click) or the active
    /// tab's cwd updates (OSC 7 via WatchEvents `TabCwd`).
    fn refresh_window_subtitle(self: &Rc<Self>) {
        let projects = self.projects.borrow();
        let active = *self.active_project_id.borrow();
        if let Some(ui) = projects.get(&active) {
            let subtitle = active_tab_cwd(ui);
            self.window_title.set_subtitle(&subtitle);
        }
    }

    /// OpenTab RPC → on success, attach the tab to the project's
    /// TabView.
    async fn open_new_tab_in(self: &Rc<Self>, project_id: i64) -> anyhow::Result<()> {
        let cwd = self.launch_cwd(project_id);
        self.open_tab_in_with(project_id, &cwd, "", &[])
            .await
            .map(|_| ())
    }

    /// New-tab/launcher cwd, native-first: a native read of the active
    /// tab's shell cwd (current, even for shells that don't emit OSC 7,
    /// e.g. stock /bin/bash) → the OSC 7-tracked cwd → "" (open_tab then
    /// resolves project cwd → $HOME → `/`). A new tab spawns a LOCAL
    /// shell, so the local path is what it should inherit. Shared by
    /// Cmd-T (`open_new_tab_in`) and the command launcher
    /// (`launch_command`) so both inherit the active dir identically.
    fn launch_cwd(self: &Rc<Self>, project_id: i64) -> String {
        let tracked = self.active_tab_cwd(project_id);
        let native = self.active_tab_id(project_id).and_then(|tid| {
            self.client
                .borrow()
                .as_ref()
                .and_then(|c| c.supervisor.foreground_cwd(tid))
        });
        resolve_launch_cwd(native, &tracked)
    }

    /// Open a tab in `project_id` starting at `cwd` with placeholder
    /// `title` (both empty → resolved/derived by `LocalClient::open_tab`),
    /// then attach it in the UI. Returns the new tab id. Shared by
    /// the new-tab path (empty cwd/title/argv) and session restore (which
    /// passes each saved tab's cwd + title). `argv` runs in place of the
    /// default `$SHELL` — the launcher passes `$SHELL -i -c '<cmd>'`; an
    /// empty slice keeps the default shell.
    async fn open_tab_in_with(
        self: &Rc<Self>,
        project_id: i64,
        cwd: &str,
        title: &str,
        argv: &[String],
    ) -> anyhow::Result<i64> {
        let Some(client) = self.client.borrow().clone() else {
            return Ok(0);
        };
        let rt = self.rt.clone();
        let cwd = cwd.to_string();
        let title = title.to_string();
        let argv = argv.to_vec();
        let (tab, _rx) = rt
            .spawn(async move {
                client
                    .open_tab(project_id, &cwd, &title, &argv, 80, 24)
                    .await
            })
            .await
            .context("open_tab join")??;
        let tab_id = tab.id;
        // Optimistic attach: the workspace's TabOpened event also
        // arrives via the events subscription; the
        // `ProjectUi.pending_attaches` synchronous insert below
        // closes that race. We drop the broadcast receiver returned
        // by `client.open_tab` here because `attach_existing_tab`
        // re-subscribes via `PtySupervisor::subscribe_output` — the
        // window between spawn returning and subscribe_output
        // running is single-digit microseconds in-process, well
        // inside the broadcast channel's per-subscriber buffer.
        self.attach_existing_tab(tab);
        Ok(tab_id)
    }

    /// Set the AdwTabView's selected page as a UI reaction to the core,
    /// without the `selected-page` notify echoing it back into the core.
    /// Use this for every programmatic selection change; genuine in-widget
    /// user gestures (pill click, Ctrl-nav) bypass it and so do sync the
    /// core. `replace`/restore (not plain `= true/false`) keeps nesting
    /// safe. See `connect_selected_page_notify` and
    /// `suppress_selected_page_sync`.
    fn select_page_programmatic(
        self: &Rc<Self>,
        tab_view: &libadwaita::TabView,
        page: &libadwaita::TabPage,
    ) {
        let prev = self.suppress_selected_page_sync.replace(true);
        tab_view.set_selected_page(page);
        self.suppress_selected_page_sync.replace(prev);
    }

    /// Select the tab at `position` (0-based; equals open order, so
    /// it matches the saved layout's position) in `project_id`'s
    /// `AdwTabView`, syncing the workspace's active selection. Used at
    /// bootstrap to restore the saved active tab. `attach_existing_tab`
    /// appends each page synchronously, so the pages are present here
    /// even though the rest of the attach completes asynchronously.
    fn select_tab_by_position(self: &Rc<Self>, project_id: i64, position: i32) {
        let page = {
            let projects = self.projects.borrow();
            let Some(ui) = projects.get(&project_id) else {
                return;
            };
            let n = ui.tab_view.n_pages();
            if n == 0 {
                return;
            }
            let page = ui.tab_view.nth_page(position.clamp(0, n - 1));
            self.select_page_programmatic(&ui.tab_view, &page);
            page
        };
        if let Some(tab_id) = parse_tab_id_from_page(&page) {
            if let Some(client) = self.client.borrow().clone() {
                // Sync the workspace's active selection. Emits
                // `ActiveChanged`, but the event subscription isn't
                // draining yet (set up after bootstrap), so it's a
                // no-op for the UI here.
                let _ = client.workspace.focus_tab(tab_id);
            }
        }
        // `set_active_project` (above) focused whatever page was
        // selected before this; hand keyboard focus to the restored
        // tab's terminal so relaunch lands input on it. #95 review.
        self.focus_active_terminal();
    }

    /// Wrap an existing daemon tab in UI: TerminalView + TabSession +
    /// AdwTabPage. Called both at bootstrap (existing tabs) and on
    /// `TabOpened` events from WatchEvents.
    fn attach_existing_tab(self: &Rc<Self>, tab: Tab) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&tab.project_id) else {
            tracing::warn!(
                tab_id = tab.id,
                project_id = tab.project_id,
                "attach_existing_tab: unknown project"
            );
            return;
        };
        // M9.5: synchronously dedupe against BOTH the live tabs map
        // AND the pending-attach set. The async TabSession::spawn
        // below doesn't populate `ui.tabs` until it resolves, so a
        // bare `tabs.contains_key` check race-passes when Cmd+T's
        // optimistic attach + WatchEvents TabOpened both fire for
        // the same tab. Inserting into `pending_attaches` here
        // (synchronous, before the await) closes the race — the
        // second caller sees the marker and returns.
        if is_already_attached_or_pending(&ui.tabs.borrow(), &ui.pending_attaches.borrow(), tab.id)
        {
            return;
        }
        ui.pending_attaches.borrow_mut().insert(tab.id);
        // Use the *live* font size (post-FontIncrease/Decrease) so a
        // tab opened after the user has zoomed in matches the
        // existing tabs, rather than snapping back to the config
        // baseline.
        let terminal = Rc::new(TerminalView::with_theme_font_and_copy(
            self.theme.borrow().clone(),
            self.font_family.borrow().as_deref(),
            Some(*self.current_font_size_pt.borrow()),
            *self.copy_on_select.borrow(),
            self.word_break_chars.borrow().clone(),
            self.link_modifier,
        ));
        let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel::<TabOutput>();
        let Some(client_for_session) = self.client.borrow().clone() else {
            return;
        };
        let tab_id = tab.id;
        let rt = self.rt.clone();
        // Test-mode wiring. Both are no-ops outside ROOST_TEST_MODE=1:
        //   - `feed_senders` clones the same `output_tx` the real
        //     `TabSession` already writes to, so `tab.feed_pty_bytes`
        //     sends `TabOutput::Bytes` through the same mpsc consumer
        //     loop the OSC drain reads from. Multi-producer mpsc
        //     means the test sender races safely with the live
        //     producer; single consumer drains FIFO.
        //   - `input_captures` allocates a shared buffer that
        //     `TabSession::send_input` mirrors every keystroke / OSC
        //     reply / paste into, for `tab.capture_pty_input` to read.
        let input_capture: Option<crate::tab_session::InputCapture> = if self.test_mode {
            self.feed_senders
                .borrow_mut()
                .insert(tab_id, output_tx.clone());
            let buf: crate::tab_session::InputCapture =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            self.input_captures.borrow_mut().insert(tab_id, buf.clone());
            Some(buf)
        } else {
            None
        };
        // M3b: subscribe to the in-process PtySupervisor. The
        // attach is synchronous (no gRPC dial), but we still hop
        // through `rt.spawn` so the drain task it kicks off lands
        // on the tokio runtime rather than the glib main loop.
        let supervisor = client_for_session.supervisor.clone();
        let session_handle = rt
            .spawn(async move { TabSession::attach(supervisor, tab_id, output_tx, input_capture) });

        // `append` makes the first page of an empty AdwTabView its
        // selection, firing `selected-page` before the page is named — guard
        // it so the notify doesn't treat the bootstrap append as a user
        // gesture. (Subsequent appends don't change the selection.)
        let prev_sps = self.suppress_selected_page_sync.replace(true);
        let page: libadwaita::TabPage = ui.tab_view.append(terminal.widget());
        self.suppress_selected_page_sync.replace(prev_sps);
        let page_for_future = page.clone();
        let label = if tab.title.is_empty() {
            format!("Tab {}", tab.id)
        } else {
            tab.title.clone()
        };
        page.set_title(&label);
        // Tag the page with the tab id so close handler can read it.
        terminal
            .widget()
            .set_widget_name(&format!("tab-{}", tab.id));

        // Build this tab's pill in the project's custom strip (a view of the
        // AdwTabView). restyle marks the just-selected pill active.
        let pill = self.build_tab_pill(tab.project_id, tab.id, &page, &label);
        ui.tab_strip.append(&pill.root);
        ui.pills.borrow_mut().insert(tab.id, pill);
        self.restyle_pills(tab.project_id);

        // Defer the rest of the wiring until the session actually
        // spawns. The session_handle's JoinHandle is awaited on the
        // GTK main loop's executor.
        let app_for_attach = self.clone();
        let project_id = tab.project_id;
        let terminal_for_drain = terminal.clone();
        let tab_cwd = tab.cwd.clone();
        // Carry the snapshot's accumulated metadata onto the attach so
        // a resync (or any non-fresh tab) reflects ground truth. Fresh
        // tabs from `tab.open` carry defaults, so this is a no-op there.
        let tab_state = TabState::from_ipc(tab.state);
        let tab_has_notification = tab.has_notification;
        let tab_hook_active = tab.hook_active;
        let page_for_cleanup = page.clone();
        glib::spawn_future_local(async move {
            // Helper that clears the pending-attach marker AND tears
            // down the orphaned AdwTabPage on failure. Without the
            // page teardown, a failed attach would leave a dead pill
            // in the TabView with no session backing it. Without
            // clearing the pending marker, the id would be
            // permanently dedupe'd and the next user attempt would
            // be silently dropped.
            let fail_cleanup = || {
                let projects = app_for_attach.projects.borrow();
                if let Some(ui) = projects.get(&project_id) {
                    ui.pending_attaches.borrow_mut().remove(&tab_id);
                    // Mark the tab as server-driven so the close-page
                    // signal handler skips the CloseTab RPC. The
                    // daemon still has the tab — we're only tearing
                    // down the orphaned UI page after a local
                    // StreamPty attach failure (transient daemon
                    // hiccup, resource limit). Sending CloseTab here
                    // would let a recoverable failure nuke persisted
                    // daemon state. CodeRabbit caught this on PR #64.
                    app_for_attach
                        .server_driven_closes
                        .borrow_mut()
                        .insert(tab_id);
                    // libadwaita's internal `page_belongs_to_this_view`
                    // check handles the case where the page is already
                    // gone; calling close_page on a stranger is a
                    // silent no-op.
                    ui.tab_view.close_page(&page_for_cleanup);
                }
                // Drop the test-mode entries we registered above
                // before kicking off the session future. The TabClosed
                // event arm wouldn't fire on this path because the
                // workspace never saw a successful attach, so this
                // cleanup is the only place that catches the leak.
                app_for_attach.drop_test_mode_state(tab_id);
            };
            let session = match session_handle.await {
                Ok(Ok(s)) => Rc::new(s),
                Ok(Err(err)) => {
                    tracing::warn!(?err, tab_id, "StreamPty spawn failed");
                    fail_cleanup();
                    return;
                }
                Err(join_err) => {
                    tracing::warn!(?join_err, tab_id, "StreamPty join failed");
                    fail_cleanup();
                    return;
                }
            };
            terminal_for_drain.set_on_input({
                let session = session.clone();
                move |bytes| session.send_input(bytes)
            });
            // Push grid reflows to the PTY (ioctl TIOCSWINSZ) so a
            // full-screen TUI fills the window and reflows on resize.
            // Installing the callback also forces an immediate reflow,
            // catching the case where the widget's first `resize` signal
            // fired before this attach future resolved.
            terminal_for_drain.set_on_resize({
                let session = session.clone();
                move |cols, rows| session.send_resize(cols, rows)
            });
            // HiDPI diagnostic (no metric-math change — see plan Phase
            // 5). GTK4's DrawingArea draw `cairo::Context` is pre-scaled
            // by the device scale factor, so the logical-px cell metrics
            // render crisply without manual scaling. Log the realized
            // terminal widget's factor once so a future HiDPI
            // investigation has the value on hand.
            static SCALE_LOGGED: std::sync::Once = std::sync::Once::new();
            SCALE_LOGGED.call_once(|| {
                tracing::info!(
                    scale_factor = terminal_for_drain.widget().scale_factor(),
                    "gtk terminal scale factor"
                );
            });
            let projects = app_for_attach.projects.borrow();
            if let Some(ui) = projects.get(&project_id) {
                // Clear the pending marker *before* inserting the real
                // entry into `tabs` so the dedupe guard's next-call
                // semantics flip cleanly: "in flight" → "live".
                ui.pending_attaches.borrow_mut().remove(&tab_id);
                ui.tabs.borrow_mut().insert(
                    tab_id,
                    TabUi {
                        view: terminal_for_drain.clone(),
                        session: session.clone(),
                        page: page_for_future.clone(),
                        cwd: RefCell::new(tab_cwd),
                        // Seeded from the snapshot `Tab`: fresh tabs from
                        // `tab.open` carry `None` / `false`; a resync
                        // carries the accumulated state. Subsequent
                        // `TabStateChanged` / `HookActiveChanged` events
                        // keep these current.
                        state: RefCell::new(tab_state),
                        hook_active: RefCell::new(tab_hook_active),
                    },
                );
            }
            drop(projects);
            // Reflect the snapshot's indicator / attention state on the
            // freshly-attached page (no-op for fresh tabs), then refresh
            // the rollup so a restored notification/state shows up.
            apply_indicator_icon(&page_for_future, tab_state);
            page_for_future.set_needs_attention(tab_has_notification);
            app_for_attach.refresh_rollup_for(project_id);
            app_for_attach.refresh_pill_indicators(project_id, tab_id);
            // Update the headerbar subtitle if this tab belongs to
            // the active project; cheap idempotent refresh.
            app_for_attach.refresh_window_subtitle();

            // Output drain: PTY bytes → renderer + OSC scanner
            // (Phase 7 commit 10). For each OSC event the scanner
            // emits, fire `ReportOsc` to the daemon so the routing
            // layer (Phase 6b P5) can update title / cwd / state /
            // notification fields. Bytes still flow through
            // `vt_write` unchanged so libghostty owns the rendering
            // state.
            let app_for_osc = app_for_attach.clone();
            let app_for_exit = app_for_attach.clone();
            let session_for_replies = session.clone();
            glib::spawn_future_local(async move {
                let mut scanner = roost_osc::OscScanner::new();
                while let Some(msg) = output_rx.recv().await {
                    match msg {
                        TabOutput::Bytes(data) => {
                            let events = scanner.feed(&data);
                            for event in events {
                                // Synthesise OSC 10/11/12 query
                                // replies — libghostty-vt drops the
                                // .query arm, so without us answering
                                // codex (and reportedly claude-code)
                                // skip their prompt-row bg SGR. The
                                // reply rides the same per-tab serial
                                // PTY-input channel as keystrokes
                                // (`TabSession::send_input`), so it's
                                // FIFO-ordered with other writes
                                // *once enqueued* — not with PTY
                                // output that hasn't been drained yet.
                                // Reads libghostty's *currently
                                // effective* colors so a prior
                                // `OSC 10/11/12;rgb:…` set is
                                // reflected in the next query reply
                                // (vim colorscheme plugins etc.).
                                if let roost_osc::OscEvent::ColorQuery(n) = event {
                                    let color = match terminal_for_drain.live_colors() {
                                        Ok(c) => match n {
                                            10 => Some(c.foreground),
                                            11 => Some(c.background),
                                            // Cursor may be unset; fall
                                            // back to the theme.
                                            12 => c.cursor.or_else(|| {
                                                Some(app_for_osc.theme.borrow().cursor)
                                            }),
                                            _ => None,
                                        },
                                        // Libghostty isn't reporting a
                                        // default color yet (theme push
                                        // hasn't landed, or FFI hiccup);
                                        // the theme is what we last
                                        // asked the terminal to render
                                        // with, so it's the right
                                        // fallback.
                                        Err(err) => {
                                            tracing::debug!(
                                                ?err,
                                                "live_colors failed; falling back to theme"
                                            );
                                            let theme = app_for_osc.theme.borrow();
                                            match n {
                                                10 => Some(theme.foreground),
                                                11 => Some(theme.background),
                                                12 => Some(theme.cursor),
                                                _ => None,
                                            }
                                        }
                                    };
                                    if let Some(color) = color {
                                        if let Some(reply) = roost_osc::format_color_query_response(
                                            n,
                                            (color.r, color.g, color.b),
                                        ) {
                                            session_for_replies.send_input(reply);
                                        }
                                    }
                                    continue;
                                }
                                // OSC 4 palette query — answer each index
                                // from libghostty's live palette (a prior
                                // `OSC 4;Ps;rgb:…` set wins), falling back to
                                // the theme palette on FFI error. Same per-tab
                                // serial reply channel as the OSC 10/11/12 path
                                // above. opentui (opencode in local mode + other
                                // TUIs) gates its color detection on a reply to
                                // `OSC 4;0;?`.
                                if let roost_osc::OscEvent::PaletteQuery(ref indices) = event {
                                    let palette = match terminal_for_drain.live_palette() {
                                        Ok(p) => p,
                                        Err(err) => {
                                            tracing::debug!(
                                                ?err,
                                                "live_palette failed; falling back to theme palette"
                                            );
                                            app_for_osc.theme.borrow().palette
                                        }
                                    };
                                    let mut reply = Vec::new();
                                    for &idx in indices {
                                        let color = palette[idx as usize];
                                        reply.extend_from_slice(
                                            &roost_osc::format_palette_query_response(
                                                idx,
                                                (color.r, color.g, color.b),
                                            ),
                                        );
                                    }
                                    if !reply.is_empty() {
                                        session_for_replies.send_input(reply);
                                    }
                                    continue;
                                }
                                app_for_osc.report_osc_event(tab_id, event);
                            }
                            terminal_for_drain.vt_write(&data);
                        }
                        TabOutput::Exit { status, reason } => {
                            tracing::info!(tab_id, status, %reason, "PTY exited");
                            // M9.5: close the page directly from the
                            // drain task on PTY exit — a unified close
                            // path through the close-page signal handler
                            // instead of round-tripping through the
                            // daemon's TabDeletedEvent. The
                            // `connect_close_page` handler installed
                            // in `add_project_ui` does the actual
                            // removal + CloseTab RPC.
                            //
                            // Deferred via `idle_add_local_once` so
                            // the close_page emission lands on a
                            // fresh main-loop iteration, outside the
                            // current `spawn_future_local` poll —
                            // GTK signal dispatch from inside an
                            // in-flight future was where the previous
                            // implementation (close_page in
                            // TabDeleted handler) silently failed to
                            // fire the close-page signal.
                            let app = app_for_exit.clone();
                            glib::idle_add_local_once(move || {
                                app.close_page_for_tab(project_id, tab_id);
                            });
                            break;
                        }
                        TabOutput::Error(reason) => {
                            tracing::warn!(reason, "PTY stream error");
                            // Same close-on-stream-error path. The
                            // error usually means the daemon dropped
                            // the StreamPty — no live PTY to close
                            // remotely, but we still need to remove
                            // the UI page so the user isn't left
                            // looking at a dead tab.
                            let app = app_for_exit.clone();
                            glib::idle_add_local_once(move || {
                                app.close_page_for_tab(project_id, tab_id);
                            });
                            break;
                        }
                    }
                }
            });
        });

        // Focus the new tab — unless a palette currently owns focus. An attach
        // landing while the palette is open (e.g. a late/background attach) must
        // not steal focus from it; the palette's dismiss refocuses the active
        // terminal, matching the `focus_active_terminal` ownership guard (#234).
        self.select_page_programmatic(&ui.tab_view, &page);
        if self.palette.borrow().is_none() {
            safe_grab_focus(terminal.widget());
        }
        drop(projects);
    }

    /// Dispatch a workspace event onto the GTK widget tree. M3b:
    /// these are emitted in-process by [`Workspace`] mutations and
    /// the only convergence path is roostctl → workspace → here.
    /// There is no cross-client coordination; we are the only
    /// consumer.
    fn handle_event(self: &Rc<Self>, event: WorkspaceEvent) {
        match event {
            WorkspaceEvent::ProjectCreated(project) => {
                self.add_project_ui(&project);
            }
            WorkspaceEvent::ProjectRenamed { project_id, name } => {
                let mut projects = self.projects.borrow_mut();
                if let Some(ui) = projects.get_mut(&project_id) {
                    ui.name = name.clone();
                    // M9: update the Stack's label child directly.
                    // If the user is mid-rename (Stack showing the
                    // entry), the label text still updates but the
                    // visible child stays on entry — "do not clobber
                    // in-progress edits."
                    ui.sidebar_label.set_text(&name);
                    if *self.active_project_id.borrow() == project_id {
                        self.window_title.set_title(&name);
                        self.window.set_title(Some(&name));
                    }
                }
            }
            WorkspaceEvent::ProjectDeleted { project_id } => {
                let mut projects = self.projects.borrow_mut();
                if let Some(ui) = projects.remove(&project_id) {
                    self.sidebar.remove(&ui.sidebar_row);
                    self.bar_stack.remove(&ui.tab_strip);
                    self.tab_stack.remove(
                        &self
                            .tab_stack
                            .child_by_name(&stack_name(project_id))
                            .expect("tab stack child for project"),
                    );
                }
                drop(projects);
                // Drop any inbox rows for the deleted project's tabs.
                let stale: Vec<i64> = self
                    .notification_inbox
                    .borrow()
                    .snapshot()
                    .iter()
                    .filter(|r| r.project_id == project_id)
                    .map(|r| r.tab_id)
                    .collect();
                if !stale.is_empty() {
                    let mut inbox = self.notification_inbox.borrow_mut();
                    for tab_id in stale {
                        inbox.remove(tab_id);
                    }
                    drop(inbox);
                    self.refresh_notif_badge();
                }
                let was_active = *self.active_project_id.borrow() == project_id;
                if was_active {
                    let projects = self.projects.borrow();
                    let fallback = pick_next_active_project(&projects);
                    drop(projects);
                    match fallback {
                        Some(pid) => {
                            *self.active_project_id.borrow_mut() = 0;
                            self.set_active_project(pid);
                        }
                        None => {
                            *self.active_project_id.borrow_mut() = 0;
                            self.window_title.set_title("Roost");
                            self.window_title.set_subtitle("");
                            self.window.close();
                        }
                    }
                }
            }
            WorkspaceEvent::TabOpened(tab) => {
                self.attach_existing_tab(tab);
            }
            WorkspaceEvent::TabClosed { tab_id } => {
                tracing::debug!(tab_id, "tab.closed event");
                if !self.close_tab_page(tab_id) {
                    tracing::debug!(tab_id, "tab.closed: no UI mapping (already closed locally)");
                }
                // A closed tab can't hold a pending notification — drop
                // its inbox row to preserve "row exists iff pending".
                self.notification_inbox.borrow_mut().remove(tab_id);
                self.refresh_notif_badge();
                // Drop any test-mode entries (no-op outside test mode).
                self.drop_test_mode_state(tab_id);
            }
            WorkspaceEvent::TabTitleChanged { tab_id, title } => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                        tab_ui.page.set_title(&title);
                    }
                    // Keep the pill label in sync (the AdwTabPage isn't shown).
                    if let Some(pill) = ui.pills.borrow().get(&tab_id) {
                        pill.label.set_label(&title);
                    }
                }
            }
            WorkspaceEvent::TabCwdChanged { tab_id, cwd } => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                        // Title handling is now done by the workspace itself:
                        // `set_tab_cwd` re-derives the title from cwd when
                        // `!user_titled` and emits a `TabTitleChanged` arm,
                        // which lands at the handler above. The prior local
                        // fallback that overwrote a `Tab N` placeholder with
                        // the tilde-abbreviated cwd was superseded by that
                        // model-side path (issue #196).
                        *tab_ui.cwd.borrow_mut() = cwd.clone();
                    }
                }
                drop(projects);
                self.refresh_window_subtitle();
            }
            WorkspaceEvent::TabNotification {
                tab_id,
                has_pending,
            } => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                        tab_ui.page.set_needs_attention(has_pending);
                    }
                    // Update the pill's trailing badge (Mac shows it on inactive
                    // notified tabs); the AdwTabPage isn't displayed.
                    if let Some(pill) = ui.pills.borrow().get(&tab_id) {
                        let active = ui
                            .tab_view
                            .selected_page()
                            .and_then(|p| parse_tab_id_from_page(&p))
                            == Some(tab_id);
                        pill.badge.set_visible(has_pending && !active);
                    }
                }
                drop(projects);
                // Inbox false-edge: clearing a tab's notification
                // (focus / prompt-submit / session-end / explicit clear)
                // drops its row + updates the badge, keeping the list ==
                // tab indicators == badge.
                if !has_pending {
                    self.notification_inbox.borrow_mut().remove(tab_id);
                    self.refresh_notif_badge();
                }
            }
            WorkspaceEvent::NotificationFired {
                tab_id,
                title,
                body,
            } => {
                // Live-inbox upsert: compose a project-forward row
                // ("<project> · <tab>", body) keyed by tab id for dedup
                // + jump. The `TabNotification` true-edge fires
                // alongside this and marks the tab; the false-edge
                // removes the row.
                if let Some((project_id, row_title)) = self.inbox_title_for(tab_id) {
                    self.notification_inbox
                        .borrow_mut()
                        .upsert(NotificationRecord::new(
                            tab_id,
                            project_id,
                            row_title,
                            body.clone(),
                        ));
                    self.refresh_notif_badge();
                }
                self.fire_desktop_notification(tab_id, &title, &body);
            }
            WorkspaceEvent::TabStateChanged { tab_id, state } => {
                let state = TabState::from_ipc(state);
                let mut affected_project: Option<i64> = None;
                {
                    let projects = self.projects.borrow();
                    for (project_id, ui) in projects.iter() {
                        let tabs = ui.tabs.borrow();
                        if let Some(tab_ui) = tabs.get(&tab_id) {
                            *tab_ui.state.borrow_mut() = state;
                            apply_indicator_icon(&tab_ui.page, state);
                            self.refresh_pill_indicators(*project_id, tab_id);
                            affected_project = Some(*project_id);
                            break;
                        }
                    }
                }
                if let Some(project_id) = affected_project {
                    self.refresh_rollup_for(project_id);
                }
                tracing::debug!(tab_id, ?state, "tab.state_changed applied");
            }
            WorkspaceEvent::HookActiveChanged { tab_id, active } => {
                let mut affected_project: Option<i64> = None;
                {
                    let projects = self.projects.borrow();
                    for (project_id, ui) in projects.iter() {
                        let tabs = ui.tabs.borrow();
                        if let Some(tab_ui) = tabs.get(&tab_id) {
                            *tab_ui.hook_active.borrow_mut() = active;
                            affected_project = Some(*project_id);
                            break;
                        }
                    }
                }
                if let Some(project_id) = affected_project {
                    self.refresh_rollup_for(project_id);
                }
                tracing::debug!(tab_id, hook_active = active, "hook_active.changed applied");
            }
            WorkspaceEvent::ActiveChanged { project_id, tab_id } => {
                if project_id != 0 {
                    self.set_active_project(project_id);
                }
                if tab_id != 0 {
                    let projects = self.projects.borrow();
                    if let Some(ui) = projects.get(&project_id) {
                        if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                            self.select_page_programmatic(&ui.tab_view, &tab_ui.page);
                        }
                    }
                }
            }
            WorkspaceEvent::TabsReordered { .. } | WorkspaceEvent::ProjectsReordered { .. } => {
                // M9 polish: reorder events are emitted by the
                // workspace post-mutation, but the GTK UI's own
                // drag-reorder path already updates the AdwTabBar /
                // sidebar inline before firing the `reorder_tabs` /
                // `reorder_projects` RPC. Cross-client convergence
                // (e.g. `roostctl tab reorder` from another shell)
                // is a follow-up slice — for now we drop these
                // events on the UI side rather than risk
                // double-applying a local reorder.
            }
            WorkspaceEvent::Resync(projects) => {
                self.reconcile_to_snapshot(projects);
            }
        }
    }

    /// Mark `tab_id` server-driven and close its `AdwTabPage`. The
    /// `connect_close_page` handler then drops the `TabUi` entry and
    /// skips the redundant `CloseTab` RPC. Returns whether a UI
    /// mapping was found. Shared by the `TabClosed` event arm and
    /// `reconcile_to_snapshot`.
    fn close_tab_page(self: &Rc<Self>, tab_id: i64) -> bool {
        let target = {
            let projects = self.projects.borrow();
            projects.values().find_map(|ui| {
                ui.tabs
                    .borrow()
                    .get(&tab_id)
                    .map(|t| (ui.tab_view.clone(), t.page.clone()))
            })
        };
        let Some((tab_view, page)) = target else {
            return false;
        };
        self.server_driven_closes.borrow_mut().insert(tab_id);
        tab_view.close_page(&page);
        true
    }

    /// Full-state reconcile after a broadcast `Lagged` (issue #79).
    /// Diffs the live UI against `snapshot` (ground truth) via the
    /// pure `reconcile::plan` and applies only the delta — surviving
    /// tabs keep their live `TerminalView` (no teardown/rebuild, so
    /// no scrollback loss).
    ///
    /// A tab present in the snapshot but absent from the UI (its
    /// `TabOpened` was among the dropped events) re-attaches via
    /// `PtySupervisor::subscribe_output`, so its terminal renders
    /// only from the subscribe point forward — earlier output is
    /// unrecoverable. Inherent to a degraded-recovery path; surviving
    /// tabs are unaffected.
    fn reconcile_to_snapshot(self: &Rc<Self>, snapshot: Vec<Project>) {
        // 1. Read current UI membership and build the plan. Include
        //    in-flight attaches so a mid-attach tab isn't double-added.
        let current = {
            let projects = self.projects.borrow();
            let mut project_ids = Vec::new();
            let mut tabs = Vec::new();
            for (pid, ui) in projects.iter() {
                project_ids.push(*pid);
                for tid in ui.tabs.borrow().keys() {
                    tabs.push((*tid, *pid));
                }
                for tid in ui.pending_attaches.borrow().iter() {
                    tabs.push((*tid, *pid));
                }
            }
            reconcile::CurrentView { project_ids, tabs }
        };
        let plan = reconcile::plan(&current, &snapshot);
        tracing::info!(
            add_projects = plan.projects_to_add.len(),
            remove_projects = plan.projects_to_remove.len(),
            add_tabs = plan.tabs_to_add.len(),
            remove_tabs = plan.tabs_to_remove.len(),
            "resync reconcile"
        );

        // 2. Remove stale projects (cascades their tabs).
        for pid in &plan.projects_to_remove {
            let removed = self.projects.borrow_mut().remove(pid);
            if let Some(ui) = removed {
                self.sidebar.remove(&ui.sidebar_row);
                self.bar_stack.remove(&ui.tab_strip);
                if let Some(child) = self.tab_stack.child_by_name(&stack_name(*pid)) {
                    self.tab_stack.remove(&child);
                }
            }
            if *self.active_project_id.borrow() == *pid {
                *self.active_project_id.borrow_mut() = 0;
            }
        }

        // 3. Remove stale tabs from surviving projects.
        for tid in &plan.tabs_to_remove {
            self.close_tab_page(*tid);
        }

        // 4. Add new projects, then 5. their/other new tabs.
        for pid in &plan.projects_to_add {
            if let Some(project) = snapshot.iter().find(|p| p.id == *pid) {
                self.add_project_ui(project);
            }
        }
        for tid in &plan.tabs_to_add {
            if let Some(tab) = snapshot
                .iter()
                .flat_map(|p| p.tabs.iter())
                .find(|t| t.id == *tid)
            {
                self.attach_existing_tab(tab.clone());
            }
        }

        // 5.5 Sync surviving projects' names (a dropped ProjectRenamed
        //     would otherwise leave the sidebar label stale). Mirrors
        //     the `ProjectRenamed` arm.
        {
            let active = *self.active_project_id.borrow();
            let mut projects = self.projects.borrow_mut();
            for project in &snapshot {
                if let Some(ui) = projects.get_mut(&project.id) {
                    if ui.name != project.name {
                        ui.name = project.name.clone();
                        ui.sidebar_label.set_text(&project.name);
                        if active == project.id {
                            self.window_title.set_title(&project.name);
                            self.window.set_title(Some(&project.name));
                        }
                    }
                }
            }
        }

        // 6. Update surviving tabs' fields from the snapshot.
        let mut affected: HashSet<i64> = HashSet::new();
        {
            let projects = self.projects.borrow();
            for project in &snapshot {
                let Some(ui) = projects.get(&project.id) else {
                    continue;
                };
                let tabs = ui.tabs.borrow();
                for tab in &project.tabs {
                    let Some(tab_ui) = tabs.get(&tab.id) else {
                        continue;
                    };
                    let label = if tab.title.is_empty() {
                        format!("Tab {}", tab.id)
                    } else {
                        tab.title.clone()
                    };
                    tab_ui.page.set_title(&label);
                    if let Some(pill) = ui.pills.borrow().get(&tab.id) {
                        pill.label.set_label(&label);
                    }
                    let state = TabState::from_ipc(tab.state);
                    *tab_ui.state.borrow_mut() = state;
                    apply_indicator_icon(&tab_ui.page, state);
                    *tab_ui.cwd.borrow_mut() = tab.cwd.clone();
                    tab_ui.page.set_needs_attention(tab.has_notification);
                    *tab_ui.hook_active.borrow_mut() = tab.hook_active;
                    self.refresh_pill_indicators(project.id, tab.id);
                    affected.insert(project.id);
                }
            }
        }
        for pid in &affected {
            self.refresh_rollup_for(*pid);
        }

        // 6.5 Reconcile inbox membership from the snapshot's
        //     notification flags — a dropped edge would otherwise leave
        //     the inbox + bell badge stale while the dot above is fixed.
        self.reconcile_inbox_from_snapshot(&snapshot);

        // 7. Reorder the sidebar to the snapshot order.
        self.apply_sidebar_order(&plan.project_order);

        // 8. Reorder surviving tab pills to the snapshot order.
        //    Best-effort: tabs still mid-attach append and settle on a
        //    later reorder. Suppress the `connect_page_reordered` echo
        //    so these programmatic moves don't fire ReorderTabs RPCs.
        *self.suppress_tab_reorder_echo.borrow_mut() = true;
        {
            let projects = self.projects.borrow();
            for (pid, tab_ids) in &plan.tab_order {
                let Some(ui) = projects.get(pid) else {
                    continue;
                };
                let tabs = ui.tabs.borrow();
                let mut pos = 0;
                for tid in tab_ids {
                    if let Some(tab_ui) = tabs.get(tid) {
                        ui.tab_view.reorder_page(&tab_ui.page, pos);
                        pos += 1;
                    }
                }
            }
        }
        *self.suppress_tab_reorder_echo.borrow_mut() = false;

        // 9. Restore active selection from the snapshot.
        if plan.active_project != 0 {
            self.set_active_project(plan.active_project);
            if plan.active_tab != 0 {
                let projects = self.projects.borrow();
                if let Some(ui) = projects.get(&plan.active_project) {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&plan.active_tab) {
                        self.select_page_programmatic(&ui.tab_view, &tab_ui.page);
                    }
                }
            }
        }
        self.refresh_window_subtitle();
    }

    /// Recompute the sidebar rollup CSS stripe for `project_id` from
    /// its tabs' current `(state, hook_active)`. Drops every
    /// `roost-rollup-*` class on the row first, then applies the
    /// active one (or none, if the rollup is `RollupState::None`).
    /// Called whenever a `TabStateChangedEvent` or
    /// `HookActiveChangedEvent` arrives for one of the project's tabs.
    fn refresh_rollup_for(self: &Rc<Self>, project_id: i64) {
        // Same try_borrow pattern as `active_tab_cwd`: this can be
        // invoked from a TabState / HookActive handler that already
        // sits inside a synchronously-fired AdwTabView signal, so a
        // bare `borrow()` can panic. Skipping the refresh in that
        // case is safe — the next state change will recompute.
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        let Ok(tabs) = ui.tabs.try_borrow() else {
            return;
        };
        let pairs: Vec<(TabState, bool)> = tabs
            .values()
            .map(|t| (*t.state.borrow(), *t.hook_active.borrow()))
            .collect();
        let rollup = project_rollup(&pairs);
        for cls in RollupState::all_classes() {
            ui.sidebar_row.remove_css_class(cls);
        }
        if let Some(cls) = rollup.css_class() {
            ui.sidebar_row.add_css_class(cls);
        }
    }

    /// Load `~/.config/roost/config.conf`, merge user keybinds with
    /// the Linux defaults, and install each `(Accel, action)` pair on
    /// the window's ShortcutController.
    fn install_keybinds(self: &Rc<Self>) {
        let cfg = RoostConfig::load_default();
        let bindings = canonicalize_bindings(default_bindings(), cfg.keybinds.clone(), |w| {
            tracing::warn!("keybind: {w}")
        });

        let controller = gtk4::ShortcutController::new();
        controller.set_scope(gtk4::ShortcutScope::Global);
        // M9.5: fire shortcuts during the CAPTURE phase so they get
        // first crack at the keystroke, BEFORE the focused
        // TerminalView's `EventControllerKey` consumes it. Default
        // is Bubble (post-widget), which let `Cmd+N` flow through
        // libghostty's key encoder into the PTY as a Kitty-protocol
        // escape sequence — the shell rendered the sequence as
        // garbled text and the keybind never fired. Matches the
        // Mac UI's `NSMenu performKeyEquivalent` priority (menu
        // fires before responder chain).
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);

        for (accel, action) in bindings {
            let trigger = build_shortcut_trigger(&accel);
            let app = self.clone();
            let cb = gtk4::CallbackAction::new(move |_widget, _args| {
                app.dispatch_action(action);
                glib::Propagation::Stop
            });
            let shortcut = gtk4::Shortcut::new(Some(trigger), Some(cb));
            controller.add_shortcut(shortcut);
        }

        self.window.add_controller(controller);
    }

    fn dispatch_action(self: &Rc<Self>, action: KeybindAction) {
        // While the palette is open it owns the keyboard; suppress every
        // other shortcut so e.g. Cmd+T can't fire underneath it. The
        // picker toggles themselves stay live (re-press is a no-op since
        // each `show_*` guards on an already-open palette).
        // GTK analog of the Swift app's `validateMenuItem` gate.
        let is_picker_toggle = matches!(
            action,
            KeybindAction::CommandPalette
                | KeybindAction::CommandLauncher
                | KeybindAction::CustomPalette
        );
        if !is_picker_toggle && self.palette.borrow().is_some() {
            return;
        }
        match action {
            KeybindAction::CommandPalette => self.show_command_palette(),
            KeybindAction::CommandLauncher => self.show_command_launcher(),
            KeybindAction::CustomPalette => self.show_custom_palette(),
            KeybindAction::NewTab => {
                let pid = *self.active_project_id.borrow();
                if pid == 0 {
                    return;
                }
                let app = self.clone();
                glib::spawn_future_local(async move {
                    if let Err(err) = app.open_new_tab_in(pid).await {
                        tracing::warn!(?err, "new_tab failed");
                    }
                });
            }
            KeybindAction::CloseTab => {
                let pid = *self.active_project_id.borrow();
                if let Some(tab_id) = self.active_tab_id(pid) {
                    self.close_tab_async(pid, tab_id);
                }
            }
            KeybindAction::NewProject => {
                let app = self.clone();
                glib::spawn_future_local(async move {
                    if let Err(err) = app.create_new_project().await {
                        tracing::warn!(?err, "new_project failed");
                    }
                });
            }
            KeybindAction::RenameProject => {
                let pid = *self.active_project_id.borrow();
                if pid != 0 {
                    self.begin_rename_project(pid);
                }
            }
            KeybindAction::RenameTab => {
                let pid = *self.active_project_id.borrow();
                if let Some(tab_id) = self.active_tab_id(pid) {
                    self.begin_rename_tab(pid, tab_id);
                }
            }
            KeybindAction::CloseProject => {
                let pid = *self.active_project_id.borrow();
                if pid != 0 {
                    self.confirm_and_delete_project(pid);
                }
            }
            KeybindAction::JumpToUnread => self.jump_to_unread(),
            KeybindAction::CycleTabPrev => self.cycle_tab(-1),
            KeybindAction::CycleTabNext => self.cycle_tab(1),
            KeybindAction::Copy => self.copy_active_selection(),
            KeybindAction::Paste => self.paste_into_active(),
            KeybindAction::ToggleSidebar => self.toggle_sidebar(),
            KeybindAction::FontIncrease => self.adjust_font_size(1.0),
            KeybindAction::FontDecrease => self.adjust_font_size(-1.0),
            KeybindAction::FontReset => {
                let baseline = self.font_size_pt.unwrap_or(DEFAULT_FONT_SIZE_PT);
                let current = *self.current_font_size_pt.borrow();
                // No-op when the live size already matches the baseline.
                // Skipping the apply call also skips its config write —
                // otherwise a stray Cmd+0 on an unconfigured user would
                // materialize `font-size = <default>` into a config that
                // never had a font-size line.
                if (current - baseline).abs() < 0.01 {
                    return;
                }
                *self.current_font_size_pt.borrow_mut() = baseline;
                self.apply_font_size_to_all(baseline);
            }
            KeybindAction::SwitchProject(n) => self.switch_project_by_index(n as usize),
            KeybindAction::SwitchTab(n) => self.switch_tab_by_index(n as usize),
            KeybindAction::Unbind => {
                // Reaches here only if the table somehow installed an
                // Unbind action; the canonicalizer should've dropped
                // the entry instead. Harmless no-op.
            }
        }
    }

    fn active_tab_id(self: &Rc<Self>, project_id: i64) -> Option<i64> {
        let projects = self.projects.borrow();
        let ui = projects.get(&project_id)?;
        let page = ui.tab_view.selected_page()?;
        parse_tab_id_from_page(&page)
    }

    fn cycle_tab(self: &Rc<Self>, delta: i32) {
        let pid = *self.active_project_id.borrow();
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&pid) else {
            return;
        };
        let pages = ui.tab_view.pages();
        let n = pages.n_items() as i32;
        if n == 0 {
            return;
        }
        let current = ui
            .tab_view
            .selected_page()
            .map(|p| ui.tab_view.page_position(&p))
            .unwrap_or(0);
        // Round-4 R2 (Mac parity): clamp at endpoints instead of
        // wrapping. Ctrl+Shift+[ on the first tab is a no-op;
        // Ctrl+Shift+] on the last tab is a no-op.
        let target_signed = current + delta;
        if target_signed < 0 || target_signed >= n {
            return;
        }
        let target = target_signed as u32;
        let tab_id = if let Some(obj) = pages.item(target) {
            if let Ok(page) = obj.downcast::<libadwaita::TabPage>() {
                self.select_page_programmatic(&ui.tab_view, &page);
                parse_tab_id_from_page(&page)
            } else {
                None
            }
        } else {
            None
        };
        drop(projects);
        // Explicit user gesture: sync the core's active tab. The guarded
        // set above suppresses the `selected-page` auto-sync, so do it here
        // (mirrors `switch_tab_by_index`).
        if let Some(tab_id) = tab_id {
            self.sync_core_active_tab(tab_id);
        }
    }

    fn copy_active_selection(self: &Rc<Self>) {
        if let Some(view) = self.active_terminal_view() {
            view.copy_selection_to_clipboard();
        }
    }

    fn paste_into_active(self: &Rc<Self>) {
        if let Some(view) = self.active_terminal_view() {
            view.paste_from_clipboard();
        }
    }

    /// Shift the live font size by `delta` points (clamped to a
    /// sane 6..72 range), then reapply to every TerminalView. The
    /// daemon doesn't know or care about font size — it's purely
    /// a UI concern — so no RPC fires.
    fn adjust_font_size(self: &Rc<Self>, delta: f64) {
        let new = {
            let mut size = self.current_font_size_pt.borrow_mut();
            let new = (*size + delta).clamp(6.0, 72.0);
            if (new - *size).abs() < 0.01 {
                return;
            }
            *size = new;
            new
        };
        self.apply_font_size_to_all(new);
    }

    /// Push `size_pt` to every TerminalView in every project. Reuses
    /// each view's existing `apply_font` path so cell metrics get
    /// remeasured + a redraw is queued automatically.
    fn apply_font_size_to_all(self: &Rc<Self>, size_pt: f64) {
        {
            let projects = self.projects.borrow();
            let family = self.font_family.borrow();
            for ui in projects.values() {
                let tabs = ui.tabs.borrow();
                for tab_ui in tabs.values() {
                    tab_ui.view.apply_font(family.as_deref(), Some(size_pt));
                }
            }
        }
        // Persist the new size back to ~/.config/roost/config.conf
        // so the next launch starts at the same zoom level. Font-size
        // changes are commit-only (no preview/revert distinction
        // like theme + font-family have), so the write is
        // unconditional here. Format whole values as integers
        // (`font-size = 14`, not `14.0`) to keep the file readable.
        if let Err(e) = write_back_font_size(size_pt) {
            tracing::warn!(
                error = %e,
                size = size_pt,
                "failed to persist font-size to config.conf"
            );
        }
    }

    /// Switch the active theme at runtime and broadcast it to every
    /// open terminal (all tabs, all projects). Not persisted on its
    /// own — used for both live preview (`preview_theme`) and revert
    /// (`revert_theme`); the commit-only persist lives in
    /// `commit_theme`. New tabs read `self.theme` at spawn, so both
    /// confirm and revert propagate forward. Mirrors the Mac UI's
    /// `setActiveTheme`.
    fn set_active_theme(self: &Rc<Self>, theme: Theme, name: String) {
        *self.active_theme_name.borrow_mut() = name;
        let projects = self.projects.borrow();
        for ui in projects.values() {
            let tabs = ui.tabs.borrow();
            for tab_ui in tabs.values() {
                tab_ui.view.set_theme(&theme);
            }
        }
        drop(projects);
        *self.theme.borrow_mut() = theme;
    }

    /// Commit the user's Enter on the theme sub-frame: make sure the
    /// live theme matches `name` (the highlight path normally does
    /// this, but a fast-Enter without ever moving the highlight is a
    /// no-preview path), then persist it to `~/.config/roost/config.conf`
    /// so the next launch picks the same theme. Preview/revert
    /// deliberately do NOT call this — they only mutate in-memory
    /// state.
    fn commit_theme(self: &Rc<Self>, name: &str) {
        if *self.active_theme_name.borrow() != name {
            self.set_active_theme(Theme::load_bundled(name), name.to_string());
        }
        if let Err(e) = write_back_theme(name) {
            tracing::warn!(error = %e, theme = name, "failed to persist theme to config.conf");
        }
    }

    /// Switch the active font family at runtime and broadcast it to
    /// every open terminal. Mirrors `set_active_theme`. Not persisted;
    /// `commit_font_family` does that. Passing `None` falls back to
    /// the compiled-in `DEFAULT_FONT_FAMILY` so a revert from a
    /// previewed family actually resets the rendered text — Pango's
    /// `TerminalView::apply_font(None, …)` deliberately no-ops the
    /// family slot to support size-only updates, so we must pass an
    /// explicit family string to revert visually.
    fn set_active_font_family(self: &Rc<Self>, family: Option<String>) {
        // Clone the to-be-applied family BEFORE moving into the
        // RefCell so the per-tab loop below can read it without
        // holding a live borrow across arbitrary view code (any
        // future apply_font side-effect that re-enters would
        // otherwise trip a BorrowError).
        let applied: String = family
            .as_deref()
            .unwrap_or(crate::cell_metrics::DEFAULT_FONT_FAMILY)
            .to_string();
        *self.font_family.borrow_mut() = family;
        let size = *self.current_font_size_pt.borrow();
        let projects = self.projects.borrow();
        for ui in projects.values() {
            let tabs = ui.tabs.borrow();
            for tab_ui in tabs.values() {
                tab_ui.view.apply_font(Some(&applied), Some(size));
            }
        }
    }

    /// Commit the user's Enter on the font sub-frame: ensure live
    /// state matches `name`, then persist to config. Counterpart to
    /// `commit_theme`.
    ///
    /// Preserves a comma-separated fallback chain (e.g. `"JetBrains
    /// Mono, Monospace"`) when the user confirms the chain's primary
    /// — the picker only exposes individual family names but a user
    /// may have hand-edited a fallback into config. The check is
    /// against the **at-open snapshot**, not the live preview value:
    /// if the user previewed another font and arrowed back, the
    /// live state is already the stripped primary, so comparing
    /// against the live value would still drop the fallback.
    fn commit_font_family(self: &Rc<Self>, name: &str) {
        let opened = self.font_family_at_open.borrow().clone().flatten();
        let opened_primary = opened
            .as_deref()
            .and_then(|s| s.split(',').map(str::trim).find(|t| !t.is_empty()));
        if opened_primary
            .map(|p| p.eq_ignore_ascii_case(name))
            .unwrap_or(false)
        {
            // No-op confirm: restore the opened chain to live state
            // (an interim preview may have replaced it with the bare
            // primary) and DON'T rewrite the file — it already has
            // the chain the user opened with.
            if *self.font_family.borrow() != opened {
                self.set_active_font_family(opened);
            }
            return;
        }
        self.set_active_font_family(Some(name.to_string()));
        if let Err(e) = write_back_font_family(name) {
            tracing::warn!(
                error = %e,
                family = name,
                "failed to persist font-family to config.conf"
            );
        }
    }

    // ----- Command palette (Cmd+Shift+P / Alt+Shift+P) -------------

    /// Open the command palette over the content overlay. Captures the
    /// theme active at open (for revert), builds the curated command
    /// frame with shortcut hints, and presents. No-op if already open.
    fn show_command_palette(self: &Rc<Self>) {
        if self.palette.borrow().is_some() {
            return;
        }
        *self.theme_name_at_open.borrow_mut() = Some(self.active_theme_name.borrow().clone());
        *self.font_family_at_open.borrow_mut() = Some(self.font_family.borrow().clone());

        // Reverse map (action → shortcut label) from the canonicalized
        // bindings, so each command row shows its keybind hint. First
        // accel per action wins; sorted for a deterministic choice when
        // an action has multiple triggers (e.g. font_increase).
        let cfg = RoostConfig::load_default();
        let mut bindings = canonicalize_bindings(default_bindings(), cfg.keybinds.clone(), |_| {})
            .into_iter()
            .collect::<Vec<_>>();
        bindings.sort_by(|a, b| {
            (a.0.modifiers.bits(), &a.0.key).cmp(&(b.0.modifiers.bits(), &b.0.key))
        });
        let mut reverse: HashMap<KeybindAction, Accel> = HashMap::new();
        for (accel, action) in bindings {
            reverse.entry(action).or_insert(accel);
        }
        // The two notification commands sit just below the Select Theme… /
        // Select Font… drill-ins; built dynamically so "View Notifications"
        // carries the live pending count.
        let mut items = command_items(|action| reverse.get(&action).and_then(accel_label));
        let notif = self.notification_command_items();
        let at = items
            .iter()
            .position(|i| i.id == PaletteCommands::SELECT_FONT_ID)
            .map_or(items.len(), |i| i + 1);
        items.splice(at..at, notif);
        // Surface the custom palette (script-backed providers) as a
        // drill-in row, but only when at least one provider is configured
        // — otherwise the command palette stays uncluttered.
        if !cfg.providers.is_empty() {
            let hint = reverse
                .get(&KeybindAction::CustomPalette)
                .and_then(accel_label);
            items.push(PaletteItem::new("custom_commands", "Custom Commands…").with_trailing(hint));
        }
        let root = PaletteFrame::new("commands", "Execute a command…", items);

        let weak = Rc::downgrade(self);
        let behavior = PaletteBehavior::new(move |item| match weak.upgrade() {
            Some(app) => app.confirm_palette_command(item),
            None => PaletteOutcome::Close,
        });

        let top_margin = self.palette_top_margin();
        let weak_dismiss = Rc::downgrade(self);
        let overlay = PaletteOverlay::present(
            &self.content_overlay,
            root,
            behavior,
            top_margin,
            move || {
                if let Some(app) = weak_dismiss.upgrade() {
                    app.dismiss_palette();
                }
            },
        );
        *self.palette.borrow_mut() = Some(overlay);
    }

    // MARK: notification inbox

    /// Open the palette directly on the notifications list (the
    /// HeaderBar bell). Same surface as "View Notifications", just
    /// entered as the root frame so Esc closes rather than popping.
    fn show_notifications_palette(self: &Rc<Self>) {
        if self.palette.borrow().is_some() {
            return;
        }
        let root = self.notifications_frame();
        let behavior = self.notifications_behavior();
        let top_margin = self.palette_top_margin();
        let weak_dismiss = Rc::downgrade(self);
        let overlay = PaletteOverlay::present(
            &self.content_overlay,
            root,
            behavior,
            top_margin,
            move || {
                if let Some(app) = weak_dismiss.upgrade() {
                    app.dismiss_palette();
                }
            },
        );
        *self.palette.borrow_mut() = Some(overlay);
    }

    // MARK: command launcher (Cmd+Shift+T / Alt+Shift+T)

    /// Open the custom command launcher directly on the configured
    /// command list. Presented as the root frame (so Esc closes), the
    /// same surface as `show_notifications_palette`. No-op if a palette
    /// is already open.
    fn show_command_launcher(self: &Rc<Self>) {
        if self.palette.borrow().is_some() {
            return;
        }
        // Snapshot the config once and thread it through both the frame
        // and the behavior, so the row the user sees and the command that
        // launches are the same entry even if config.conf changes while
        // the picker is open. Reloading on each open still picks up edits
        // without a restart (matching `show_command_palette`).
        let commands = RoostConfig::load_default().commands;
        let root = self.launcher_frame(&commands);
        let behavior = self.launcher_behavior(commands);
        let top_margin = self.palette_top_margin();
        let weak_dismiss = Rc::downgrade(self);
        let overlay = PaletteOverlay::present(
            &self.content_overlay,
            root,
            behavior,
            top_margin,
            move || {
                if let Some(app) = weak_dismiss.upgrade() {
                    app.dismiss_palette();
                }
            },
        );
        *self.palette.borrow_mut() = Some(overlay);
    }

    /// Build the launcher frame from a config snapshot's `command =`
    /// list. An empty list yields the "No commands configured" sentinel.
    fn launcher_frame(self: &Rc<Self>, commands: &[CustomCommand]) -> PaletteFrame {
        let items = custom_command::launcher_items(commands);
        PaletteFrame::new("launcher", "Run a command…", items)
    }

    /// Confirm on a launcher row → spawn that command in a new tab. The
    /// row id's index is resolved against the same `commands` snapshot the
    /// frame was built from. The `launch:none` sentinel (and any stale id)
    /// is a no-op (stay open). The launch is deferred to an idle tick so
    /// the palette tears down before the new tab is opened + focused
    /// (mirrors the notification jump).
    fn launcher_behavior(self: &Rc<Self>, commands: Vec<CustomCommand>) -> PaletteBehavior {
        let weak = Rc::downgrade(self);
        PaletteBehavior::new(move |item| {
            let Some(app) = weak.upgrade() else {
                return PaletteOutcome::Close;
            };
            match custom_command::launch_index(&item.id) {
                Some(index) if index < commands.len() => {
                    let cmd = commands[index].clone();
                    let weak2 = Rc::downgrade(&app);
                    glib::idle_add_local_once(move || {
                        if let Some(app) = weak2.upgrade() {
                            app.launch_command(&cmd);
                        }
                    });
                    PaletteOutcome::Close
                }
                _ => PaletteOutcome::None, // sentinel / stale id
            }
        })
    }

    /// Spawn `cmd` in a new tab of the active project, in the active tab's
    /// live cwd, running it through the user's login shell. Auto-close on
    /// exit + the non-sticky title are handled by the existing tab
    /// infrastructure — everything else rides in the argv built by
    /// `custom_command::launch_argv`.
    fn launch_command(self: &Rc<Self>, cmd: &CustomCommand) {
        let pid = *self.active_project_id.borrow();
        if pid == 0 {
            return;
        }
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let argv = custom_command::launch_argv(&shell, cmd);
        let cwd = self.launch_cwd(pid);
        let title = cmd.title.clone();
        let app = self.clone();
        glib::spawn_future_local(async move {
            // `open_tab_in_with` → `attach_existing_tab` selects + focuses
            // the new page, so the launched tab is brought forward.
            if let Err(err) = app.open_tab_in_with(pid, &cwd, &title, &argv).await {
                tracing::warn!(?err, "command launcher: open_tab failed");
            }
        });
    }

    // MARK: custom palette (provider scripts) — Cmd+Shift+E / Alt+Shift+E

    /// Open the custom palette on the configured provider list
    /// (`provider =` entries + discovered scripts), presented as the root
    /// frame like the launcher. No-op if a palette is already open.
    fn show_custom_palette(self: &Rc<Self>) {
        if self.palette.borrow().is_some() {
            return;
        }
        let providers = RoostConfig::load_default().providers;
        let root = self.provider_list_frame(&providers);
        let behavior = self.provider_list_behavior(providers);
        let top_margin = self.palette_top_margin();
        let weak_dismiss = Rc::downgrade(self);
        let overlay = PaletteOverlay::present(
            &self.content_overlay,
            root,
            behavior,
            top_margin,
            move || {
                if let Some(app) = weak_dismiss.upgrade() {
                    app.dismiss_palette();
                }
            },
        );
        *self.palette.borrow_mut() = Some(overlay);
    }

    /// The provider-list frame: one row per configured provider (or the
    /// "No providers configured" sentinel when empty).
    fn provider_list_frame(self: &Rc<Self>, providers: &[provider::Provider]) -> PaletteFrame {
        PaletteFrame::new(
            "custom",
            "Custom commands…",
            provider::provider_items(providers),
        )
    }

    /// Confirm on a provider row → run its `list` phase off-main, then
    /// drill into the resulting rows. Returns `None` (stay open) — the
    /// result frame is pushed asynchronously once the script returns. The
    /// `provider:none` sentinel and any stale id are a no-op.
    fn provider_list_behavior(
        self: &Rc<Self>,
        providers: Vec<provider::Provider>,
    ) -> PaletteBehavior {
        let weak = Rc::downgrade(self);
        PaletteBehavior::new(move |item| {
            let Some(app) = weak.upgrade() else {
                return PaletteOutcome::Close;
            };
            match provider::provider_index(&item.id) {
                Some(index) if index < providers.len() => {
                    let provider = providers[index].clone();
                    let weak2 = Rc::downgrade(&app);
                    glib::spawn_future_local(async move {
                        if let Some(app) = weak2.upgrade() {
                            app.open_provider_list(provider).await;
                        }
                    });
                    PaletteOutcome::None
                }
                _ => PaletteOutcome::None, // sentinel / stale id
            }
        })
    }

    /// Run a provider's `list` phase and push its rows as a sub-frame. If
    /// the palette closed while the script ran, the result is dropped.
    async fn open_provider_list(self: &Rc<Self>, provider: provider::Provider) {
        let want = self.palette.borrow().as_ref().map(|o| o.id());
        let req = self.provider_req.get().wrapping_add(1);
        self.provider_req.set(req);
        let result = self
            .run_provider(&provider, provider::Phase::List, None)
            .await;
        // Apply only if (a) no newer provider run superseded this one, and
        // (b) the palette that asked for it is still on screen — a dismiss
        // + reopen (e.g. palette.present) during the spawn must not be
        // clobbered by a stale result.
        if self.provider_req.get() != req {
            return;
        }
        let driver = match self.palette.borrow().as_ref() {
            Some(o) if Some(o.id()) == want => o.driver(),
            _ => return,
        };
        self.push_provider_result(&driver, provider, result);
    }

    /// Confirm on a provider item → run its `activate` phase with the
    /// selected id. The script acts (usually via `$ROOST_SOCKET`); its
    /// stdout may drill in (more rows) or be empty (close the palette).
    fn provider_item_behavior(self: &Rc<Self>, provider: provider::Provider) -> PaletteBehavior {
        let weak = Rc::downgrade(self);
        PaletteBehavior::new(move |item| {
            // Non-actionable rows (the overflow hint, a provider's
            // `"actionable":false` row) never reach here — `confirm`
            // skips them before invoking the behavior.
            let Some(app) = weak.upgrade() else {
                return PaletteOutcome::Close;
            };
            let provider = provider.clone();
            let selected = item.id.clone();
            let weak2 = Rc::downgrade(&app);
            glib::spawn_future_local(async move {
                if let Some(app) = weak2.upgrade() {
                    app.activate_provider_item(provider, selected).await;
                }
            });
            PaletteOutcome::None
        })
    }

    /// Run `activate` for the chosen row, then drill in (more rows) or
    /// close (side-effect-only / empty stdout).
    async fn activate_provider_item(
        self: &Rc<Self>,
        provider: provider::Provider,
        selected_id: String,
    ) {
        let want = self.palette.borrow().as_ref().map(|o| o.id());
        let req = self.provider_req.get().wrapping_add(1);
        self.provider_req.set(req);
        let result = self
            .run_provider(&provider, provider::Phase::Activate, Some(selected_id))
            .await;
        // Same stale-result guard as `open_provider_list` (newer run +
        // palette session).
        if self.provider_req.get() != req {
            return;
        }
        let driver = match self.palette.borrow().as_ref() {
            Some(o) if Some(o.id()) == want => o.driver(),
            _ => return,
        };
        // Empty success = "done, close"; anything else (rows or error)
        // drills in / shows the error row.
        let close_only = matches!(&result, Ok(o) if o.items.is_empty());
        if close_only {
            driver.dismiss();
        } else {
            self.push_provider_result(&driver, provider, result);
        }
    }

    /// Push a provider's parsed output (or an error row) as a sub-frame.
    fn push_provider_result(
        self: &Rc<Self>,
        driver: &crate::palette_ui::PaletteDriver,
        provider: provider::Provider,
        result: Result<provider::ProviderOutput, String>,
    ) {
        match result {
            Ok(output) => {
                let placeholder = if output.placeholder.is_empty() {
                    format!("{}…", provider.title)
                } else {
                    output.placeholder.clone()
                };
                let items = provider::output_palette_items(&output, provider.limit);
                let frame = PaletteFrame::new(Self::next_provider_frame_id(), placeholder, items);
                let behavior = self.provider_item_behavior(provider);
                driver.push(frame, behavior);
            }
            Err(msg) => {
                tracing::warn!(provider = %provider.label, error = %msg, "provider run failed");
                let frame = PaletteFrame::new(
                    Self::next_provider_frame_id(),
                    "Provider error",
                    vec![PaletteItem::new("provider:_error", "Provider failed")
                        .with_subtitle(Some(msg))],
                );
                driver.push(frame, PaletteBehavior::new(|_| PaletteOutcome::None));
            }
        }
    }

    /// Assemble the active-tab context handed to a provider run.
    fn provider_context(self: &Rc<Self>, selected_id: Option<String>) -> provider::ProviderContext {
        let pid = *self.active_project_id.borrow();
        let active_tab_id = self.active_tab_id(pid);
        let active_cwd = if pid != 0 {
            self.launch_cwd(pid)
        } else {
            String::new()
        };
        let active_title = active_tab_id
            .and_then(|tid| {
                self.projects.borrow().get(&pid).and_then(|ui| {
                    ui.tabs
                        .borrow()
                        .get(&tid)
                        .map(|t| t.page.title().to_string())
                })
            })
            .unwrap_or_default();
        let socket = self
            .client
            .borrow()
            .as_ref()
            .map(|c| c.socket_path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let query = self
            .palette
            .borrow()
            .as_ref()
            .map(|o| o.driver().snapshot().query)
            .unwrap_or_default();
        // Roost's own roostctl, resolved as a sibling of the running
        // binary (`/usr/bin/roost` → `/usr/bin/roostctl` from the .deb;
        // `target/debug/roost` → `…/roostctl` in dev). Canonicalize first
        // so a symlinked launch resolves to the real install dir, and
        // require the executable bit (mirrors provider discovery in
        // config.rs, and matches the Mac `isExecutableFile` check). Lets a
        // provider shell out without roostctl on PATH.
        let roostctl = std::env::current_exe()
            .ok()
            .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe))
            .and_then(|exe| exe.parent().map(|d| d.join("roostctl")))
            .filter(|p| {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(p)
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            })
            .map(|p| p.to_string_lossy().into_owned());
        provider::ProviderContext {
            socket,
            query,
            selected_id,
            active_tab_id,
            active_project_id: if pid != 0 { Some(pid) } else { None },
            active_cwd,
            active_title,
            roostctl,
        }
    }

    /// Run one provider phase as a subprocess (off the GTK main thread,
    /// via `rt`, with the provider's timeout) and parse its stdout.
    async fn run_provider(
        self: &Rc<Self>,
        provider: &provider::Provider,
        phase: provider::Phase,
        selected_id: Option<String>,
    ) -> Result<provider::ProviderOutput, String> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let ctx = self.provider_context(selected_id);
        let run = provider.run.clone();
        let timeout = provider.timeout_secs;
        let shell_interpret = provider.shell_interpret;
        self.rt
            .spawn(async move {
                Self::run_provider_subprocess(shell, run, shell_interpret, phase, ctx, timeout)
                    .await
            })
            .await
            .unwrap_or_else(|e| Err(format!("provider task failed: {e}")))
    }

    /// The blocking half (runs on a `tokio` worker, not the GTK thread):
    /// spawn the provider command, write the context JSON to its stdin,
    /// wait with a timeout, and parse stdout. `kill_on_drop` ensures a
    /// timed-out child is reaped.
    async fn run_provider_subprocess(
        shell: String,
        run: String,
        shell_interpret: bool,
        phase: provider::Phase,
        ctx: provider::ProviderContext,
        timeout_secs: u64,
    ) -> Result<provider::ProviderOutput, String> {
        use std::process::Stdio;
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        let argv = provider::invocation_argv(&shell, &run, shell_interpret, phase);
        let env = provider::invocation_env(phase, &ctx);
        let stdin_json = provider::invocation_stdin(phase, &ctx);

        let has_roostctl = env.iter().any(|(k, _)| k == "ROOST_ROOSTCTL");
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // If Roost couldn't resolve its own roostctl, strip any inherited
        // ROOST_ROOSTCTL so the script's `${ROOST_ROOSTCTL:-roostctl}` PATH
        // fallback actually fires (don't leak a stale parent value).
        if !has_roostctl {
            cmd.env_remove("ROOST_ROOSTCTL");
        }
        // Only set the cwd if it still exists — the active tab's dir may
        // have been removed; don't let that fail the whole spawn (inherit
        // Roost's cwd instead).
        if !ctx.active_cwd.is_empty() && std::path::Path::new(&ctx.active_cwd).is_dir() {
            cmd.current_dir(&ctx.active_cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| format!("spawn provider: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_json.as_bytes()).await;
            // stdin drops here → EOF for the child.
        }
        let output =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
                .await
            {
                Err(_) => return Err(format!("provider timed out after {timeout_secs}s")),
                Ok(Err(e)) => return Err(format!("provider io error: {e}")),
                Ok(Ok(o)) => o,
            };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail = stderr.lines().last().unwrap_or("").trim().to_string();
            let code = output.status.code().unwrap_or(-1);
            return Err(if tail.is_empty() {
                format!("provider exited with status {code}")
            } else {
                format!("provider exited {code}: {tail}")
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Activate is a side-effect phase: ignore non-JSON stdout (e.g. the
        // tab id `roostctl tab open` prints) so it doesn't fail parsing.
        match phase {
            provider::Phase::Activate => provider::parse_activate_output(&stdout),
            provider::Phase::List => provider::parse_provider_output(&stdout),
        }
    }

    /// Monotonic, process-unique id for a pushed provider sub-frame, so
    /// nested drill-ins don't share a behaviors-map key (which a pop
    /// would otherwise remove for both levels).
    fn next_provider_frame_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        format!("provider:items:{}", SEQ.fetch_add(1, Ordering::Relaxed))
    }

    /// The active tab's live cwd (OSC 7-tracked) in `project_id`, or ""
    /// when unknown — the open-tab path then falls back to the project
    /// cwd → `$HOME` → `/`.
    fn active_tab_cwd(self: &Rc<Self>, project_id: i64) -> String {
        let Some(tab_id) = self.active_tab_id(project_id) else {
            return String::new();
        };
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return String::new();
        };
        let tabs = ui.tabs.borrow();
        tabs.get(&tab_id)
            .map(|t| t.cwd.borrow().clone())
            .unwrap_or_default()
    }

    /// The two notification root commands. Built dynamically (not in
    /// `PaletteCommands::SPECS`) so "View Notifications" can show the
    /// live pending count.
    fn notification_command_items(self: &Rc<Self>) -> Vec<PaletteItem> {
        let count = self.notification_inbox.borrow().count();
        let view_title = if count > 0 {
            format!("View Notifications ({count})")
        } else {
            "View Notifications".to_string()
        };
        vec![
            PaletteItem::new(PaletteCommands::VIEW_NOTIFICATIONS_ID, view_title),
            PaletteItem::new(
                PaletteCommands::CLEAR_NOTIFICATIONS_ID,
                "Clear All Notifications",
            ),
        ]
    }

    /// Build the notifications sub-frame from the live inbox snapshot.
    /// Each row encodes its tab id as `notif:<id>` (parsed on confirm to
    /// jump). An empty inbox shows a single non-actionable row.
    fn notifications_frame(self: &Rc<Self>) -> PaletteFrame {
        let inbox = self.notification_inbox.borrow();
        let items: Vec<PaletteItem> = if inbox.is_empty() {
            vec![PaletteItem::new("notif:none", "No notifications")]
        } else {
            inbox
                .snapshot()
                .iter()
                .map(|r| {
                    let trailing = relative_time(r.at.elapsed().as_secs());
                    let subtitle = if r.body.is_empty() {
                        None
                    } else {
                        Some(r.body.clone())
                    };
                    PaletteItem::new(format!("notif:{}", r.tab_id), r.title.clone())
                        .with_subtitle(subtitle)
                        .with_trailing(Some(trailing))
                })
                .collect()
        };
        PaletteFrame::new("notifications", "Jump to a notification…", items)
    }

    /// Confirm on a notification row → focus that project + tab (which
    /// clears the tab's notification → drops the row). The "No
    /// notifications" sentinel is a no-op. Deferred to an idle tick so
    /// the palette tears down before the focus/present runs (mirrors the
    /// command path).
    fn notifications_behavior(self: &Rc<Self>) -> PaletteBehavior {
        let weak = Rc::downgrade(self);
        PaletteBehavior::new(move |item| {
            let Some(app) = weak.upgrade() else {
                return PaletteOutcome::Close;
            };
            match notif_tab_id(&item.id) {
                Some(tab_id) => {
                    let weak2 = Rc::downgrade(&app);
                    glib::idle_add_local_once(move || {
                        if let Some(app) = weak2.upgrade() {
                            app.focus_tab_by_id(tab_id);
                        }
                    });
                    PaletteOutcome::Close
                }
                None => PaletteOutcome::None, // "No notifications" sentinel
            }
        })
    }

    /// "Clear All Notifications": clear each pending tab's notification
    /// through the workspace. Each clear emits the `TabNotification`
    /// false-edge, which is the single source of truth — its handler
    /// drops the inbox row, refreshes the header badge, and clears the
    /// tab's needs-attention. Driving removal only off that edge (no
    /// parallel local clear) keeps list == indicators == badge even if
    /// a clear fails: that tab stays pending rather than the UI
    /// desyncing from the workspace.
    fn clear_all_notifications(self: &Rc<Self>) {
        let tab_ids = self.notification_inbox.borrow().tab_ids();
        if let Some(client) = self.client.borrow().clone() {
            for tab_id in &tab_ids {
                let _ = client.workspace.set_tab_has_notification(*tab_id, false);
            }
        }
    }

    /// Reconcile inbox membership from a workspace snapshot during
    /// resync. The live `NotificationFired` / `TabNotification` edges
    /// normally drive the inbox, but the broadcast lag that triggers a
    /// resync can drop an edge — leaving the inbox + bell badge stale
    /// while `reconcile_to_snapshot` corrects the per-tab dot. Bring
    /// membership back in line with the authoritative `has_notification`
    /// flags. Best-effort: the message body rode the lost event, so a
    /// recovered row shows its title only.
    fn reconcile_inbox_from_snapshot(self: &Rc<Self>, snapshot: &[Project]) {
        let pending: HashSet<i64> = snapshot
            .iter()
            .flat_map(|p| p.tabs.iter())
            .filter(|t| t.has_notification)
            .map(|t| t.id)
            .collect();
        // Drop rows for tabs no longer pending (cleared or closed).
        let stale: Vec<i64> = self
            .notification_inbox
            .borrow()
            .tab_ids()
            .into_iter()
            .filter(|id| !pending.contains(id))
            .collect();
        {
            let mut inbox = self.notification_inbox.borrow_mut();
            for id in stale {
                inbox.remove(id);
            }
        }
        // Add rows for pending tabs missing from the inbox.
        let existing: HashSet<i64> = self
            .notification_inbox
            .borrow()
            .tab_ids()
            .into_iter()
            .collect();
        for tab_id in pending {
            if !existing.contains(&tab_id) {
                if let Some((project_id, title)) = self.inbox_title_for(tab_id) {
                    self.notification_inbox
                        .borrow_mut()
                        .upsert(NotificationRecord::new(
                            tab_id,
                            project_id,
                            title,
                            String::new(),
                        ));
                }
            }
        }
        self.refresh_notif_badge();
    }

    /// Compose the inbox row title for `tab_id`: `(project_id,
    /// "<project> · <tab>")`.
    ///
    /// Prefers the live UI mapping (its AdwTabPage title reflects OSC
    /// title changes). Falls back to the workspace when the UI hasn't
    /// registered the tab's `TabUi` yet — `attach_existing_tab`
    /// populates `ui.tabs` only after its async block resolves, so a
    /// notification firing in that window would otherwise be dropped
    /// from the inbox (breaking the "row iff pending" invariant). The
    /// workspace always has the tab here (notify requires it to exist).
    fn inbox_title_for(self: &Rc<Self>, tab_id: i64) -> Option<(i64, String)> {
        {
            let projects = self.projects.borrow();
            for (project_id, ui) in projects.iter() {
                if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                    let tab_label = tab_ui.page.title().to_string();
                    return Some((*project_id, compose_title(&ui.name, &tab_label)));
                }
            }
        }
        let client = self.client.borrow().clone()?;
        let tab = client.workspace.tab(tab_id).ok()?;
        let project_name = client
            .workspace
            .snapshot()
            .into_iter()
            .find(|p| p.id == tab.project_id)
            .map(|p| p.name)
            .unwrap_or_default();
        let tab_label = if !tab.title.is_empty() {
            tab.title
        } else if !tab.cwd.is_empty() {
            tilde_abbreviate(&tab.cwd)
        } else {
            "Tab".to_string()
        };
        Some((tab.project_id, compose_title(&project_name, &tab_label)))
    }

    /// Mirror the inbox count onto the HeaderBar bell's badge. Hidden at
    /// zero so the bell reads as "nothing pending".
    fn refresh_notif_badge(self: &Rc<Self>) {
        let count = self.notification_inbox.borrow().count();
        if count > 0 {
            self.notif_badge.set_text(&count.to_string());
            self.notif_badge.set_visible(true);
        } else {
            self.notif_badge.set_visible(false);
        }
    }

    /// Top margin for the palette card. The tab strip now lives in the
    /// toolbar top bar, above the content overlay the palette floats in, so
    /// the card only needs the visual gap from the content top.
    /// Top inset for the palette card. The card floats in `content_overlay`,
    /// whose top edge is now the tab band (the strip moved into the content
    /// column), so clear the band's live height before adding the gap — this
    /// keeps the card pinned just under the tabs, as it was when the strip
    /// lived in the toolbar's top bar (above the overlay). Falls back to a bare
    /// `TOP_GAP` if the band isn't allocated yet (height 0).
    fn palette_top_margin(&self) -> i32 {
        self.tab_scroller.height() + TOP_GAP
    }

    /// Palette teardown callback: clear the handle + the captured
    /// open-theme, then return focus to the active terminal.
    fn dismiss_palette(self: &Rc<Self>) {
        *self.palette.borrow_mut() = None;
        *self.theme_name_at_open.borrow_mut() = None;
        *self.font_family_at_open.borrow_mut() = None;
        self.focus_active_terminal();
    }

    /// Confirm a root-frame command. `select_theme` drills into the
    /// theme list; every other id is a `KeybindAction` dispatched
    /// through the same path as its shortcut — deferred to an idle tick
    /// so the palette tears down (and its focus-out fires) *before* the
    /// command runs, letting focus-grabbing commands (rename) win.
    fn confirm_palette_command(self: &Rc<Self>, item: &PaletteItem) -> PaletteOutcome {
        if item.id == PaletteCommands::SELECT_THEME_ID {
            return PaletteOutcome::Push(self.theme_frame(), self.theme_behavior());
        }
        if item.id == PaletteCommands::SELECT_FONT_ID {
            return PaletteOutcome::Push(self.font_frame(), self.font_behavior());
        }
        if item.id == PaletteCommands::VIEW_NOTIFICATIONS_ID {
            return PaletteOutcome::Push(self.notifications_frame(), self.notifications_behavior());
        }
        if item.id == PaletteCommands::CLEAR_NOTIFICATIONS_ID {
            self.clear_all_notifications();
            return PaletteOutcome::Close;
        }
        if item.id == "custom_commands" {
            // The dynamic drill-in surfaced in the command palette when
            // providers are configured. Reload providers fresh (matches
            // the launcher's reload-on-open) and push the custom frame.
            let providers = RoostConfig::load_default().providers;
            return PaletteOutcome::Push(
                self.provider_list_frame(&providers),
                self.provider_list_behavior(providers),
            );
        }
        let id = item.id.clone();
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(app) = weak.upgrade() {
                app.run_command(&id);
            }
        });
        PaletteOutcome::Close
    }

    /// Dispatch a palette command id through the keybind path. Runs
    /// after the palette has closed, so the open-palette gate in
    /// `dispatch_action` is already clear.
    fn run_command(self: &Rc<Self>, id: &str) {
        match KeybindAction::from_name(id) {
            Some(action) => self.dispatch_action(action),
            None => tracing::warn!(id, "palette run_command: unknown id"),
        }
    }

    /// Build the theme sub-frame: bundled names verbatim, pre-selecting
    /// the live theme.
    fn theme_frame(self: &Rc<Self>) -> PaletteFrame {
        let names = Theme::bundled_names();
        let active = self.active_theme_name.borrow().clone();
        let selection = names.iter().position(|n| *n == active).unwrap_or(0);
        let items = names
            .into_iter()
            .map(|n| PaletteItem::new(n.clone(), n))
            .collect();
        PaletteFrame::new("themes", "Select a theme…", items).with_selection(selection)
    }

    /// Theme sub-frame behavior: arrowing previews live, Enter keeps
    /// + persists, Esc/dismiss reverts. The persist is on Enter only
    /// — arrowing through every theme would otherwise thrash the
    /// config file.
    fn theme_behavior(self: &Rc<Self>) -> PaletteBehavior {
        let weak_highlight = Rc::downgrade(self);
        let weak_confirm = Rc::downgrade(self);
        let weak_cancel = Rc::downgrade(self);
        PaletteBehavior::new(move |item| {
            if let Some(app) = weak_confirm.upgrade() {
                app.commit_theme(&item.id);
            }
            PaletteOutcome::Close
        })
        .on_highlight(move |item| {
            if let Some(app) = weak_highlight.upgrade() {
                app.preview_theme(&item.id);
            }
        })
        .on_cancel(move || {
            if let Some(app) = weak_cancel.upgrade() {
                app.revert_theme();
            }
        })
    }

    /// Apply `name` to every terminal as a live preview (skip if it's
    /// already active).
    fn preview_theme(self: &Rc<Self>, name: &str) {
        if *self.active_theme_name.borrow() == name {
            return;
        }
        self.set_active_theme(Theme::load_bundled(name), name.to_string());
    }

    /// Revert to the theme captured when the palette opened.
    fn revert_theme(self: &Rc<Self>) {
        let Some(name) = self.theme_name_at_open.borrow().clone() else {
            return;
        };
        if *self.active_theme_name.borrow() == name {
            return;
        }
        self.set_active_theme(Theme::load_bundled(&name), name);
    }

    /// Build the font sub-frame: curated programming fonts first
    /// (filtered to those Pango reports installed), then every other
    /// installed monospace family alphabetically. Pre-selects the
    /// live family.
    fn font_frame(self: &Rc<Self>) -> PaletteFrame {
        let families = self.available_font_families();
        let active = self
            .font_family
            .borrow()
            .clone()
            .unwrap_or_else(|| crate::cell_metrics::DEFAULT_FONT_FAMILY.to_string());
        // Match against the primary entry of a comma list (e.g. the
        // default `"JetBrains Mono, Monospace"` should pre-select
        // "JetBrains Mono"). Fall back to row 0 if not found.
        let primary = active
            .split(',')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or("");
        let selection = families
            .iter()
            .position(|n| n.eq_ignore_ascii_case(primary))
            .unwrap_or(0);
        let items = families
            .into_iter()
            .map(|n| PaletteItem::new(n.clone(), n))
            .collect();
        PaletteFrame::new("fonts", "Select a font…", items).with_selection(selection)
    }

    /// Font sub-frame behavior: mirrors `theme_behavior` 1:1. Arrowing
    /// previews live (no persist), Enter persists, Esc reverts.
    fn font_behavior(self: &Rc<Self>) -> PaletteBehavior {
        let weak_highlight = Rc::downgrade(self);
        let weak_confirm = Rc::downgrade(self);
        let weak_cancel = Rc::downgrade(self);
        PaletteBehavior::new(move |item| {
            if let Some(app) = weak_confirm.upgrade() {
                app.commit_font_family(&item.id);
            }
            PaletteOutcome::Close
        })
        .on_highlight(move |item| {
            if let Some(app) = weak_highlight.upgrade() {
                app.preview_font_family(&item.id);
            }
        })
        .on_cancel(move || {
            if let Some(app) = weak_cancel.upgrade() {
                app.revert_font_family();
            }
        })
    }

    /// Apply `name` to every terminal as a live preview (skip if it's
    /// already active).
    fn preview_font_family(self: &Rc<Self>, name: &str) {
        let already = self
            .font_family
            .borrow()
            .as_deref()
            .map(|s| s == name)
            .unwrap_or(false);
        if already {
            return;
        }
        self.set_active_font_family(Some(name.to_string()));
    }

    /// Revert to the font family captured when the palette opened.
    /// The snapshot uses `Option<Option<String>>` so we can tell
    /// "palette never opened" (outer `None`) from "user had no
    /// `font-family =` line in config" (inner `None`).
    fn revert_font_family(self: &Rc<Self>) {
        let Some(target) = self.font_family_at_open.borrow().clone() else {
            return;
        };
        let current = self.font_family.borrow().clone();
        if current == target {
            return;
        }
        self.set_active_font_family(target);
    }

    /// Curated programming fonts that look great in a terminal, in a
    /// thoughtful order. The first entry that's actually installed
    /// becomes the top of the picker; uninstalled entries are skipped.
    /// Mirrors the Swift `availableFontFamilies` curated list.
    const CURATED_FONTS: &'static [&'static str] = &[
        "JetBrains Mono",
        "JetBrainsMono Nerd Font",
        "Fira Code",
        "Fira Mono",
        "Hack",
        "Source Code Pro",
        "Cascadia Code",
        "Cascadia Mono",
        "IBM Plex Mono",
        "Inconsolata",
        "Iosevka",
        "DejaVu Sans Mono",
        "Ubuntu Mono",
        "Liberation Mono",
        "Noto Sans Mono",
        // Mac-only families (no-op on Linux when not installed).
        "SF Mono",
        "Menlo",
        "Monaco",
    ];

    /// Enumerate font families for the picker: curated first
    /// (filtered to installed), then every other monospace family
    /// alphabetically. Uses the active window's Pango context so the
    /// resolved font_map matches what the renderer will actually use.
    fn available_font_families(self: &Rc<Self>) -> Vec<String> {
        let context = self.window.pango_context();
        let Some(font_map) = context.font_map() else {
            // Fontconfig blew up — offer only the generic Monospace
            // alias, which always resolves to *some* installed face.
            // The curated list verbatim would be unsafe: an Enter on
            // a curated row that isn't installed would persist a font
            // the renderer can't satisfy, silently falling back to a
            // different glyph set than the picker advertised.
            return vec!["Monospace".to_string()];
        };
        let families = font_map.list_families();
        // Build a name→is_monospace map (case-insensitive lookup).
        let mut installed: Vec<(String, bool)> = families
            .iter()
            .map(|family| (family.name().to_string(), family.is_monospace()))
            .collect();
        installed.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

        let canonical_name = |name: &str| -> Option<String> {
            installed
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(n, _)| n.clone())
        };

        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::<String>::new();
        for entry in Self::CURATED_FONTS {
            if let Some(n) = canonical_name(entry) {
                let key = n.to_lowercase();
                if seen.insert(key) {
                    out.push(n);
                }
            }
        }
        for (n, is_mono) in &installed {
            if !is_mono {
                continue;
            }
            let key = n.to_lowercase();
            if seen.insert(key) {
                out.push(n.clone());
            }
        }
        out
    }

    fn active_terminal_view(self: &Rc<Self>) -> Option<Rc<TerminalView>> {
        let pid = *self.active_project_id.borrow();
        let projects = self.projects.borrow();
        let ui = projects.get(&pid)?;
        let page = ui.tab_view.selected_page()?;
        let tab_id = parse_tab_id_from_page(&page)?;
        let view = ui.tabs.borrow().get(&tab_id).map(|t| t.view.clone());
        view
    }

    /// Find the `TerminalView` for any tab id (not just the active one)
    /// by scanning every project's tab map. Backs `tab.dump`.
    fn terminal_view_for(self: &Rc<Self>, tab_id: i64) -> Option<Rc<TerminalView>> {
        let projects = self.projects.borrow();
        for ui in projects.values() {
            if let Some(tab) = ui.tabs.borrow().get(&tab_id) {
                return Some(tab.view.clone());
            }
        }
        None
    }

    /// Return both `(view, session)` Rc clones in one scan. Used by
    /// the IPC mouse/focus paths so a project removal between two
    /// separate lookups can't return inconsistent
    /// `(Some(session), None)` — AND so the synthetic-event hot
    /// path pays only one O(n) walk.
    fn tab_handles_for(self: &Rc<Self>, tab_id: i64) -> Option<(Rc<TerminalView>, Rc<TabSession>)> {
        let projects = self.projects.borrow();
        for ui in projects.values() {
            if let Some(tab) = ui.tabs.borrow().get(&tab_id) {
                return Some((tab.view.clone(), tab.session.clone()));
            }
        }
        None
    }

    /// Read a tab's terminal viewport as text for the `tab.dump` op.
    /// Errors (→ `not-found` on the wire) when the tab has no live
    /// `TerminalView`.
    fn dump_tab(self: &Rc<Self>, tab_id: i64) -> Result<roost_linux::ipc::DumpData, String> {
        let view = self
            .terminal_view_for(tab_id)
            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?;
        let d = view.dump();
        Ok(roost_linux::ipc::DumpData {
            cols: d.cols,
            rows: d.rows,
            cursor: d.cursor.map(|c| (c.row, c.col, c.visible)),
            rows_text: d.rows_text,
        })
    }

    // MARK: command palette — IPC drive surface (palette.* ops)
    //
    // These service the `UiRequest::Palette*` arms on the GTK main
    // thread. Each clones the overlay's `driver()` out of `self.palette`
    // and drops the borrow *before* driving it, because a confirm's
    // dismiss path re-borrows `self.palette` (to clear the handle) — see
    // `PaletteOverlay::driver`.

    /// `palette.open`: present a root frame (kind pre-validated by the
    /// IPC layer: "" / "commands" → command palette, "launcher" → the
    /// custom-command launcher), then read back its state.
    fn ipc_palette_open(self: &Rc<Self>, kind: &str) -> PaletteStateResult {
        match kind {
            "launcher" => self.show_command_launcher(),
            "custom" => self.show_custom_palette(),
            _ => self.show_command_palette(),
        }
        self.ipc_palette_state()
    }

    /// `palette.state`: snapshot the live palette, or the default
    /// (`open: false`) when none is up.
    fn ipc_palette_state(self: &Rc<Self>) -> PaletteStateResult {
        match self.palette.borrow().as_ref() {
            Some(overlay) => palette_state_from(&overlay.driver().snapshot()),
            None => PaletteStateResult::default(),
        }
    }

    /// `palette.query`: set the filter on the open palette (no-op when
    /// closed), then read back.
    fn ipc_palette_query(self: &Rc<Self>, query: &str) -> PaletteStateResult {
        let driver = self.palette.borrow().as_ref().map(|o| o.driver());
        if let Some(driver) = driver {
            driver.set_query(query);
        }
        self.ipc_palette_state()
    }

    /// `palette.activate`: confirm the visible row with `id` (the same
    /// dispatch as its keybind). Err (→ `not-found`) when no palette is
    /// open or no row matches.
    fn ipc_palette_activate(self: &Rc<Self>, id: &str) -> Result<PaletteStateResult, String> {
        let driver = self.palette.borrow().as_ref().map(|o| o.driver());
        let Some(driver) = driver else {
            return Err("no palette open".into());
        };
        if !driver.activate(id) {
            return Err(format!("no palette row with id {id:?}"));
        }
        Ok(self.ipc_palette_state())
    }

    /// `palette.dismiss`: close any open palette (no-op when closed),
    /// then read back the closed state.
    fn ipc_palette_dismiss(self: &Rc<Self>) -> PaletteStateResult {
        let driver = self.palette.borrow().as_ref().map(|o| o.driver());
        if let Some(driver) = driver {
            driver.dismiss();
        }
        self.ipc_palette_state()
    }

    /// `palette.present`: open the palette on a caller-supplied list and
    /// fulfill `reply` once the user picks a row (`selected_id`) or
    /// dismisses. Blocking — the reply is sent from the confirm/dismiss
    /// closures, not here. Any already-open palette is dismissed so the
    /// present takes over.
    fn ipc_palette_present(
        self: &Rc<Self>,
        title: String,
        placeholder: String,
        items: Vec<(String, String, Option<String>)>,
        reply: tokio::sync::oneshot::Sender<
            Result<roost_ipc::messages::PalettePresentResult, String>,
        >,
    ) {
        // Close any open palette first (its own dismiss path clears the
        // handle via on_dismiss), then present fresh.
        let existing = self.palette.borrow().as_ref().map(|o| o.driver());
        if let Some(driver) = existing {
            driver.dismiss();
        }

        let placeholder = if !placeholder.is_empty() {
            placeholder
        } else if !title.is_empty() {
            title
        } else {
            "Select…".to_string()
        };
        let palette_items: Vec<PaletteItem> = items
            .into_iter()
            .map(|(id, title, subtitle)| PaletteItem::new(id, title).with_subtitle(subtitle))
            .collect();
        let root = PaletteFrame::new("present", placeholder, palette_items);

        // Shared between confirm (a pick) and dismiss; whoever fires
        // first takes the sender, so the reply is sent exactly once.
        let shared = Rc::new(RefCell::new(Some(reply)));
        let confirm_reply = shared.clone();
        let behavior = PaletteBehavior::new(move |item| {
            if let Some(tx) = confirm_reply.borrow_mut().take() {
                let _ = tx.send(Ok(roost_ipc::messages::PalettePresentResult {
                    selected_id: Some(item.id.clone()),
                    dismissed: false,
                }));
            }
            PaletteOutcome::Close
        });

        let top_margin = self.palette_top_margin();
        let weak_dismiss = Rc::downgrade(self);
        let dismiss_reply = shared.clone();
        let overlay = PaletteOverlay::present(
            &self.content_overlay,
            root,
            behavior,
            top_margin,
            move || {
                if let Some(app) = weak_dismiss.upgrade() {
                    app.dismiss_palette();
                }
                if let Some(tx) = dismiss_reply.borrow_mut().take() {
                    let _ = tx.send(Ok(roost_ipc::messages::PalettePresentResult {
                        selected_id: None,
                        dismissed: true,
                    }));
                }
            },
        );
        *self.palette.borrow_mut() = Some(overlay);
    }

    // MARK: selection + clipboard — IPC drive surface
    //
    // Service the `UiRequest::Selection*` / `UiRequest::Clipboard*` arms
    // on the GTK main thread. Selection ops require a live `TerminalView`
    // (`not-found` otherwise — matches `tab.dump`); clipboard ops touch
    // the host pasteboard via `crate::clipboard`, identical to the
    // user-driven Ctrl+Shift+C / drag paths.

    fn ipc_selection_set(
        self: &Rc<Self>,
        tab_id: i64,
        anchor: (u16, u16),
        cursor: (u16, u16),
    ) -> Result<(), String> {
        let view = self
            .terminal_view_for(tab_id)
            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?;
        view.set_selection(anchor, cursor);
        Ok(())
    }

    fn ipc_selection_clear(self: &Rc<Self>, tab_id: i64) -> Result<(), String> {
        let view = self
            .terminal_view_for(tab_id)
            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?;
        view.clear_selection();
        Ok(())
    }

    fn ipc_selection_dump(
        self: &Rc<Self>,
        tab_id: i64,
    ) -> Result<Option<roost_linux::ipc::SelectionData>, String> {
        let view = self
            .terminal_view_for(tab_id)
            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?;
        Ok(view
            .dump_selection()
            .map(|d| roost_linux::ipc::SelectionData {
                text: d.text,
                anchor_visible: d.anchor_visible,
                cursor_visible: d.cursor_visible,
            }))
    }

    fn ipc_clipboard_dump(
        self: &Rc<Self>,
        target: roost_linux::ipc::ClipboardOp,
        reply: tokio::sync::oneshot::Sender<Result<Option<String>, String>>,
    ) {
        // Inline equivalent of `clipboard::read` so we can answer the
        // oneshot in BOTH the "had text" and "had nothing" cases —
        // `clipboard::read`'s callback only fires on success, which
        // would leave the IPC caller waiting forever for an empty
        // pasteboard.
        let gdk_target = match target {
            roost_linux::ipc::ClipboardOp::System => clipboard::Target::Clipboard,
            roost_linux::ipc::ClipboardOp::Selection => clipboard::Target::Primary,
        };
        let Some(display) = gtk4::gdk::Display::default() else {
            let _ = reply.send(Ok(None));
            return;
        };
        let cb = match gdk_target {
            clipboard::Target::Clipboard => display.clipboard(),
            #[cfg(target_os = "linux")]
            clipboard::Target::Primary => display.primary_clipboard(),
            #[cfg(not(target_os = "linux"))]
            clipboard::Target::Primary => {
                let _ = reply.send(Ok(None));
                return;
            }
        };
        glib::spawn_future_local(async move {
            let result = match cb.read_text_future().await {
                Ok(Some(text)) => Ok(Some(text.to_string())),
                Ok(None) => Ok(None),
                Err(_) => Ok(None),
            };
            let _ = reply.send(result);
        });
    }

    fn ipc_clipboard_write(self: &Rc<Self>, target: roost_linux::ipc::ClipboardOp, text: String) {
        let target = match target {
            roost_linux::ipc::ClipboardOp::System => clipboard::Target::Clipboard,
            roost_linux::ipc::ClipboardOp::Selection => clipboard::Target::Primary,
        };
        clipboard::write(target, &text);
    }

    /// Drop the per-tab test-mode entries we registered in
    /// `attach_tab` (no-op outside test mode and for tabs whose
    /// attach failed before the registration ran). Called from
    /// both the `TabClosed` event arm and the `attach_tab`
    /// `fail_cleanup` path so a failed attach can't leak entries
    /// either.
    fn drop_test_mode_state(self: &Rc<Self>, tab_id: i64) {
        self.feed_senders.borrow_mut().remove(&tab_id);
        self.input_captures.borrow_mut().remove(&tab_id);
    }

    /// Inject bytes into a live tab's PTY-output drain. Gated on
    /// `ROOST_TEST_MODE=1`. The bytes ride the same
    /// `mpsc::UnboundedSender<TabOutput>` the supervisor uses, so the
    /// OSC scanner + libghostty + the reply path all see them as if
    /// they came from the shell — no shadow drain. (See
    /// `docs/development/vision.md`: "No test-only backdoors that
    /// drift from reality.")
    fn ipc_tab_feed_pty_bytes(self: &Rc<Self>, tab_id: i64, data: Vec<u8>) -> Result<(), String> {
        if !self.test_mode {
            return Err("tab.feed_pty_bytes requires ROOST_TEST_MODE=1 at UI launch".into());
        }
        let senders = self.feed_senders.borrow();
        let Some(tx) = senders.get(&tab_id) else {
            return Err(format!("tab {tab_id} has no live terminal"));
        };
        tx.send(TabOutput::Bytes(data))
            .map_err(|e| format!("feed channel closed: {e}"))
    }

    /// Return (and optionally drain) the PTY-input bytes the UI has
    /// queued for this tab since the last drain. Gated on
    /// `ROOST_TEST_MODE=1`. The buffer is populated by
    /// `TabSession::send_input` (a single tap point inside the
    /// per-tab serial channel), so every keystroke / paste / OSC
    /// reply is observable here without parallel plumbing.
    fn ipc_tab_capture_pty_input(
        self: &Rc<Self>,
        tab_id: i64,
        drain: bool,
    ) -> Result<Vec<u8>, String> {
        if !self.test_mode {
            return Err("tab.capture_pty_input requires ROOST_TEST_MODE=1 at UI launch".into());
        }
        let captures = self.input_captures.borrow();
        let Some(buf) = captures.get(&tab_id) else {
            return Err(format!("tab {tab_id} has no live terminal"));
        };
        let mut guard = buf
            .lock()
            .map_err(|e| format!("capture buffer poisoned: {e}"))?;
        let bytes = if drain {
            std::mem::take(&mut *guard)
        } else {
            guard.clone()
        };
        Ok(bytes)
    }

    /// Walk the tab's live render state through the SAME
    /// `resolve_cell_colors` call the production `paint` path runs,
    /// using the live `theme.bold_color`. Ungated — this is a
    /// richer read of existing render state, not a shadow surface,
    /// so it's safe (and useful) outside test mode. Pins #142's
    /// resolver call-site plumbing end-to-end.
    fn ipc_tab_dump_resolved(
        self: &Rc<Self>,
        tab_id: i64,
    ) -> Result<roost_linux::ipc::ResolvedCellsData, String> {
        let view = self
            .terminal_view_for(tab_id)
            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?;
        Ok(view.dump_resolved_cells())
    }

    /// Drive the production word-/line-expansion dispatch from
    /// explicit `(col, row, click_count)` coords. Gated on
    /// `ROOST_TEST_MODE=1` (same gate as `tab.feed_pty_bytes`) so a
    /// user-local script can't move the selection out from under the
    /// user. Returns the (col0, col1, text) triple matching the
    /// committed selection — same shape `tab.expand_selection_at`
    /// emits on the wire.
    fn ipc_tab_expand_selection_at(
        self: &Rc<Self>,
        tab_id: i64,
        col: u16,
        row: u16,
        click_count: u8,
    ) -> Result<roost_linux::ipc::ExpandSelectionData, String> {
        if !self.test_mode {
            return Err("tab.expand_selection_at requires ROOST_TEST_MODE=1 at UI launch".into());
        }
        let view = self
            .terminal_view_for(tab_id)
            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?;
        let (col0, col1, text) =
            view.expand_selection_at(col, row, click_count)
                .ok_or_else(|| {
                    format!(
                        "no word/line span at ({col}, {row}) on tab {tab_id} \
                     (whitespace double-click, or row out of range)"
                    )
                })?;
        Ok(roost_linux::ipc::ExpandSelectionData { col0, col1, text })
    }

    /// Drive a synthetic mouse event into the production routing
    /// path at cell coords. Gated on `ROOST_TEST_MODE=1`. Same
    /// mouse-tracking gating + encoder format negotiation production
    /// uses — the seam is `TerminalView::ipc_dispatch_mouse_event`.
    fn ipc_tab_dispatch_mouse_event(
        self: &Rc<Self>,
        tab_id: i64,
        kind: roost_linux::mouse_routing::MouseRoutingAction,
        button: Option<roost_linux::mouse_routing::MouseRoutingButton>,
        cell_x: u32,
        cell_y: u32,
        mods: u32,
    ) -> Result<(), String> {
        if !self.test_mode {
            return Err("tab.dispatch_mouse_event requires ROOST_TEST_MODE=1 at UI launch".into());
        }
        let (view, session) = self
            .tab_handles_for(tab_id)
            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?;
        // libghostty's `Mods` is u16; reject IPC payloads that overflow
        // it rather than silently dropping the high bits.
        let mods = u16::try_from(mods).map_err(|_| format!("modifier mask {mods} exceeds u16"))?;
        let bytes = view.ipc_dispatch_mouse_event(kind, button, cell_x, cell_y, mods);
        if bytes.is_empty() {
            // Encoder declined (mode/format mismatch). Not a fault —
            // production callers also fall through silently.
            return Ok(());
        }
        // Push to the same PTY input channel keystrokes use. The
        // capture buffer taps that channel, so the e2e test reads
        // the emitted bytes via `tab.capture_pty_input`.
        session.send_input(bytes);
        Ok(())
    }

    /// Drive a synthetic window-focus state change. Gated on
    /// `ROOST_TEST_MODE=1`. Targets the active tab (matches the Mac
    /// behavior). Encoder is gated on mode 1004 by
    /// `TerminalView::ipc_set_window_focus`.
    fn ipc_app_set_window_focus(self: &Rc<Self>, focused: bool) -> Result<(), String> {
        if !self.test_mode {
            return Err("app.set_window_focus requires ROOST_TEST_MODE=1 at UI launch".into());
        }
        // Active tab: the page selected in the active project's
        // TabView. Returns None when nothing is selected (workspace
        // is empty on a cold start).
        let pid = *self.active_project_id.borrow();
        let Some(active_tab_id) = self.active_tab_id(pid) else {
            // Empty / cold workspace is not a server fault — map to
            // the `not-found` contract via the established phrase so
            // `ipc.rs::map_test_op_err` returns the right code.
            return Err("active tab has no live terminal".into());
        };
        let (view, session) = self
            .tab_handles_for(active_tab_id)
            .ok_or_else(|| format!("active tab {active_tab_id} has no live terminal"))?;
        let bytes = view.ipc_set_window_focus(focused);
        if !bytes.is_empty() {
            session.send_input(bytes);
        }
        Ok(())
    }

    /// `app.cursor_shape` — read the active tab's last-seen OSC 22
    /// W3C cursor name. Ungated (read-only).
    fn ipc_app_cursor_shape(self: &Rc<Self>) -> Result<String, String> {
        let pid = *self.active_project_id.borrow();
        let Some(active_tab_id) = self.active_tab_id(pid) else {
            return Ok("default".to_string());
        };
        let view = self
            .terminal_view_for(active_tab_id)
            .ok_or_else(|| format!("tab {active_tab_id} has no live terminal"))?;
        Ok(view.current_cursor_shape_name())
    }

    /// `app.active_terminal_focused` — whether the active tab's
    /// TerminalView holds GTK *logical* keyboard focus
    /// (`window.focus_widget() == terminal`). Reads logical focus, not
    /// the global `:has-focus` property, so it reports the state
    /// `grab_focus()` sets even under the WM-less Xvfb e2e runner where
    /// the toplevel never gains the compositor's input focus. Ungated
    /// (read-only); returns `false` when there is no active terminal.
    fn ipc_app_active_terminal_focused(self: &Rc<Self>) -> Result<bool, String> {
        let Some(view) = self.active_terminal_view() else {
            return Ok(false);
        };
        let widget = view.widget().clone().upcast::<gtk4::Widget>();
        Ok(gtk4::prelude::GtkWindowExt::focus(&self.window) == Some(widget))
    }

    /// `app.selected_tab_id` — the tab id selected in the active project's
    /// AdwTabView (the on-screen tab), independent of the core's active
    /// tab. `0` when there's no active project or no selection. Lets tests
    /// assert the UI selection and `Workspace::active()` agree.
    fn ipc_app_selected_tab_id(self: &Rc<Self>) -> Result<i64, String> {
        let pid = *self.active_project_id.borrow();
        Ok(self.active_tab_id(pid).unwrap_or(0))
    }

    /// `app.window_metrics` — return window size + sidebar pane width +
    /// collapsed flag in logical points. Read-only: always succeeds.
    /// `Widget::width()` returns the allocated logical width; for the
    /// sidebar that's the start child of the `gtk4::Paned` we built
    /// with `resize_start_child(false) + shrink_start_child(false)`,
    /// so it equals the paned position when visible.
    fn ipc_window_metrics(self: &Rc<Self>) -> Result<(f64, f64, f64, bool), String> {
        let w = self.window.width() as f64;
        let h = self.window.height() as f64;
        let collapsed = !self.sidebar_box.is_visible();
        let sw = if collapsed {
            0.0
        } else {
            self.sidebar_box.width() as f64
        };
        Ok((w, h, sw, collapsed))
    }

    /// `window.resize` (test-mode only) — programmatically resize the
    /// window. Gated on `ROOST_TEST_MODE=1` so a user-local script
    /// can't yank window geometry out from under the user.
    fn ipc_window_resize(self: &Rc<Self>, width: f64, height: f64) -> Result<(), String> {
        if !self.test_mode {
            return Err("window.resize requires ROOST_TEST_MODE=1 at UI launch".into());
        }
        self.window
            .set_default_size(width.round() as i32, height.round() as i32);
        Ok(())
    }

    fn toggle_sidebar(self: &Rc<Self>) {
        // Hide the entire sidebar container — header + list + footer
        // button — so the Paned divider snaps to the left edge. Pre-M5
        // we toggled only the list, leaving the Projects header and
        // `+ Project` button orphaned in a thin strip.
        let visible = self.sidebar_box.is_visible();
        self.sidebar_box.set_visible(!visible);
        // Persist the choice so it survives relaunch (GTK parity with the
        // Mac UI's RoostSidebarVisible). New visibility is `!visible`, so
        // collapsed == `visible` (the prior state).
        if let Some(client) = self.client.borrow().clone() {
            client.workspace.set_sidebar_collapsed(visible);
        }
    }

    fn switch_project_by_index(self: &Rc<Self>, n: usize) {
        if n == 0 {
            return;
        }
        // Map Ctrl/Cmd+N to the Nth project in VISUAL (sidebar) order, so it
        // tracks drag-reorder. Previously this sorted the project ids, which
        // ignored reordering — after dragging a row, Ctrl+N still picked the
        // id-sorted Nth project, not the one at the Nth visible position.
        let order = self.sidebar_order();
        if let Some(&id) = order.get(n - 1) {
            self.activate_project_from_ui(id);
        }
    }

    fn switch_tab_by_index(self: &Rc<Self>, n: usize) {
        if n == 0 {
            return;
        }
        let pid = *self.active_project_id.borrow();
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&pid) else {
            return;
        };
        let pages = ui.tab_view.pages();
        let tab_id = if let Some(obj) = pages.item((n - 1) as u32) {
            if let Ok(page) = obj.downcast::<libadwaita::TabPage>() {
                self.select_page_programmatic(&ui.tab_view, &page);
                parse_tab_id_from_page(&page)
            } else {
                None
            }
        } else {
            None
        };
        drop(projects);
        // Explicit user gesture: sync the core's active tab (the
        // `selected-page` notify intentionally doesn't — see its comment).
        if let Some(tab_id) = tab_id {
            self.sync_core_active_tab(tab_id);
        }
    }

    async fn create_new_project(self: &Rc<Self>) -> anyhow::Result<()> {
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        self.create_new_project_with_cwd(&cwd).await
    }

    /// Variant that lets the caller pin the project's cwd — used by
    /// the headerbar folder-picker button (M5) where the user picks
    /// a directory before the project exists.
    async fn create_new_project_with_cwd(self: &Rc<Self>, cwd: &str) -> anyhow::Result<()> {
        let Some(client) = self.client.borrow().clone() else {
            return Ok(());
        };
        let rt = self.rt.clone();
        let cwd_owned = cwd.to_string();
        let project = rt
            .spawn(async move { client.create_project("", &cwd_owned).await })
            .await
            .context("create_project join")??;
        // Race note: WatchEvents will also deliver ProjectCreated and
        // call `add_project_ui` from `handle_event`. Calling it here
        // optimistically is safe — unlike `attach_existing_tab` (which
        // checks `ui.tabs` before an *async* StreamPty spawn that
        // populates the map), `add_project_ui` is synchronous and
        // dedupes at the top via `projects.contains_key(&id)` *before*
        // any widget construction. The second caller hits the guard
        // and returns cleanly. We keep the optimistic call here for
        // the UX: `set_active_project(project.id)` selects the new
        // project immediately on RPC return, rather than waiting for
        // the WatchEvents round-trip (which doesn't itself flip the
        // active selection).
        self.add_project_ui(&project);
        self.set_active_project(project.id);
        // M9.5: seed the new project with a default tab so the user
        // lands inside a shell. The Mac UI does this same dance —
        // clicking "+ Project" with no follow-up never makes sense,
        // and a bare project with no tabs renders an empty terminal
        // pane (the bug reported).
        self.open_new_tab_in(project.id).await?;
        Ok(())
    }

    /// Forward a parsed OSC event to the daemon. Mirrors the Mac
    /// UI's `RoostApp.reportOsc` path; the daemon decides whether
    /// to emit `TabTitleChanged` / `TabCwdChanged` /
    /// `NotificationEvent` / etc.
    fn report_osc_event(self: &Rc<Self>, tab_id: i64, event: roost_osc::OscEvent) {
        use roost_osc::OscEvent as E;
        // OSC 52 short-circuits before the daemon dispatch: it's not
        // workspace state, just an action. Honoring it on the UI side
        // is correct because only the UI has the OS clipboard handle.
        // `clipboard-write = deny` drops silently + logs at info,
        // matching Ghostty's behavior (Surface.zig:2164-2166).
        if let E::Clipboard { target, text } = event {
            if *self.clipboard_write_policy.borrow() == config::ClipboardWrite::Deny {
                tracing::info!(
                    tab_id,
                    "OSC 52 clipboard write dropped — clipboard-write = deny"
                );
                return;
            }
            let target = match target {
                roost_osc::ClipboardTarget::System => clipboard::Target::Clipboard,
                roost_osc::ClipboardTarget::Selection => clipboard::Target::Primary,
            };
            clipboard::write(target, &text);
            return;
        }
        // OSC 22 pointer shape — UI-only action (no workspace state).
        // Apply the W3C cursor name to the matching tab's
        // TerminalView; the view stores it, gates the actual
        // `set_cursor_from_name` on pointer-in-view, and resets on
        // alt-screen exit. Mirrors the Mac UI's
        // `applyCurrentCursorShapeIfNeeded` path.
        if let E::MouseShape(name) = event {
            if let Some(view) = self.terminal_view_for(tab_id) {
                view.apply_mouse_shape(&name);
            }
            return;
        }
        let (command, payload): (u32, String) = match event {
            E::Title(t) => (0, t),
            // OSC 7 wire format is `file://<host>/<path>`. The
            // scanner already stripped `file://[host]` so `p`
            // starts with `/`. Wrap back as `file://<empty-host>/<p>`
            // = `file:///path` so the daemon's parse_cwd_from_osc7
            // doesn't mistake the first path segment for a host
            // (caught during Phase 7 smoke testing — sending
            // `file:/<path>` produced cwd `/<second-component>`).
            E::Pwd(p) => (7, format!("file://{}", p)),
            E::Notification { title, body } => {
                if body.is_empty() {
                    (9, title)
                } else {
                    (777, format!("notify;{title};{body}"))
                }
            }
            E::ColorQuery(n) => (n as u32, "?".to_string()),
            // OSC 133 prompt/command mark — pass the body through to
            // apply_osc, which maps it to tab state (P4b).
            E::CommandMark(body) => (133, body),
            // Handled by the short-circuits above; unreachable here.
            E::Clipboard { .. } => unreachable!(),
            // Handled above via `apply_mouse_shape` on the tab's view.
            E::MouseShape(_) => unreachable!(),
            // Handled by the drain's OSC 4 reply short-circuit; never
            // routed to the daemon.
            E::PaletteQuery(_) => unreachable!(),
        };
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        // M3b: OSC routing no longer round-trips through a daemon.
        // `LocalClient::apply_osc` updates the in-process workspace
        // directly; the event broadcast handler picks up the
        // resulting `TabCwdChanged` / `TabTitleChanged` /
        // `NotificationFired` events on the GTK main loop.
        client.apply_osc(tab_id, command, &payload);
    }

    /// Fire a desktop notification via `gio::Notification`. On Linux
    /// this routes through `org.freedesktop.Notifications` (DBus);
    /// on macOS Homebrew GTK's gio backend bridges to
    /// NSUserNotification (good enough for side-by-side verification
    /// against the Swift Mac UI).
    fn fire_desktop_notification(self: &Rc<Self>, tab_id: i64, title: &str, body: &str) {
        let Some(app) = self.window.application() else {
            return;
        };
        let notification = gtk4::gio::Notification::new(title);
        if !body.is_empty() {
            notification.set_body(Some(body));
        }
        // Default-action target so click→focus-tab works once the
        // tab-focus app action is registered (commit 11 hooks it).
        let target = tab_id.to_variant();
        notification.set_default_action_and_target_value("app.focus-tab", Some(&target));
        let id = format!("roost-tab-{tab_id}");
        app.send_notification(Some(&id), &notification);
    }

    /// Bring the tab with `tab_id` forward — focus its project's
    /// AdwTabView, select the matching page, then raise the window.
    /// Wired via the `app.focus-tab` action; click-handler in the
    /// gio::Notification's default action target.
    fn focus_tab_by_id(self: &Rc<Self>, tab_id: i64) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        // Route through the core: `focus_tab` updates `workspace.active()`
        // (so `identify` / `tab.focus` report the tab the user was sent
        // to) and fires `ActiveChanged`, which the handler reacts to with
        // the project switch + tab select. Previously this did a UI-only
        // `set_active_project` + `set_selected_page`, leaving the core's
        // active tab stale vs. what's on screen — the UI as its own source
        // of truth rather than a reaction to the core.
        if client.workspace.focus_tab(tab_id).is_err() {
            return; // tab vanished between the inbox snapshot and the jump
        }
        // Bring the window forward (jump-specific; ActiveChanged handles
        // the on-screen selection).
        self.window.present();
        // Parity with Mac's `selectTab`: focusing a tab clears its
        // pending notification, which drops the inbox row + the tab's
        // needs-attention badge via the `TabNotification` false-edge.
        let _ = client.workspace.set_tab_has_notification(tab_id, false);
    }

    /// Make `project_id` active from a UI action (sidebar click, project
    /// keybind) and sync the core to its active tab. The sync lives here,
    /// not in `set_active_project`, because that is also the
    /// `ActiveChanged` reaction body — syncing there would echo the core's
    /// *previous* selection back over the one the caller asked for.
    fn activate_project_from_ui(self: &Rc<Self>, project_id: i64) {
        self.set_active_project(project_id);
        // An empty project (no tab yet) has nothing to focus, so the
        // core's active selection stays put until its first tab opens. In
        // normal use a project always has >=1 tab — closing the last tab
        // cascades the project away — so this only bites a raw IPC
        // `project.create` with no `tab.open`.
        if let Some(tab_id) = self.active_tab_id(project_id) {
            self.sync_core_active_tab(tab_id);
        }
    }

    /// Sync the workspace core's active selection to `tab_id` after a
    /// UI-originating selection change. Without it the core's `active()` —
    /// read by `identify`, persistence, and notification routing — goes
    /// stale vs. what's on screen (UI as its own source of truth, against
    /// the north star). Guarded on the *read*: `focus_tab` always emits
    /// `ActiveChanged` (state.rs:881), so skipping when the core is
    /// already on this tab stops a core-driven `set_selected_page` notify
    /// from echoing `focus_tab` back and looping.
    fn sync_core_active_tab(self: &Rc<Self>, tab_id: i64) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if client.workspace.active().1 != tab_id {
            // This is the boundary (a GTK signal handler), so log rather
            // than propagate. The only error is the tab vanishing between
            // the UI selection and here — benign for a sync.
            if let Err(e) = client.workspace.focus_tab(tab_id) {
                tracing::debug!(?e, tab_id, "sync_core_active_tab: focus_tab failed");
            }
        }
    }

    /// `KeybindAction::JumpToUnread`: focus the next tab with a pending
    /// notification — preferring the active project (the multi-project
    /// triage shortcut), else the oldest pending elsewhere. Mirrors the
    /// Mac app's `jumpToUnread`. The focus clears that tab's badge (via
    /// `focus_tab_by_id`), so repeating the action walks the inbox.
    fn jump_to_unread(self: &Rc<Self>) {
        let active_pid = *self.active_project_id.borrow();
        let target = {
            let inbox = self.notification_inbox.borrow();
            let pending = inbox.snapshot();
            pending
                .iter()
                .find(|r| r.project_id == active_pid)
                .or_else(|| pending.first())
                .map(|r| r.tab_id)
        };
        if let Some(tab_id) = target {
            self.focus_tab_by_id(tab_id);
        }
    }

    /// M9.5: close an AdwTabPage by tab id. Routes through the
    /// `tab_view.close_page` → close-page signal handler set up in
    /// `add_project_ui`, which performs the actual page removal +
    /// the CloseTab RPC. Used by the PTY-exit drain (shell typed
    /// `exit` or process died) and by the cross-client TabDeleted
    /// handler. Called via `glib::idle_add_local_once` from the
    /// drain so the close_page emission lands on a fresh main-loop
    /// iteration, outside the in-flight `spawn_future_local`.
    fn close_page_for_tab(self: &Rc<Self>, project_id: i64, tab_id: i64) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        // Snapshot the page + tab_view out of the borrow so the
        // close_page call below can fire the close-page signal
        // synchronously without us holding any borrow on App state.
        let page = ui.tabs.borrow().get(&tab_id).map(|t| t.page.clone());
        let tab_view = ui.tab_view.clone();
        drop(projects);
        if let Some(page) = page {
            tracing::debug!(project_id, tab_id, "close_page_for_tab");
            tab_view.close_page(&page);
        }
    }

    /// Fire `CloseTab` on the daemon for `tab_id`. Async so we don't
    /// block the GTK main loop.
    ///
    /// `NotFound` is treated as an expected race: when the daemon
    /// cascade-deletes a tab (M5 shell-exit cascade, project delete,
    /// CLI `tab close`, another client's close), the resulting
    /// `TabDeletedEvent` may race this RPC. By the time the RPC
    /// lands, the daemon-side tab is already gone, so `NotFound` is
    /// the expected steady state, not a bug to log. Other status
    /// codes (Internal, FailedPrecondition, etc.) still surface as
    /// warnings.
    fn close_tab_async(self: &Rc<Self>, _project_id: i64, tab_id: i64) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            match client.close_tab(tab_id).await {
                Ok(_) => {}
                Err(err) => {
                    // The "tab already gone" case surfaces as a
                    // `not-found` WorkspaceError, wrapped in anyhow.
                    // Treat it as info, not warn — common during a
                    // close-on-PTY-exit race where the supervisor
                    // already removed the session.
                    if err.to_string().contains("not found") {
                        tracing::debug!(tab_id, "close_tab: tab already gone");
                    } else {
                        tracing::warn!(?err, tab_id, "close_tab failed");
                    }
                }
            }
        });
    }

    /// M10: read the current visual order of project ids by
    /// walking the sidebar's rows. Each row's `widget_name` is
    /// `"project-<id>"` (set in `add_project_ui`).
    fn sidebar_order(&self) -> Vec<i64> {
        let mut ids = Vec::new();
        let mut i: i32 = 0;
        while let Some(row) = self.sidebar.row_at_index(i) {
            if let Some(rest) = row.widget_name().strip_prefix("project-") {
                if let Ok(id) = rest.parse::<i64>() {
                    ids.push(id);
                }
            }
            i += 1;
        }
        ids
    }

    /// M10: rebuild the sidebar listbox so rows match `ordered_ids`.
    /// Selection-sort: walks the target and remove/inserts any
    /// out-of-place row. Active selection is restored at the end
    /// (Remove clears selection in SelectionBrowse mode, which
    /// would otherwise jump the active project mid-rebuild).
    /// Used on rollback (drag cancelled) and on inbound
    /// `ProjectsReordered` events.
    fn apply_sidebar_order(self: &Rc<Self>, ordered_ids: &[i64]) {
        let prev_active = *self.active_project_id.borrow();
        let projects = self.projects.borrow();
        for (i, id) in ordered_ids.iter().enumerate() {
            let Some(ui) = projects.get(id) else { continue };
            if ui.sidebar_row.index() != i as i32 {
                self.sidebar.remove(&ui.sidebar_row);
                self.sidebar.insert(&ui.sidebar_row, i as i32);
            }
        }
        if let Some(ui) = projects.get(&prev_active) {
            self.sidebar.select_row(Some(&ui.sidebar_row));
        }
    }

    /// M10: live-shuffle the dragged row toward `raw_target_idx`
    /// during drag-motion. Pure visual feedback; persistence
    /// happens later in the drop handler. No-op when the drop
    /// would land on the source's own slot.
    fn shuffle_sidebar_toward(self: &Rc<Self>, src_id: i64, raw_target_idx: usize) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&src_id) else {
            return;
        };
        let source_idx = ui.sidebar_row.index();
        if source_idx < 0 {
            return;
        }
        let Some(insert_idx) = compute_insert_idx(source_idx as usize, raw_target_idx) else {
            return;
        };
        // Preserve active selection across the remove/insert.
        // Removing the selected row clears selection in
        // SelectionBrowse mode, which would otherwise switch
        // projects when the user drags the active row.
        let prev_active = *self.active_project_id.borrow();
        self.sidebar.remove(&ui.sidebar_row);
        self.sidebar.insert(&ui.sidebar_row, insert_idx as i32);
        if let Some(active_ui) = projects.get(&prev_active) {
            self.sidebar.select_row(Some(&active_ui.sidebar_row));
        }
    }

    /// M10: turn a pointer y-coordinate (in the listbox's coord
    /// space) into a raw insertion index for the visual order.
    /// When the pointer is over a row, the index is the row's
    /// index if y is above the row's midline, else row_index + 1.
    /// When the pointer is in empty space below the last row, we
    /// return the row count ("insert at end").
    fn raw_target_for_y(&self, y: f64) -> usize {
        if let Some(row) = self.sidebar.row_at_y(y as i32) {
            let mut idx = row.index();
            if let Some(bounds) = row.compute_bounds(&self.sidebar) {
                let mid = bounds.y() as f64 + bounds.height() as f64 / 2.0;
                if y >= mid {
                    idx += 1;
                }
            }
            return idx.max(0) as usize;
        }
        self.sidebar_order().len()
    }

    /// M10 (Wayland-safe): pointer-drag reorder for a project row via
    /// `GtkGestureDrag` — NOT GTK DnD. The old `DragSource` + listbox
    /// `DropTarget` created a drag-icon surface that aborted the whole process
    /// on Wayland in `gdksurface-wayland.c:348:frame_callback` (same class as
    /// the tab pills; see `build_tab_pill`). The gesture arms past an 8px
    /// threshold, claims the sequence (so a plain click still selects the row /
    /// switches project), live-shuffles the rows under the pointer
    /// (`shuffle_sidebar_toward`), and on release persists the order via
    /// `reorder_projects_async` (which rolls back on RPC error). A cancelled
    /// drag rolls back to the pre-drag order. The row dims to 40% opacity while
    /// dragging (CSS `:drop(active)` was unreliable here, so we set it direct).
    /// M10 (Wayland-safe): pointer-drag reorder for the project sidebar via a
    /// single `GtkGestureDrag` on the listbox — NOT GTK DnD. The old
    /// `DragSource` + `DropTarget` created a drag-icon surface that aborted the
    /// whole process on Wayland in `gdksurface-wayland.c:348:frame_callback`
    /// (same class as the tab pills; see `build_tab_pill`). A *per-row* gesture
    /// is starved by GtkListBox's own selection gesture, so this lives on the
    /// listbox: it derives the pressed row from the start point, arms past an
    /// 8px threshold (claiming so a plain click still selects / switches
    /// project), live-shuffles the rows under the pointer
    /// (`shuffle_sidebar_toward`), and on release persists the order via
    /// `reorder_projects_async` (which rolls back on RPC error). A cancelled
    /// drag rolls back to the pre-drag order. Installed once from `App::new`.
    fn install_sidebar_reorder_gesture(self: &Rc<Self>) {
        let reorder = gtk4::GestureDrag::new();
        reorder.set_button(gtk4::gdk::BUTTON_PRIMARY);
        // The gesture only *claims* once past the 8px threshold, so a plain
        // click is never claimed and falls through to the listbox's own
        // selection (project switch) unchanged — verified against the old DnD
        // sidebar, which behaves identically for clicks.
        reorder.connect_drag_update({
            let app = self.clone();
            move |g, off_x, off_y| {
                // Arm once past the threshold; until then it stays a click.
                if app.dragged_project_id.borrow().is_none() {
                    if off_x.hypot(off_y) < 8.0 {
                        return;
                    }
                    // Which row was pressed? start_point is in listbox coords
                    // (the gesture is on the listbox).
                    let Some((_sx, sy)) = g.start_point() else {
                        return;
                    };
                    let Some(row) = app.sidebar.row_at_y(sy as i32) else {
                        return; // press landed in empty space
                    };
                    // Don't hijack an inline rename — the entry owns the pointer.
                    if let Some(stack) = row.child().and_then(|c| c.downcast::<gtk4::Stack>().ok())
                    {
                        if stack.visible_child_name().as_deref() == Some("entry") {
                            return;
                        }
                    }
                    let Some(pid) = row
                        .widget_name()
                        .strip_prefix("project-")
                        .and_then(|s| s.parse::<i64>().ok())
                    else {
                        return;
                    };
                    g.set_state(gtk4::EventSequenceState::Claimed);
                    row.set_opacity(0.4);
                    *app.dragged_project_id.borrow_mut() = Some(pid);
                    *app.drag_original_order.borrow_mut() = app.sidebar_order();
                }
                // Armed: live-shuffle the rows toward the current pointer.
                if let Some(pid) = *app.dragged_project_id.borrow() {
                    if let Some((_px, py)) = g.point(None) {
                        let raw = app.raw_target_for_y(py);
                        app.shuffle_sidebar_toward(pid, raw);
                    }
                }
            }
        });
        reorder.connect_drag_end({
            let app = self.clone();
            move |g, _off_x, _off_y| {
                let Some(pid) = *app.dragged_project_id.borrow() else {
                    return; // never armed → it was a click, not a drag
                };
                app.set_row_opacity(pid, 1.0);
                // Settle the final slot, then persist the order once. The
                // snapshot lets reorder_projects_async roll back on RPC error.
                if let Some((_px, py)) = g.point(None) {
                    let raw = app.raw_target_for_y(py);
                    app.shuffle_sidebar_toward(pid, raw);
                }
                let current = app.sidebar_order();
                let original = app.drag_original_order.borrow().clone();
                if current != original {
                    app.reorder_projects_async(current, original);
                }
                *app.dragged_project_id.borrow_mut() = None;
                app.drag_original_order.borrow_mut().clear();
            }
        });
        reorder.connect_cancel({
            let app = self.clone();
            move |_, _| {
                let Some(pid) = *app.dragged_project_id.borrow() else {
                    return;
                };
                app.set_row_opacity(pid, 1.0);
                // Cancelled drag (grab broken) → revert to the pre-drag order.
                let original = app.drag_original_order.borrow().clone();
                app.apply_sidebar_order(&original);
                *app.dragged_project_id.borrow_mut() = None;
                app.drag_original_order.borrow_mut().clear();
            }
        });
        self.sidebar.add_controller(reorder);
    }

    /// Set a project's sidebar-row opacity (dims the dragged row to 40% during
    /// a reorder drag; CSS `:drop(active)` was unreliable here).
    fn set_row_opacity(&self, project_id: i64, opacity: f64) {
        if let Some(ui) = self.projects.borrow().get(&project_id) {
            ui.sidebar_row.set_opacity(opacity);
        }
    }

    /// M10: fire `ReorderProjects` RPC. On error, roll the
    /// sidebar back to `snapshot` so the visual order doesn't
    /// diverge from the daemon's persisted order. The double-spawn
    /// (`glib::spawn_future_local` wrapping `rt.spawn`) bridges
    /// the tokio gRPC future back to the GTK main loop so the
    /// rollback runs on the right thread — same pattern as
    /// `attach_existing_tab`.
    fn reorder_projects_async(self: &Rc<Self>, ordered_ids: Vec<i64>, snapshot: Vec<i64>) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        let app = self.clone();
        glib::spawn_future_local(async move {
            let handle = rt.spawn(async move { client.reorder_projects(ordered_ids).await });
            match handle.await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::warn!(?err, "reorder_projects failed");
                    app.apply_sidebar_order(&snapshot);
                }
                Err(join_err) => {
                    tracing::warn!(?join_err, "reorder_projects task join failed");
                    app.apply_sidebar_order(&snapshot);
                }
            }
        });
    }

    /// M10: fire `ReorderTabs` RPC. Fire-and-forget — on success
    /// the daemon broadcasts `TabsReorderedEvent` and the
    /// WatchEvents arm re-applies the order; on failure we log
    /// and let the user retry. (AdwTabView's built-in drag
    /// already completed the visual move; reverting it would be
    /// jarring for a transient daemon hiccup.)
    fn reorder_tabs_async(self: &Rc<Self>, project_id: i64, ordered_ids: Vec<i64>) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            let res = client.reorder_tabs(project_id, ordered_ids).await;
            if let Err(err) = res {
                tracing::warn!(?err, project_id, "reorder_tabs failed");
            }
        });
    }
}

fn stack_name(project_id: i64) -> String {
    format!("project-{project_id}")
}

/// Parse a notification row's item id (`notif:<tab_id>`) into the tab
/// id. Returns `None` for the `notif:none` sentinel and any malformed
/// id, so the confirm handler treats those as no-ops.
fn notif_tab_id(item_id: &str) -> Option<i64> {
    item_id.strip_prefix("notif:").and_then(|s| s.parse().ok())
}

/// Tilde-abbreviated cwd of `ui`'s currently selected tab (or the
/// first tab if no selection exists yet). Empty string when the
/// project has no attached tabs — caller uses that as the "subtitle
/// goes blank" signal.
/// New-tab cwd precedence: native shell cwd (current, local) → the
/// OSC 7-tracked cwd. An empty result lets `LocalClient::open_tab`
/// resolve the project cwd → $HOME. Pure + unit-testable.
fn resolve_launch_cwd(native: Option<String>, tracked: &str) -> String {
    match native {
        Some(n) if !n.is_empty() => n,
        _ => tracked.to_string(),
    }
}

fn active_tab_cwd(ui: &ProjectUi) -> String {
    // adw::TabView::selected_page returns the currently focused tab.
    // We resolve that back through `parse_tab_id_from_page` (same
    // path the close-page handler uses) to find the matching TabUi.
    //
    // Use `try_borrow` here: AdwTabView fires `selected_page_notify`
    // synchronously during operations like `tab_view.append`,
    // `set_selected_page`, and `close_page`, and the caller may
    // already hold a borrow of `ui.tabs` (e.g. the `TabDeleted`
    // arm holds `borrow_mut` while it calls `close_page`). Returning
    // an empty subtitle when a borrow is contended is the right
    // graceful-degrade — the very next non-contended notify or
    // event will recompute. The previous version's bare `.borrow()`
    // panicked under exactly this re-entrant signal pattern.
    if let Some(page) = ui.tab_view.selected_page() {
        if let Some(tab_id) = parse_tab_id_from_page(&page) {
            if let Ok(tabs) = ui.tabs.try_borrow() {
                if let Some(tab_ui) = tabs.get(&tab_id) {
                    return tilde_abbreviate(&tab_ui.cwd.borrow());
                }
            } else {
                return String::new();
            }
        }
    }
    // Fallback: walk the TabView's page list in **display order** and
    // pick the first page whose tab id we have a TabUi for. The
    // previous version used `ui.tabs.borrow().iter().next()` which
    // pulls an arbitrary entry from the HashMap — when `selected_page`
    // briefly returns None (e.g. mid-close-page transition) the
    // subtitle could flicker to a random tab's cwd. CodeRabbit caught
    // this on PR #61.
    let pages = ui.tab_view.pages();
    let n = pages.n_items();
    let Ok(tabs) = ui.tabs.try_borrow() else {
        return String::new();
    };
    for i in 0..n {
        let Some(obj) = pages.item(i) else { continue };
        let Ok(page) = obj.downcast::<libadwaita::TabPage>() else {
            continue;
        };
        let Some(tab_id) = parse_tab_id_from_page(&page) else {
            continue;
        };
        if let Some(tab_ui) = tabs.get(&tab_id) {
            return tilde_abbreviate(&tab_ui.cwd.borrow());
        }
    }
    String::new()
}

/// Replace `$HOME` prefix in a path with `~`. Used by the OSC 7
/// (cwd) tab pill label and the headerbar subtitle (M2) so
/// home-rooted dirs don't dominate the chrome.
fn tilde_abbreviate(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) else {
        return path.to_string();
    };
    tilde_abbreviate_with_home(path, &home)
}

/// Pure variant of `tilde_abbreviate` that takes the home directory
/// explicitly. Lets tests pass a fixed `home` instead of mutating
/// `std::env::HOME` (which would race with parallel tests; flagged
/// by CodeRabbit on the initial M2 commit).
fn tilde_abbreviate_with_home(path: &str, home: &str) -> String {
    if path == home {
        return "~".to_string();
    }
    if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
        return format!("~/{rest}");
    }
    path.to_string()
}

/// M9.5 dedupe rule for `attach_existing_tab`: skip the spawn if
/// the tab id is already either fully attached (`tabs`) or
/// in-flight (`pending_attaches`). The pending set covers the
/// async window between the optimistic RPC-return attach and the
/// WatchEvents `TabOpened` arm — without it, both paths
/// race-pass the `tabs.contains_key` check and double-spawn.
fn is_already_attached_or_pending<T>(
    tabs: &HashMap<i64, T>,
    pending: &HashSet<i64>,
    tab_id: i64,
) -> bool {
    tabs.contains_key(&tab_id) || pending.contains(&tab_id)
}

/// M9.5 cascade rule for `ProjectDeleted`: when the active
/// project was the one deleted, the lowest-id remaining project
/// becomes active. Returns `None` when the workspace is empty
/// (caller closes the window). Lowest-id (not last-active, not
/// random) so the choice is deterministic across UI clients.
fn pick_next_active_project<T>(projects: &HashMap<i64, T>) -> Option<i64> {
    projects.keys().copied().min()
}

/// Session-restore rule: the `(cwd, title, user_titled)` specs to open
/// for a project. Each saved tab maps to one spec; a project with
/// **no** saved tabs seeds a single default tab (empty cwd + title →
/// resolved/derived by the open path; `user_titled=false` since the
/// caller didn't pick a name). Pure so the seed-one-when-empty rule
/// is unit-tested without the GTK bootstrap.
fn restore_open_specs(saved: &[RestoreTab]) -> Vec<(String, String, bool)> {
    if saved.is_empty() {
        vec![(String::new(), String::new(), false)]
    } else {
        saved
            .iter()
            .map(|t| (t.cwd.clone(), t.title.clone(), t.user_titled))
            .collect()
    }
}

/// M10 sidebar-reorder pure math. Given a source row sitting at
/// `source_idx` and the user's desired insertion point in the
/// *with-source* visual order (`raw_target_idx`), return the
/// listbox `Insert` position the row should be moved to. Returns
/// `None` when the move would be a no-op (the drag lands on the
/// source's own slot — either side of itself). Off-by-one is
/// load-bearing here: when `raw_target_idx > source_idx`, removing
/// the source first shifts every later index down by one, so the
/// insert position is `raw_target_idx - 1`. The table-driven test
/// below exercises the boundary cases.
fn compute_insert_idx(source_idx: usize, raw_target_idx: usize) -> Option<usize> {
    if raw_target_idx == source_idx || raw_target_idx == source_idx + 1 {
        return None;
    }
    if raw_target_idx > source_idx {
        return Some(raw_target_idx - 1);
    }
    Some(raw_target_idx)
}

/// M9.5 one-shot test-and-drain for the server-driven-close
/// marker. Returns true if the marker was set (caller should
/// skip the CloseTab RPC); also clears the marker so a second
/// close attempt without re-insertion WILL send the RPC. The
/// drain semantics matter — a leftover marker would silently
/// swallow a legitimate user-driven close.
fn drain_server_driven_marker(set: &RefCell<HashSet<i64>>, tab_id: i64) -> bool {
    set.borrow_mut().remove(&tab_id)
}

/// Persist `theme = <name>` to the user's config file. Returns the
/// IO error to the caller (which logs once at the user-action
/// boundary), per the repo convention "return errors rather than
/// logging-and-swallowing them; log at the boundary that handles
/// the error". A failed write must not crash the UI; the in-memory
/// selection still works for the rest of the session.
///
/// Returns `Ok(())` if `$HOME` and `$ROOST_CONFIG` are both unset
/// (no config path is resolvable) — there's nothing to persist to
/// in that case, and bubbling the absence up as an error would be
/// noise.
fn write_back_theme(name: &str) -> std::io::Result<()> {
    let Some(path) = config::config_path() else {
        return Ok(());
    };
    config::set_key(&path, "theme", name)
}

/// Persist `font-family = "<name>"` to the user's config file.
/// Wraps the value in double quotes since family names commonly
/// contain spaces ("JetBrains Mono"); the parser strips them back
/// off on read.
fn write_back_font_family(name: &str) -> std::io::Result<()> {
    let Some(path) = config::config_path() else {
        return Ok(());
    };
    let quoted = format!("\"{}\"", name);
    config::set_key(&path, "font-family", &quoted)
}

/// Persist `font-size = <pt>` to the user's config file. Whole
/// values are written as integers ("14") rather than floats ("14.0")
/// to keep the file human-readable.
fn write_back_font_size(size_pt: f64) -> std::io::Result<()> {
    let Some(path) = config::config_path() else {
        return Ok(());
    };
    let formatted = format_font_size(size_pt);
    config::set_key(&path, "font-size", &formatted)
}

/// Format a font size in points for the config file. Whole numbers
/// render as integers; non-whole values keep up to two decimal places
/// so a `font-size = 14.5` round-trip cleanly. Split out for testing
/// (no I/O).
fn format_font_size(size_pt: f64) -> String {
    if (size_pt.round() - size_pt).abs() < 0.001 {
        format!("{}", size_pt.round() as i64)
    } else {
        // Two decimals is plenty for point sizes; trim trailing zeros.
        let s = format!("{:.2}", size_pt);
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        trimmed.to_string()
    }
}

pub fn parse_tab_id_from_page(page: &libadwaita::TabPage) -> Option<i64> {
    let name = page.child().widget_name().to_string();
    name.strip_prefix("tab-").and_then(|n| n.parse().ok())
}

/// Render an `Accel` as a platform-appropriate shortcut label (Cmd
/// glyphs on macOS, `Alt+Shift+P` text on Linux) for the palette's
/// right-hand hint. `None` when the accel can't be parsed. Routes
/// through `accelerator_parse` + `accelerator_get_label` so GTK owns
/// the platform formatting.
fn accel_label(accel: &Accel) -> Option<String> {
    let mut s = String::new();
    if accel.modifiers.contains(AccelMods::CTRL) {
        s.push_str("<Control>");
    }
    if accel.modifiers.contains(AccelMods::SHIFT) {
        s.push_str("<Shift>");
    }
    if accel.modifiers.contains(AccelMods::ALT) {
        s.push_str("<Alt>");
    }
    if accel.modifiers.contains(AccelMods::SUPER) {
        s.push_str("<Meta>");
    }
    s.push_str(&accel.key);
    let (key, mods) = gtk4::accelerator_parse(&s)?;
    Some(gtk4::accelerator_get_label(key, mods).to_string())
}

/// Convert our `Accel` to a `gtk::ShortcutTrigger` that the GTK
/// shortcut controller understands.
fn build_shortcut_trigger(accel: &Accel) -> gtk4::ShortcutTrigger {
    let mut s = String::new();
    if accel.modifiers.contains(AccelMods::CTRL) {
        s.push_str("<Control>");
    }
    if accel.modifiers.contains(AccelMods::SHIFT) {
        s.push_str("<Shift>");
    }
    if accel.modifiers.contains(AccelMods::ALT) {
        s.push_str("<Alt>");
    }
    if accel.modifiers.contains(AccelMods::SUPER) {
        s.push_str("<Meta>");
    }
    s.push_str(&accel.key);
    gtk4::ShortcutTrigger::parse_string(&s).unwrap_or_else(|| {
        // Parse fail = a programmer error in the binding table.
        // Fall back to a never-firing trigger built by GTK itself.
        gtk4::ShortcutTrigger::parse_string("<Control><Shift>F24")
            .expect("fallback shortcut trigger parse")
    })
}

#[cfg(test)]
mod tests {
    use super::{
        compute_insert_idx, drain_server_driven_marker, format_font_size,
        is_already_attached_or_pending, notif_tab_id, pick_next_active_project, resolve_launch_cwd,
        restore_open_specs, tilde_abbreviate_with_home, RestoreTab,
    };
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    /// The notification-row id parser: `notif:<tab>` → the tab id; the
    /// `notif:none` empty sentinel + malformed ids → `None` (so confirm
    /// treats them as no-ops rather than jumping to a bogus tab).
    #[test]
    fn notif_tab_id_parses_only_well_formed_ids() {
        assert_eq!(notif_tab_id("notif:42"), Some(42));
        assert_eq!(notif_tab_id("notif:-3"), Some(-3));
        assert_eq!(notif_tab_id("notif:none"), None);
        assert_eq!(notif_tab_id("notif:"), None);
        assert_eq!(notif_tab_id("new_tab"), None);
        assert_eq!(notif_tab_id("notif:1.5"), None);
    }

    /// `restore_open_specs` encodes the seed-one-when-empty rule: a
    /// project with saved tabs re-opens exactly those (cwd + title +
    /// user_titled in order); a project with none seeds a single
    /// default tab (`user_titled=false` since nobody picked a name).
    #[test]
    fn restore_open_specs_seeds_one_when_empty() {
        assert_eq!(
            restore_open_specs(&[]),
            vec![(String::new(), String::new(), false)]
        );
        let saved = vec![
            RestoreTab {
                cwd: "/a".into(),
                title: "first".into(),
                user_titled: false,
            },
            RestoreTab {
                cwd: "/b".into(),
                title: "docs".into(),
                user_titled: true,
            },
        ];
        assert_eq!(
            restore_open_specs(&saved),
            vec![
                ("/a".to_string(), "first".to_string(), false),
                ("/b".to_string(), "docs".to_string(), true),
            ]
        );
    }

    /// `tilde_abbreviate_with_home` is the headerbar-subtitle (M2)
    /// helper that also powers the OSC 7 tab-pill label. Keep its
    /// contract pinned so a future "shorten my path" optimisation
    /// doesn't break what the chrome relies on. We test the
    /// pure-function variant directly so the test doesn't mutate
    /// `std::env::HOME` (which would race with parallel tests —
    /// CodeRabbit caught this on the initial M2 commit).
    #[test]
    fn tilde_abbreviate_replaces_home_prefix() {
        let home = "/Users/test";

        assert_eq!(tilde_abbreviate_with_home("/Users/test", home), "~");
        assert_eq!(
            tilde_abbreviate_with_home("/Users/test/projects/roost", home),
            "~/projects/roost"
        );
        // Non-home paths pass through unchanged.
        assert_eq!(tilde_abbreviate_with_home("/etc/hosts", home), "/etc/hosts");
        // Paths that share a prefix but aren't actually under HOME
        // (e.g. `/Users/testbed` when HOME=`/Users/test`) must NOT
        // be tilde-abbreviated.
        assert_eq!(
            tilde_abbreviate_with_home("/Users/testbed", home),
            "/Users/testbed"
        );
    }

    /// `is_already_attached_or_pending` is the dedupe rule that
    /// guards `attach_existing_tab` against the M9.5 Cmd+T
    /// double-spawn race. Pin the EITHER/OR semantics so a future
    /// "optimisation" that drops one of the two checks regresses
    /// the race fix.
    #[test]
    fn dedupe_skips_when_already_attached_or_pending() {
        let mut tabs: HashMap<i64, ()> = HashMap::new();
        let mut pending: HashSet<i64> = HashSet::new();

        // Neither set contains the id → don't skip (proceed to spawn).
        assert!(!is_already_attached_or_pending(&tabs, &pending, 7));

        // Live tab → skip.
        tabs.insert(7, ());
        assert!(is_already_attached_or_pending(&tabs, &pending, 7));
        tabs.clear();

        // In-flight tab (between the optimistic attach and
        // session_handle.await resolving) → skip.
        pending.insert(7);
        assert!(is_already_attached_or_pending(&tabs, &pending, 7));

        // Belt-and-braces: both sets carrying the id → skip.
        tabs.insert(7, ());
        assert!(is_already_attached_or_pending(&tabs, &pending, 7));

        // Unrelated id → don't skip.
        assert!(!is_already_attached_or_pending(&tabs, &pending, 99));
    }

    /// `pick_next_active_project` encodes the M9.5 cascade rule:
    /// lowest remaining id when the active project was deleted,
    /// `None` when the workspace is empty (caller closes the
    /// window). Pin "lowest id" so a future change to "last
    /// active" or "random" breaks loudly.
    #[test]
    fn next_active_picks_lowest_id_or_none_when_empty() {
        let empty: HashMap<i64, ()> = HashMap::new();
        assert_eq!(pick_next_active_project(&empty), None);

        let mut projects: HashMap<i64, ()> = HashMap::new();
        projects.insert(3, ());
        projects.insert(7, ());
        projects.insert(2, ());
        assert_eq!(pick_next_active_project(&projects), Some(2));

        let mut single: HashMap<i64, ()> = HashMap::new();
        single.insert(42, ());
        assert_eq!(pick_next_active_project(&single), Some(42));
    }

    /// `compute_insert_idx` is the off-by-one math for sidebar
    /// drag-reorder. The raw target index is computed with the
    /// source row still in place; when we remove-then-insert
    /// past the source, every index after the source shifts
    /// down by one. The table walks the full source-position ×
    /// raw-target matrix so this UI and the Mac UI stay
    /// byte-for-byte equivalent on the reorder math.
    #[test]
    fn compute_insert_idx_matches_reference_table() {
        // (source_idx, raw_target_idx, expected) where expected
        // is `None` for a no-op or `Some(insert_idx)`.
        let cases: &[(usize, usize, Option<usize>)] = &[
            // Source at 0 in a list of 4. rawTarget covers 0..=4.
            (0, 0, None),
            (0, 1, None),
            (0, 2, Some(1)),
            (0, 3, Some(2)),
            (0, 4, Some(3)),
            // Source at 1.
            (1, 0, Some(0)),
            (1, 1, None),
            (1, 2, None),
            (1, 3, Some(2)),
            (1, 4, Some(3)),
            // Source at 2.
            (2, 0, Some(0)),
            (2, 1, Some(1)),
            (2, 2, None),
            (2, 3, None),
            (2, 4, Some(3)),
            // Source at 3 (last). Tail-drop is a no-op.
            (3, 0, Some(0)),
            (3, 1, Some(1)),
            (3, 2, Some(2)),
            (3, 3, None),
            (3, 4, None),
        ];
        for &(src, raw, want) in cases {
            let got = compute_insert_idx(src, raw);
            assert_eq!(
                got, want,
                "compute_insert_idx(src={src}, raw={raw}) = {got:?}, want {want:?}"
            );
        }
    }

    /// `drain_server_driven_marker` is one-shot: it returns
    /// whether the marker was present AND clears it. A leftover
    /// marker would silently swallow the next user-driven close
    /// (× button or Cmd+W). Also pins that an unrelated id
    /// doesn't return true — that would skip a legitimate RPC.
    #[test]
    fn server_driven_marker_drains_on_first_check() {
        let set: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
        set.borrow_mut().insert(5);

        // First check: marker present, returns true, drains.
        assert!(drain_server_driven_marker(&set, 5));
        assert!(!set.borrow().contains(&5));

        // Second check on the same id: marker gone, returns
        // false — caller now correctly sends the CloseTab RPC.
        assert!(!drain_server_driven_marker(&set, 5));

        // Unrelated id: never present, returns false.
        set.borrow_mut().insert(5);
        assert!(!drain_server_driven_marker(&set, 99));
        // The unrelated query didn't drain the real marker.
        assert!(set.borrow().contains(&5));
    }

    #[test]
    fn resolve_launch_cwd_prefers_native() {
        assert_eq!(resolve_launch_cwd(Some("/n".into()), "/t"), "/n");
        // Empty/absent native falls back to the tracked cwd.
        assert_eq!(resolve_launch_cwd(Some(String::new()), "/t"), "/t");
        assert_eq!(resolve_launch_cwd(None, "/t"), "/t");
        // Both empty stays empty (open_tab then resolves project → $HOME).
        assert_eq!(resolve_launch_cwd(None, ""), "");
    }

    #[test]
    fn format_font_size_whole_renders_as_integer() {
        assert_eq!(format_font_size(14.0), "14");
        assert_eq!(format_font_size(8.0), "8");
        // Floating-point fuzz like 14.0000000001 still rounds.
        assert_eq!(format_font_size(14.0 + f64::EPSILON), "14");
    }

    #[test]
    fn format_font_size_keeps_decimals_when_needed() {
        assert_eq!(format_font_size(14.5), "14.5");
        // Trailing zeros are trimmed (no "14.50").
        assert_eq!(format_font_size(14.50), "14.5");
        // Two-decimal precision is preserved.
        assert_eq!(format_font_size(13.25), "13.25");
    }
}
