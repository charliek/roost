//! Top-level App: window, sidebar, per-project tab views.
//!
//! Holds the shared `RoostClient`, the WatchEvents subscription, and
//! per-project / per-tab UI state. Sidebar = `gtk::ListBox` of project
//! rows on the left. Right pane = `gtk::Stack` of `adw::TabView`s,
//! one per project, swapped when the sidebar selection changes.
//! Mirrors the Go binary's `cmd/roost/app.go` widget tree shape.
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
use libadwaita::{ApplicationWindow, HeaderBar, TabView, WindowTitle};
use roost_ipc::messages::{Project, Tab};
use tokio::runtime::Handle;

use roost_linux::daemon::WorkspaceEvent;
use roost_linux::local_client::LocalClient;
use roost_linux::reconcile;

use crate::cell_metrics::DEFAULT_FONT_SIZE_PT;
use crate::config::RoostConfig;
use crate::events;
use crate::keybind::{canonicalize_bindings, default_bindings, Accel, AccelMods, KeybindAction};
use crate::palette::{command_items, PaletteCommands, PaletteFrame, PaletteItem};
use crate::palette_ui::{PaletteBehavior, PaletteOutcome, PaletteOverlay, TOP_GAP};
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
    /// AdwTabBar — held so the M9 rename popover can anchor here
    /// (under the selected pill) instead of at the bottom of the
    /// TabView's terminal area.
    tab_bar: libadwaita::TabBar,
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
    /// `adw::WindowTitle` widget in the headerbar. Title = active
    /// project name, subtitle = active tab's cwd (tilde-abbreviated).
    /// Mirrors the Mac UI's `updateWindowTitle` flow.
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
    /// The open command palette overlay, or `None` when closed.
    palette: RefCell<Option<crate::palette_ui::PaletteOverlay>>,
    /// Optional font-family override from config.
    font_family: Option<String>,
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
    /// when no drag is in progress). Read by the sidebar drop
    /// target's motion handler to know which row to live-shuffle.
    /// Set in the drag-source's drag-begin, cleared in drag-end.
    dragged_project_id: RefCell<Option<i64>>,
    /// M10: snapshot of the sidebar order taken at drag-begin.
    /// Drag-end rolls back to this if the drop didn't persist
    /// (drag cancelled outside the sidebar, or `ReorderProjects`
    /// RPC failed). Mirrors the Go binary's `dragOriginalOrder`.
    drag_original_order: RefCell<Vec<i64>>,
    /// M10: set true once `ReorderProjects` persists (or a no-op
    /// drop is acknowledged). Drag-end consults this to decide
    /// whether to roll back. Distinct from `gdk::DragAction::MOVE`
    /// completion since GTK's "drop succeeded" signal covers
    /// transport, not our application-level persistence.
    drop_occurred: RefCell<bool>,
    /// True while `reconcile_to_snapshot` is applying programmatic
    /// tab reorders. The `connect_page_reordered` handler checks this
    /// and skips firing a `ReorderTabs` RPC, so a resync that
    /// re-sorts pills doesn't echo spurious mutations back to the
    /// workspace. (The same primitive will gate applying a remote
    /// reorder when cross-client convergence lands.)
    suppress_tab_reorder_echo: RefCell<bool>,
}

/// Bundled chrome stylesheet. Ported from `cmd/roost/style.css`;
/// kept in sync verbatim with the Go binary so the two UIs feel
/// identical. Loaded once at App::new and applied via the display's
/// shared style context so it composes with the user's libadwaita
/// theme rather than replacing it.
const STYLE_CSS: &str = include_str!("resources/style.css");

/// Bundled chrome icons. Vendored from `cmd/roost/icons/` (which in
/// turn comes from upstream Adwaita) so the GTK app doesn't depend
/// on the user's system `adwaita-icon-theme` being installed —
/// matches the Go binary's icons.gresource approach but skips the
/// gresource compilation step (one less build-tool dependency) by
/// embedding the SVGs directly into the binary via `include_bytes!`.
const ICON_FOLDER_SYMBOLIC: &[u8] = include_bytes!("resources/icons/folder-symbolic.svg");
const ICON_SIDEBAR_SHOW_SYMBOLIC: &[u8] =
    include_bytes!("resources/icons/sidebar-show-symbolic.svg");
const ICON_TAB_NEW_SYMBOLIC: &[u8] = include_bytes!("resources/icons/tab-new-symbolic.svg");

/// Per-tab status indicator icons (M7). Vendored from cmd/roost
/// alongside the chrome icons so the GTK app doesn't depend on any
/// system icon-theme search path for these — same trade-off as the
/// M5 chrome icons.
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

/// Apply the per-tab status indicator icon to `page`. Mirrors the
/// Go binary's `cmd/roost/indicator.go` cache. `TabState::None`
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

impl App {
    /// Build the window + start the daemon bootstrap. Returns an
    /// `Rc<App>` so closures can hold references back into the App
    /// for event dispatch.
    pub fn new(
        app: &libadwaita::Application,
        rt: Handle,
        client: LocalClient,
        activate_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
        screenshot_rx: Option<
            tokio::sync::mpsc::UnboundedReceiver<roost_linux::ipc::ScreenshotRequest>,
        >,
    ) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .default_width(1100)
            .default_height(700)
            .title("Roost (Linux)")
            .build();

        // M9.5: force the libadwaita color scheme to dark. The Go
        // binary does the same on its `adw::StyleManager` (a
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

        let header = HeaderBar::new();
        // `adw::WindowTitle` is the libadwaita 1.x analog of NSWindow's
        // title + subtitle. Title = project name (or status during
        // bootstrap), subtitle = active tab's cwd. Matches the Mac UI's
        // two-line title format and the Go binary's `HeaderBar.WindowTitle`.
        let window_title = WindowTitle::new("Roost", "connecting…");
        header.set_title_widget(Some(&window_title));

        // Headerbar buttons: folder picker (left) + sidebar toggle
        // (left) + `+ Tab` (right). Matches the Go binary's
        // `cmd/roost/app.go` chrome — same icons, same positions.
        // Wired below after `app_struct` exists so the handlers can
        // capture an `Rc<App>` clone.
        let folder_button = gtk4::Button::builder()
            .child(&gtk4::Image::from_gicon(&embedded_icon(
                ICON_FOLDER_SYMBOLIC,
            )))
            .css_classes(["flat"])
            .tooltip_text("New project from folder…")
            .build();
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
        header.pack_start(&folder_button);
        header.pack_start(&sidebar_toggle_button);
        header.pack_end(&new_tab_button);

        // Sidebar: vertical Box of [section header] / [scrolled project
        // list] / [`+ Project` footer button]. Matches the Go binary's
        // `cmd/roost/app.go` sidebar layout verbatim — header label,
        // list, button — so users moving between the two UIs find the
        // same affordances in the same places.
        let sidebar_header = gtk4::Label::builder()
            .label("Projects")
            .halign(gtk4::Align::Start)
            .css_classes(["sidebar-section-header"])
            .build();

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

        // `pill` for the libadwaita rounded-rect look; drop the
        // `flat` class so the button reads as a discrete affordance
        // with a visible chip rather than text-only. Matches the
        // Go binary's `+ Project` button which renders with the
        // default libadwaita button background. M9.5.
        let new_project_button = gtk4::Button::builder()
            .label("+ Project")
            .css_classes(["roost-add-project", "pill"])
            .margin_top(4)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .halign(gtk4::Align::Fill)
            .build();

        let sidebar_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .width_request(220)
            .build();
        sidebar_box.append(&sidebar_header);
        sidebar_box.append(&sidebar_scroll);
        sidebar_box.append(&new_project_button);

        // Right pane: a Stack of per-project AdwTabView widgets.
        let tab_stack = gtk4::Stack::builder().hexpand(true).vexpand(true).build();

        let paned = gtk4::Paned::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .resize_start_child(false)
            .shrink_start_child(false)
            .position(220)
            .start_child(&sidebar_box)
            .end_child(&tab_stack)
            .build();

        // Wrap the content below the header in a `gtk::Overlay` so the
        // command palette card can float centered over the whole
        // window (sidebar + tabs), pinned just under the tab bar.
        let content_overlay = gtk4::Overlay::builder().child(&paned).build();

        let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        outer.append(&header);
        outer.append(&content_overlay);
        window.set_content(Some(&outer));

        // Load + apply user config now so the first TerminalView
        // gets the right theme + font.
        let cfg = RoostConfig::load_default();
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
            window_title: window_title.clone(),
            projects: RefCell::new(HashMap::new()),
            active_project_id: RefCell::new(0),
            theme: RefCell::new(theme),
            active_theme_name: RefCell::new(active_theme_name),
            theme_name_at_open: RefCell::new(None),
            content_overlay: content_overlay.clone(),
            palette: RefCell::new(None),
            font_family: cfg.font_family.clone(),
            font_size_pt: cfg.font_size,
            current_font_size_pt: RefCell::new(cfg.font_size.unwrap_or(DEFAULT_FONT_SIZE_PT)),
            server_driven_closes: RefCell::new(HashSet::new()),
            dragged_project_id: RefCell::new(None),
            drag_original_order: RefCell::new(Vec::new()),
            drop_occurred: RefCell::new(false),
            suppress_tab_reorder_echo: RefCell::new(false),
        });
        // M10: a single drop target on the sidebar listbox owns the
        // motion (live-shuffle) + drop (persist) handling for all
        // project rows. Per-row drag sources are wired inside
        // `add_project_ui`. Mirrors the Go binary's
        // `installSidebarDropTarget` (cmd/roost/app.go:1011).
        app_struct.install_sidebar_drop_target();

        // Sidebar row selection → switch active project.
        sidebar.connect_row_selected({
            let app = app_struct.clone();
            move |_, row| {
                if let Some(row) = row {
                    let pid = row.index() as i64;
                    // Sidebar rows carry their project id in the
                    // `name` GObject property (set when we build
                    // the row); read it back here.
                    if let Some(name) = row.widget_name().to_string().strip_prefix("project-") {
                        if let Ok(id) = name.parse::<i64>() {
                            app.set_active_project(id);
                            return;
                        }
                    }
                    let _ = pid;
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

        // Headerbar buttons.
        // - Folder picker: pop a FileChooser, then create a project
        //   with the chosen cwd. Mirrors `cmd/roost/app.go`'s
        //   "new project from folder" path.
        folder_button.connect_clicked({
            let app = app_struct.clone();
            move |btn| {
                let parent = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok());
                // `gtk::FileDialog` is the gtk-4.10 successor to the
                // deprecated `FileChooserNative`. Async-only API:
                // `select_folder` takes a callback that receives the
                // chosen file (or a `Dismissed` error on cancel).
                let dialog = gtk4::FileDialog::builder()
                    .title("Choose project folder")
                    .modal(true)
                    .accept_label("Open")
                    .build();
                let app_for_pick = app.clone();
                dialog.select_folder(
                    parent.as_ref(),
                    None::<&gtk4::gio::Cancellable>,
                    move |result| match result {
                        Ok(file) => {
                            let Some(path) = file.path() else { return };
                            let path = path.to_string_lossy().to_string();
                            let app = app_for_pick.clone();
                            glib::spawn_future_local(async move {
                                if let Err(err) = app.create_new_project_with_cwd(&path).await {
                                    tracing::warn!(
                                        ?err,
                                        path = %path,
                                        "folder-picker new_project failed"
                                    );
                                }
                            });
                        }
                        Err(err) => {
                            // The user dismissing the dialog comes
                            // back as `Dismissed` — that's the happy
                            // cancel path, not a failure. Anything
                            // else is logged.
                            if !err.matches(gtk4::DialogError::Dismissed) {
                                tracing::warn!(?err, "folder-picker dialog failed");
                            }
                        }
                    },
                );
            }
        });
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

            // M8: per-tab context menu actions (Rename / Close).
            // Tab id is the action target.
            let rename_tab_action =
                gtk4::gio::SimpleAction::new("rename-tab", Some(&i64::static_variant_type()));
            let app_for_rename_tab = app_struct.clone();
            rename_tab_action.connect_activate(move |_, target| {
                if let Some(tab_id) = target.and_then(|t| t.get::<i64>()) {
                    // Resolve the tab's project_id by scanning.
                    let project_id = app_for_rename_tab.project_for_tab(tab_id);
                    if let Some(pid) = project_id {
                        app_for_rename_tab.begin_rename_tab(pid, tab_id);
                    }
                }
            });
            app.add_action(&rename_tab_action);

            let close_tab_action =
                gtk4::gio::SimpleAction::new("close-tab", Some(&i64::static_variant_type()));
            let app_for_close = app_struct.clone();
            close_tab_action.connect_activate(move |_, target| {
                if let Some(tab_id) = target.and_then(|t| t.get::<i64>()) {
                    if let Some(pid) = app_for_close.project_for_tab(tab_id) {
                        app_for_close.close_tab_async(pid, tab_id);
                    }
                }
            });
            app.add_action(&close_tab_action);
        }

        app_struct.window.present();

        // #6: a second launch that loses the single-instance flock
        // dials `app.activate`; the IPC handler forwards a unit here.
        // Raise + focus the window on the GTK main thread.
        if let Some(mut activate_rx) = activate_rx {
            let window = app_struct.window.clone();
            glib::spawn_future_local(async move {
                while activate_rx.recv().await.is_some() {
                    window.present();
                }
            });
        }

        // `app.screenshot`: the IPC handler forwards a render request +
        // a oneshot reply channel here. Render synchronously on the main
        // thread (GTK + the renderer are main-thread-only) and reply with
        // the PNG bytes.
        if let Some(mut screenshot_rx) = screenshot_rx {
            let window = app_struct.window.clone();
            glib::spawn_future_local(async move {
                while let Some(req) = screenshot_rx.recv().await {
                    let result = render_window_png(&window, req.scale);
                    let _ = req.reply.send(result);
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
            // M3b: tabs do NOT survive UI quits (no-session-restore
            // goal). At bootstrap there are never any tabs to
            // hydrate — the snapshot's `tabs` is always empty.
            // Keep the iterate-and-attach loop in case a future
            // change re-enables tab restore.
            for tab in &project.tabs {
                self.attach_existing_tab(tab.clone());
            }
        }
        if let Some(first) = projects.first() {
            self.set_active_project(first.id);
            if first.tabs.is_empty() {
                self.open_new_tab_in(first.id).await?;
            }
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
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
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
            .build();
        name_stack.add_named(&label, Some("label"));
        name_stack.add_named(&entry, Some("entry"));
        name_stack.set_visible_child_name("label");

        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&name_stack));
        row.set_widget_name(&format!("project-{}", project.id));
        self.sidebar.append(&row);
        // M10: wire the drag-source so this row can be picked up
        // and reordered. The matching listbox-level drop target
        // is installed once in App::new.
        self.install_row_drag_source(&row, project.id);

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

        // M9: double-click also enters rename mode (matches the Go
        // binary). Button 1 (primary) double-press.
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
        // Hook "close-page" so the daemon learns about the close,
        // even when the user clicks the [×] on a tab pill.
        tab_view.connect_close_page({
            let app = self.clone();
            let project_id = project.id;
            move |tv, page| {
                let tab_id = parse_tab_id_from_page(page);
                tracing::debug!(project_id, ?tab_id, "close-page signal");
                tv.close_page_finish(page, true);
                // Drop the local TabUi entry so cwd / state tracking
                // for the now-dead tab is freed and the headerbar
                // subtitle / rollup recompute don't try to look it
                // up. Snapshot the project ref out of the borrow
                // before touching `tabs` so we don't deadlock with
                // any subsequent borrow inside `close_tab_async`.
                if let Some(tab_id) = tab_id {
                    if let Some(ui) = app.projects.borrow().get(&project_id) {
                        ui.tabs.borrow_mut().remove(&tab_id);
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
        // arm handles cross-client convergence. Mirrors the Go
        // binary's `persistTabOrder` (cmd/roost/app.go:1061).
        tab_view.connect_page_reordered({
            let app = self.clone();
            let project_id = project.id;
            move |tv, _moved_page, _new_idx| {
                // Skip the echo when the reorder is our own
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
            move |_| {
                if *app.active_project_id.borrow() == project_id {
                    app.refresh_window_subtitle();
                }
            }
        });
        // M8: per-tab context menu (right-click a pill). AdwTabView
        // calls `setup-menu` with the page being right-clicked (or
        // `None` when the menu is being torn down) and expects us
        // to populate `tab_view.menu_model()` via `set_menu_model`.
        // The model uses detailed-action syntax to carry the tab id.
        let initial_menu = build_tab_context_menu(0);
        tab_view.set_menu_model(Some(&initial_menu));
        tab_view.connect_setup_menu(move |tv, page| {
            // `page == None` fires on close; nothing to do.
            let Some(page) = page else { return };
            if let Some(tab_id) = parse_tab_id_from_page(page) {
                let model = build_tab_context_menu(tab_id);
                tv.set_menu_model(Some(&model));
            }
        });

        // `autohide(false)`: libadwaita defaults to hiding the tab bar
        // when there's only one page (iOS-style minimal chrome). Both
        // the Go GTK binary and the Mac UI always show the strip, so
        // single-tab projects still get a visible tab pill + `×`
        // close affordance. Without this, users see only the
        // terminal area until a second tab is opened.
        let tab_bar = libadwaita::TabBar::builder()
            .view(&tab_view)
            .autohide(false)
            .build();
        let project_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        project_box.append(&tab_bar);
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
                tab_bar,
                tabs: RefCell::new(HashMap::new()),
                pending_attaches: RefCell::new(HashSet::new()),
            },
        );
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

    /// Open the inline rename popover for `(project_id, tab_id)`.
    /// Inline rename for tabs uses a `gtk::Popover` pointing at the
    /// tab pill (the Go binary's pattern) rather than `connect_setup_menu`
    /// (which is the per-page context menu, not an inline-edit hook).
    fn begin_rename_tab(self: &Rc<Self>, project_id: i64, tab_id: i64) {
        let projects = self.projects.borrow();
        let Some(ui) = projects.get(&project_id) else {
            return;
        };
        let tabs = ui.tabs.borrow();
        let Some(tab_ui) = tabs.get(&tab_id) else {
            return;
        };
        let current_title = tab_ui.page.title().to_string();

        // Select the target tab first. AdwTabView positions the
        // popover above the currently-selected pill (the only
        // pill-locating affordance the public API gives us), so if
        // the rename was invoked from a background tab's context
        // menu, the popover would otherwise float over the wrong
        // pill while the RPC silently renames the background tab.
        // CodeRabbit caught this on PR #63.
        ui.tab_view.set_selected_page(&tab_ui.page);

        let entry = gtk4::Entry::builder()
            .text(&current_title)
            .activates_default(true)
            .build();
        let popover = gtk4::Popover::builder()
            .has_arrow(true)
            .position(gtk4::PositionType::Bottom)
            .build();
        popover.set_child(Some(&entry));
        // Anchor at the AdwTabBar (the pill strip), positioned
        // pointing-down from the tab strip. M9.5: pre-fix, the
        // anchor was the TabView itself — whose visual centre is
        // the terminal area — so the popover floated halfway down
        // the window. AdwTabBar doesn't expose the individual
        // pill, but its widget bounds are the strip itself, so a
        // popover anchored here lands just below the pills.
        popover.set_parent(&ui.tab_bar);

        let app = self.clone();
        let popover_for_commit = popover.clone();
        entry.connect_activate(move |entry| {
            let new_title = entry.text().to_string();
            popover_for_commit.popdown();
            app.commit_rename_tab(tab_id, new_title);
        });
        let popover_for_cancel = popover.clone();
        let entry_keys = gtk4::EventControllerKey::new();
        entry_keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk4::gdk::Key::Escape {
                popover_for_cancel.popdown();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(entry_keys);
        popover.connect_closed(|p| p.unparent());
        popover.popup();
        entry.grab_focus();
        entry.select_region(0, -1);
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
        self.window_title.set_title(&ui.name);
        let subtitle = active_tab_cwd(ui);
        self.window_title.set_subtitle(&subtitle);
        // Sync sidebar selection without re-firing the handler.
        self.sidebar.select_row(Some(&ui.sidebar_row));
        drop(projects);
        *self.active_project_id.borrow_mut() = project_id;
        // M9.5: clicking a project shouldn't leave focus on the
        // sidebar row — the user wants to type into the terminal
        // immediately, not navigate the project list. Hand focus to
        // the active tab's TerminalView. Matches Mac UI behaviour
        // (`selectProject(id:)` ends with `terminalView.window?.makeFirstResponder`).
        self.focus_active_terminal();
    }

    /// Move keyboard focus to the active project's active tab's
    /// TerminalView. No-op if there is no active tab (e.g. the
    /// workspace just emptied). Used by `set_active_project`,
    /// `commit_rename_project`, `cancel_rename_project` and the
    /// post-attach hook so post-chrome-interaction the user can
    /// resume typing without an extra mouse click.
    fn focus_active_terminal(self: &Rc<Self>) {
        if let Some(view) = self.active_terminal_view() {
            view.widget().grab_focus();
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
        let Some(client) = self.client.borrow().clone() else {
            return Ok(());
        };
        let rt = self.rt.clone();
        // Empty cwd here delegates resolution to
        // `LocalClient::open_tab`, which prefers the project's
        // stored cwd, then $HOME, then `/`. CR (M4b3b review)
        // flagged the pre-existing HOME-only path: a project
        // pinned to a directory should open its tabs there, not
        // bounce them to the user's home.
        let cwd = String::new();
        let (tab, _rx) = rt
            .spawn(async move { client.open_tab(project_id, &cwd, 80, 24).await })
            .await
            .context("open_tab join")??;
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
        Ok(())
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
        let terminal = Rc::new(TerminalView::with_theme_and_font(
            self.theme.borrow().clone(),
            self.font_family.as_deref(),
            Some(*self.current_font_size_pt.borrow()),
        ));
        let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel::<TabOutput>();
        let Some(client_for_session) = self.client.borrow().clone() else {
            return;
        };
        let tab_id = tab.id;
        let rt = self.rt.clone();
        // M3b: subscribe to the in-process PtySupervisor. The
        // attach is synchronous (no gRPC dial), but we still hop
        // through `rt.spawn` so the drain task it kicks off lands
        // on the tokio runtime rather than the glib main loop.
        let supervisor = client_for_session.supervisor.clone();
        let session_handle =
            rt.spawn(async move { TabSession::attach(supervisor, tab_id, output_tx) });

        let page: libadwaita::TabPage = ui.tab_view.append(terminal.widget());
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
            glib::spawn_future_local(async move {
                let mut scanner = roost_osc::OscScanner::new();
                while let Some(msg) = output_rx.recv().await {
                    match msg {
                        TabOutput::Bytes(data) => {
                            let events = scanner.feed(&data);
                            for event in events {
                                app_for_osc.report_osc_event(tab_id, event);
                            }
                            terminal_for_drain.vt_write(&data);
                        }
                        TabOutput::Exit { status, reason } => {
                            tracing::info!(tab_id, status, %reason, "PTY exited");
                            // M9.5: close the page directly from the
                            // drain task on PTY exit. Mirrors the Go
                            // binary's `sess.onPTYExit = func() {
                            // view.ClosePage(page) }` pattern — a
                            // unified close path through the
                            // close-page signal handler instead of
                            // round-tripping through the daemon's
                            // TabDeletedEvent. The
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

        // Focus the new tab.
        ui.tab_view.set_selected_page(&page);
        terminal.widget().grab_focus();
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
                    }
                }
            }
            WorkspaceEvent::ProjectDeleted { project_id } => {
                let mut projects = self.projects.borrow_mut();
                if let Some(ui) = projects.remove(&project_id) {
                    self.sidebar.remove(&ui.sidebar_row);
                    self.tab_stack.remove(
                        &self
                            .tab_stack
                            .child_by_name(&stack_name(project_id))
                            .expect("tab stack child for project"),
                    );
                }
                let was_active = *self.active_project_id.borrow() == project_id;
                if was_active {
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
            }
            WorkspaceEvent::TabTitleChanged { tab_id, title } => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                        tab_ui.page.set_title(&title);
                    }
                }
            }
            WorkspaceEvent::TabCwdChanged { tab_id, cwd } => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                        let label = tilde_abbreviate(&cwd);
                        let current = tab_ui.page.title().to_string();
                        if current.starts_with("Tab ") {
                            tab_ui.page.set_title(&label);
                        }
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
                }
            }
            WorkspaceEvent::NotificationFired {
                tab_id,
                title,
                body,
            } => {
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
                            ui.tab_view.set_selected_page(&tab_ui.page);
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
                    let state = TabState::from_ipc(tab.state);
                    *tab_ui.state.borrow_mut() = state;
                    apply_indicator_icon(&tab_ui.page, state);
                    *tab_ui.cwd.borrow_mut() = tab.cwd.clone();
                    tab_ui.page.set_needs_attention(tab.has_notification);
                    *tab_ui.hook_active.borrow_mut() = tab.hook_active;
                    affected.insert(project.id);
                }
            }
        }
        for pid in &affected {
            self.refresh_rollup_for(*pid);
        }

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
                        ui.tab_view.set_selected_page(&tab_ui.page);
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
        // fires before responder chain) and the Go binary's same
        // pattern on its window-scoped ShortcutController.
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
        // palette toggle itself stays live (re-press is a no-op since
        // `show_command_palette` guards on an already-open palette).
        // GTK analog of the Swift app's `validateMenuItem` gate.
        if action != KeybindAction::CommandPalette && self.palette.borrow().is_some() {
            return;
        }
        match action {
            KeybindAction::CommandPalette => self.show_command_palette(),
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
            KeybindAction::DeleteProject => {
                let pid = *self.active_project_id.borrow();
                if pid != 0 {
                    self.confirm_and_delete_project(pid);
                }
            }
            KeybindAction::CycleTabPrev => self.cycle_tab(-1),
            KeybindAction::CycleTabNext => self.cycle_tab(1),
            KeybindAction::Copy => self.copy_active_selection(),
            KeybindAction::Paste => self.paste_into_active(),
            KeybindAction::ToggleSidebar => self.toggle_sidebar(),
            KeybindAction::FontIncrease => self.adjust_font_size(1.0),
            KeybindAction::FontDecrease => self.adjust_font_size(-1.0),
            KeybindAction::FontReset => {
                let baseline = self.font_size_pt.unwrap_or(DEFAULT_FONT_SIZE_PT);
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

    /// Resolve a tab id to its parent project id by scanning. M8's
    /// per-tab context-menu actions get the tab id from the action
    /// target but need the project id to dispatch close / rename.
    fn project_for_tab(self: &Rc<Self>, tab_id: i64) -> Option<i64> {
        let projects = self.projects.borrow();
        for (project_id, ui) in projects.iter() {
            if ui.tabs.borrow().contains_key(&tab_id) {
                return Some(*project_id);
            }
        }
        None
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
        if let Some(obj) = pages.item(target) {
            if let Ok(page) = obj.downcast::<libadwaita::TabPage>() {
                ui.tab_view.set_selected_page(&page);
            }
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
        let projects = self.projects.borrow();
        for ui in projects.values() {
            let tabs = ui.tabs.borrow();
            for tab_ui in tabs.values() {
                tab_ui
                    .view
                    .apply_font(self.font_family.as_deref(), Some(size_pt));
            }
        }
    }

    /// Switch the active theme at runtime and broadcast it to every
    /// open terminal (all tabs, all projects). Not persisted:
    /// `config.conf` still wins on next launch. New tabs read
    /// `self.theme` at spawn, so both confirm and revert propagate
    /// forward. Mirrors the Mac UI's `setActiveTheme`.
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

    // ----- Command palette (Cmd+Shift+P / Alt+Shift+P) -------------

    /// Open the command palette over the content overlay. Captures the
    /// theme active at open (for revert), builds the curated command
    /// frame with shortcut hints, and presents. No-op if already open.
    fn show_command_palette(self: &Rc<Self>) {
        if self.palette.borrow().is_some() {
            return;
        }
        *self.theme_name_at_open.borrow_mut() = Some(self.active_theme_name.borrow().clone());

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
        let items = command_items(|action| reverse.get(&action).and_then(accel_label));
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

    /// Top margin pinning the card under the tab bar: the active
    /// project's tab-bar height (falling back to ~46px before it's
    /// allocated) plus the visual gap.
    fn palette_top_margin(self: &Rc<Self>) -> i32 {
        let pid = *self.active_project_id.borrow();
        let projects = self.projects.borrow();
        let bar_h = projects
            .get(&pid)
            .map(|ui| ui.tab_bar.height())
            .filter(|h| *h > 0)
            .unwrap_or(46);
        bar_h + TOP_GAP
    }

    /// Palette teardown callback: clear the handle + the captured
    /// open-theme, then return focus to the active terminal.
    fn dismiss_palette(self: &Rc<Self>) {
        *self.palette.borrow_mut() = None;
        *self.theme_name_at_open.borrow_mut() = None;
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
    /// (highlight already applied it), Esc/dismiss reverts.
    fn theme_behavior(self: &Rc<Self>) -> PaletteBehavior {
        let weak_highlight = Rc::downgrade(self);
        let weak_cancel = Rc::downgrade(self);
        PaletteBehavior::new(|_| PaletteOutcome::Close)
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

    fn active_terminal_view(self: &Rc<Self>) -> Option<Rc<TerminalView>> {
        let pid = *self.active_project_id.borrow();
        let projects = self.projects.borrow();
        let ui = projects.get(&pid)?;
        let page = ui.tab_view.selected_page()?;
        let tab_id = parse_tab_id_from_page(&page)?;
        let view = ui.tabs.borrow().get(&tab_id).map(|t| t.view.clone());
        view
    }

    fn toggle_sidebar(self: &Rc<Self>) {
        // Hide the entire sidebar container — header + list + footer
        // button — so the Paned divider snaps to the left edge. Pre-M5
        // we toggled only the list, leaving the Projects header and
        // `+ Project` button orphaned in a thin strip.
        let visible = self.sidebar_box.is_visible();
        self.sidebar_box.set_visible(!visible);
    }

    fn switch_project_by_index(self: &Rc<Self>, n: usize) {
        if n == 0 {
            return;
        }
        let projects = self.projects.borrow();
        let mut ids: Vec<i64> = projects.keys().copied().collect();
        ids.sort();
        if let Some(&id) = ids.get(n - 1) {
            drop(projects);
            self.set_active_project(id);
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
        if let Some(obj) = pages.item((n - 1) as u32) {
            if let Ok(page) = obj.downcast::<libadwaita::TabPage>() {
                ui.tab_view.set_selected_page(&page);
            }
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
        // lands inside a shell. Mac UI + Go binary do this same
        // dance — clicking "+ Project" with no follow-up never
        // makes sense, and a bare project with no tabs renders an
        // empty terminal pane (the bug reported).
        self.open_new_tab_in(project.id).await?;
        Ok(())
    }

    /// Forward a parsed OSC event to the daemon. Mirrors the Mac
    /// UI's `RoostApp.reportOsc` path; the daemon decides whether
    /// to emit `TabTitleChanged` / `TabCwdChanged` /
    /// `NotificationEvent` / etc.
    fn report_osc_event(self: &Rc<Self>, tab_id: i64, event: roost_osc::OscEvent) {
        use roost_osc::OscEvent as E;
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
        let projects = self.projects.borrow();
        let mut hit: Option<(i64, libadwaita::TabPage)> = None;
        for (project_id, ui) in projects.iter() {
            if let Some(tab_ui) = ui.tabs.borrow().get(&tab_id) {
                hit = Some((*project_id, tab_ui.page.clone()));
                break;
            }
        }
        drop(projects);
        let Some((project_id, page)) = hit else {
            return;
        };
        self.set_active_project(project_id);
        let projects = self.projects.borrow();
        if let Some(ui) = projects.get(&project_id) {
            ui.tab_view.set_selected_page(&page);
        }
        // Bring the window forward.
        self.window.present();
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
    /// `"project-<id>"` (set in `add_project_ui`). Mirrors the Go
    /// binary's `sidebarOrder` (cmd/roost/app.go:989).
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
    /// `ProjectsReordered` events. Verbatim port of the Go
    /// binary's `applySidebarOrder` (cmd/roost/app.go:957).
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
    /// would land on the source's own slot. Mirrors the Go
    /// binary's `shuffleSidebarToward` (cmd/roost/app.go:907).
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
    /// return the row count ("insert at end"). Mirrors the Go
    /// binary's `rawTargetForY` (cmd/roost/app.go:1044).
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

    /// M10: attach a `gtk::DragSource` to a project row so it can
    /// be picked up. Dragged content is the project's int64 id.
    /// Visual feedback: the source row dims to 40% opacity while
    /// the drag is in flight (Go binary path; CSS `:drop(active)`
    /// styling was unreliable in our environment so we set
    /// opacity directly). The drop target on the listbox handles
    /// motion + persistence; this side just publishes the
    /// payload and tracks rollback snapshot.
    fn install_row_drag_source(self: &Rc<Self>, row: &gtk4::ListBoxRow, project_id: i64) {
        let src = gtk4::DragSource::new();
        src.set_actions(gtk4::gdk::DragAction::MOVE);
        // Suppress the drag while the user is renaming inline —
        // the entry needs to keep focus + own mouse input. We
        // detect "renaming" by checking the row's name-stack's
        // visible child (set by M9's begin_rename_project).
        let row_for_prepare = row.clone();
        src.connect_prepare(move |_, _, _| {
            if let Some(stack) = row_for_prepare
                .child()
                .and_then(|c| c.downcast::<gtk4::Stack>().ok())
            {
                if stack.visible_child_name().as_deref() == Some("entry") {
                    return None;
                }
            }
            Some(gtk4::gdk::ContentProvider::for_value(&glib::Value::from(
                project_id,
            )))
        });
        let row_for_begin = row.clone();
        let app_for_begin = self.clone();
        src.connect_drag_begin(move |_, _| {
            row_for_begin.set_opacity(0.4);
            *app_for_begin.dragged_project_id.borrow_mut() = Some(project_id);
            *app_for_begin.drag_original_order.borrow_mut() = app_for_begin.sidebar_order();
            *app_for_begin.drop_occurred.borrow_mut() = false;
        });
        let row_for_end = row.clone();
        let app_for_end = self.clone();
        src.connect_drag_end(move |_, _, _| {
            row_for_end.set_opacity(1.0);
            if !*app_for_end.drop_occurred.borrow() {
                let snapshot = app_for_end.drag_original_order.borrow().clone();
                app_for_end.apply_sidebar_order(&snapshot);
            }
            *app_for_end.dragged_project_id.borrow_mut() = None;
            app_for_end.drag_original_order.borrow_mut().clear();
            *app_for_end.drop_occurred.borrow_mut() = false;
        });
        row.add_controller(src);
    }

    /// M10: wire the listbox-level drop target. Motion live-
    /// shuffles the dragged row to the insertion point implied
    /// by the pointer y; drop persists the resulting order via
    /// `ReorderProjects` RPC (or skips the write when the order
    /// is unchanged). Rollback on RPC failure restores the
    /// drag-begin snapshot. Called once from `App::new`.
    fn install_sidebar_drop_target(self: &Rc<Self>) {
        let dst = gtk4::DropTarget::new(glib::types::Type::I64, gtk4::gdk::DragAction::MOVE);
        let app_for_motion = self.clone();
        dst.connect_motion(move |_, _, y| {
            if let Some(src_id) = *app_for_motion.dragged_project_id.borrow() {
                let raw = app_for_motion.raw_target_for_y(y);
                app_for_motion.shuffle_sidebar_toward(src_id, raw);
            }
            gtk4::gdk::DragAction::MOVE
        });
        let app_for_drop = self.clone();
        dst.connect_drop(move |_, value, _, _| {
            // The payload value is the project id we set in
            // `connect_prepare`. We don't strictly need to read
            // it here — the dragged id is also in
            // `dragged_project_id` — but failing the type check
            // is a quick way to reject foreign content (e.g. a
            // file drop from the file manager).
            if value.get::<i64>().is_err() {
                return false;
            }
            let current = app_for_drop.sidebar_order();
            let original = app_for_drop.drag_original_order.borrow().clone();
            if current == original {
                // Unchanged drop is still a successful drop —
                // drag-end mustn't roll back.
                *app_for_drop.drop_occurred.borrow_mut() = true;
                return true;
            }
            // Mark as persisted optimistically; rollback below
            // on RPC error reverts both the visual order AND
            // clears the flag so the drag-end roll-back logic
            // isn't double-fired.
            *app_for_drop.drop_occurred.borrow_mut() = true;
            app_for_drop.reorder_projects_async(current, original);
            true
        });
        self.sidebar.add_controller(dst);
    }

    /// M10: fire `ReorderProjects` RPC. On error, roll the
    /// sidebar back to `snapshot` so the visual order doesn't
    /// diverge from the daemon's persisted order. Mirrors the
    /// Go binary's drop handler error path
    /// (cmd/roost/app.go:1028-1032). The double-spawn
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

/// Tilde-abbreviated cwd of `ui`'s currently selected tab (or the
/// first tab if no selection exists yet). Empty string when the
/// project has no attached tabs — caller uses that as the "subtitle
/// goes blank" signal.
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

/// M10 sidebar-reorder pure math. Given a source row sitting at
/// `source_idx` and the user's desired insertion point in the
/// *with-source* visual order (`raw_target_idx`), return the
/// listbox `Insert` position the row should be moved to. Returns
/// `None` when the move would be a no-op (the drag lands on the
/// source's own slot — either side of itself). Off-by-one is
/// load-bearing here: when `raw_target_idx > source_idx`, removing
/// the source first shifts every later index down by one, so the
/// insert position is `raw_target_idx - 1`. Verbatim port of the
/// Go binary's `computeInsertIdx` (cmd/roost/app.go:936); the
/// table-driven test below mirrors `TestComputeInsertIdx` in
/// `cmd/roost/sidebar_reorder_test.go`.
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

pub fn parse_tab_id_from_page(page: &libadwaita::TabPage) -> Option<i64> {
    let name = page.child().widget_name().to_string();
    name.strip_prefix("tab-").and_then(|n| n.parse().ok())
}

/// Build the per-tab right-click menu model (Rename / Close) with
/// detailed action names carrying `tab_id` as the parameter. The
/// resulting model is fed to `adw::TabView::set_menu_model` from
/// `connect_setup_menu`.
fn build_tab_context_menu(tab_id: i64) -> gtk4::gio::Menu {
    let menu = gtk4::gio::Menu::new();
    menu.append(Some("Rename"), Some(&format!("app.rename-tab({tab_id})")));
    menu.append(Some("Close"), Some(&format!("app.close-tab({tab_id})")));
    menu
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
        compute_insert_idx, drain_server_driven_marker, is_already_attached_or_pending,
        pick_next_active_project, tilde_abbreviate_with_home,
    };
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

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
    /// down by one. Table mirrors the Go binary's
    /// `TestComputeInsertIdx` (cmd/roost/sidebar_reorder_test.go)
    /// — same source-position × raw-target matrix so the two
    /// UIs stay byte-for-byte equivalent on the reorder math.
    #[test]
    fn compute_insert_idx_matches_go_table() {
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
}
