//! Top-level App: window, sidebar, per-project tab views.
//!
//! Holds the shared `RoostClient`, the WatchEvents subscription, and
//! per-project / per-tab UI state. Sidebar = `gtk::ListBox` of project
//! rows on the left. Right pane = `gtk::Stack` of `adw::TabView`s,
//! one per project, swapped when the sidebar selection changes.
//! Mirrors the Go binary's `cmd/roost/app.go` widget tree shape.
//!
//! Reorder is wired RPC-side via commit 3 (ReorderTabs /
//! ReorderProjects); the gtk4-rs drag-source / drop-target hookups
//! land in a follow-up commit. Today the Linux UI converges via
//! WatchEvents whenever any client (CLI, Mac, or another Linux UI)
//! calls the reorder RPCs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Context;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita::prelude::*;
use libadwaita::{ApplicationWindow, HeaderBar, TabView, WindowTitle};
use tokio::runtime::Handle;

use roost_common::default_socket_path;
use roost_proto::v1::event::Kind as EventKind;
use roost_proto::v1::{Project, Tab};

use crate::client::RoostClient;
use crate::config::RoostConfig;
use crate::events;
use crate::keybind::{canonicalize_bindings, default_bindings, Accel, AccelMods, KeybindAction};
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
    /// Tab id → (TerminalView, TabSession).
    tabs: RefCell<HashMap<i64, TabUi>>,
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
    client: RefCell<Option<RoostClient>>,
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
    /// TerminalView so cells use the same palette.
    theme: Theme,
    /// Optional font-family override from config.
    font_family: Option<String>,
    /// Optional font-size override from config (points).
    font_size_pt: Option<f64>,
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

impl App {
    /// Build the window + start the daemon bootstrap. Returns an
    /// `Rc<App>` so closures can hold references back into the App
    /// for event dispatch.
    pub fn new(app: &libadwaita::Application, rt: Handle) -> Rc<Self> {
        let window = ApplicationWindow::builder()
            .application(app)
            .default_width(1100)
            .default_height(700)
            .title("Roost (Linux)")
            .build();

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

        let new_project_button = gtk4::Button::builder()
            .label("+ Project")
            .css_classes(["roost-add-project", "flat"])
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

        let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        outer.append(&header);
        outer.append(&paned);
        window.set_content(Some(&outer));

        // Load + apply user config now so the first TerminalView
        // gets the right theme + font.
        let cfg = RoostConfig::load_default();
        let theme = match cfg.theme_name.as_deref() {
            Some(name) => Theme::load_bundled(name),
            None => Theme::roost_dark(),
        };

        let app_struct = Rc::new(App {
            window,
            client: RefCell::new(None),
            rt: rt.clone(),
            sidebar: sidebar.clone(),
            sidebar_box: sidebar_box.clone(),
            tab_stack: tab_stack.clone(),
            window_title: window_title.clone(),
            projects: RefCell::new(HashMap::new()),
            active_project_id: RefCell::new(0),
            theme,
            font_family: cfg.font_family.clone(),
            font_size_pt: cfg.font_size,
        });

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

    /// One-shot bootstrap: connect, identify, build initial project
    /// list, subscribe to WatchEvents, open a tab in the first
    /// project if none exist.
    async fn bootstrap(self: &Rc<Self>) -> anyhow::Result<()> {
        let socket = default_socket_path().context("default_socket_path")?;
        let rt = self.rt.clone();
        let client = rt
            .spawn(async move { RoostClient::connect(socket).await })
            .await
            .context("connect join")??;
        *self.client.borrow_mut() = Some(client.clone());

        let mut client_for_setup = client.clone();
        let (id, projects) = rt
            .spawn(async move {
                let id = client_for_setup.identify().await?;
                let projects = client_for_setup.list_projects().await?;
                anyhow::Ok((id, projects))
            })
            .await
            .context("setup join")??;

        // Transitional title until `set_active_project` lands the
        // first project's name. The subtitle holds the daemon version
        // so the user can confirm the round-trip succeeded at a glance.
        self.window_title.set_title("Roost");
        self.window_title
            .set_subtitle(&format!("daemon v{}", id.daemon_version));

        // Materialize the project list. If empty, create a default
        // "roost-linux" project so the user has something to look at.
        let projects = if projects.is_empty() {
            let mut client_for_create = client.clone();
            let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let project = rt
                .spawn(async move { client_for_create.create_project("roost-linux", &cwd).await })
                .await
                .context("create_project join")??;
            vec![project]
        } else {
            projects
        };

        for project in &projects {
            self.add_project_ui(project);
            // Attach existing tabs for every project, not just the
            // first one. Persisted state across daemon restarts (or
            // cross-client opens) puts tabs in any project; pre-fix
            // the GTK UI only hydrated the first project's tabs and
            // silently dropped the rest. Mac UI hydrates all.
            for tab in &project.tabs {
                self.attach_existing_tab(tab.clone());
            }
        }
        // Open a tab in the first project so the user lands inside
        // a shell — same shape as the Mac UI's bootstrap. Only fires
        // when the first project has no tabs, so a workspace that
        // was empty on disk gets a usable terminal on first boot.
        if let Some(first) = projects.first() {
            self.set_active_project(first.id);
            if first.tabs.is_empty() {
                self.open_new_tab_in(first.id).await?;
            }
        }

        // Subscribe to WatchEvents and drain on the GTK main loop.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut client_for_watch = client.clone();
            rt.spawn(async move {
                if let Err(err) = events::subscribe(&mut client_for_watch, tx).await {
                    tracing::warn!(?err, "WatchEvents stream ended with error");
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
                tv.close_page_finish(page, true);
                if let Some(tab_id) = tab_id {
                    app.close_tab_async(project_id, tab_id);
                }
                glib::Propagation::Stop
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
                tabs: RefCell::new(HashMap::new()),
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
        let trimmed = new_name.trim().to_string();
        if trimmed.is_empty() {
            return; // empty rename = no-op
        }
        let Some(mut client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Err(err) = client
                .inner()
                .rename_project(tonic::Request::new(roost_proto::v1::RenameProjectRequest {
                    project_id,
                    name: trimmed,
                }))
                .await
            {
                tracing::warn!(?err, project_id, "RenameProject RPC failed");
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
                        let Some(mut client) = app.client.borrow().clone() else {
                            return;
                        };
                        let rt = app.rt.clone();
                        rt.spawn(async move {
                            if let Err(err) = client
                                .inner()
                                .delete_project(tonic::Request::new(
                                    roost_proto::v1::DeleteProjectRequest { project_id },
                                ))
                                .await
                            {
                                tracing::warn!(?err, project_id, "DeleteProject RPC failed");
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

        let entry = gtk4::Entry::builder()
            .text(&current_title)
            .activates_default(true)
            .build();
        let popover = gtk4::Popover::builder().has_arrow(true).build();
        popover.set_child(Some(&entry));
        // Anchor the popover at the tab page — adw::TabView doesn't
        // expose the individual pill widget, so anchor at the
        // TabView itself; the popover positions itself above the
        // selected tab in practice.
        popover.set_parent(&ui.tab_view);

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
        let Some(mut client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Err(err) = client
                .inner()
                .set_tab_title(tonic::Request::new(roost_proto::v1::SetTabTitleRequest {
                    tab_id,
                    title: trimmed,
                }))
                .await
            {
                tracing::warn!(?err, tab_id, "SetTabTitle RPC failed");
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
        let Some(mut client) = self.client.borrow().clone() else {
            return Ok(());
        };
        let rt = self.rt.clone();
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let tab = rt
            .spawn(async move { client.open_tab(project_id, &cwd, 80, 24).await })
            .await
            .context("open_tab join")??;
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
        if ui.tabs.borrow().contains_key(&tab.id) {
            return;
        }
        let terminal = Rc::new(TerminalView::with_theme_and_font(
            self.theme.clone(),
            self.font_family.as_deref(),
            self.font_size_pt,
        ));
        let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel::<TabOutput>();
        let Some(mut client_for_session) = self.client.borrow().clone() else {
            return;
        };
        let tab_id = tab.id;
        let rt = self.rt.clone();
        // Spawn the StreamPty session on the tokio runtime; bridging
        // back to GTK happens via the unbounded mpsc.
        let session_handle = rt.spawn(async move {
            TabSession::spawn(&mut client_for_session, tab_id, 80, 24, output_tx).await
        });

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
        glib::spawn_future_local(async move {
            let session = match session_handle.await {
                Ok(Ok(s)) => Rc::new(s),
                Ok(Err(err)) => {
                    tracing::warn!(?err, tab_id, "StreamPty spawn failed");
                    return;
                }
                Err(join_err) => {
                    tracing::warn!(?join_err, tab_id, "StreamPty join failed");
                    return;
                }
            };
            terminal_for_drain.set_on_input({
                let session = session.clone();
                move |bytes| session.send_input(bytes)
            });
            let projects = app_for_attach.projects.borrow();
            if let Some(ui) = projects.get(&project_id) {
                ui.tabs.borrow_mut().insert(
                    tab_id,
                    TabUi {
                        view: terminal_for_drain.clone(),
                        session: session.clone(),
                        page: page_for_future.clone(),
                        cwd: RefCell::new(tab_cwd),
                        // Fresh tabs start in `None` until the daemon
                        // emits a `TabStateChangedEvent`. `hook_active`
                        // defaults to false; the daemon owns that flag
                        // and broadcasts changes via `HookActiveChangedEvent`.
                        state: RefCell::new(TabState::None),
                        hook_active: RefCell::new(false),
                    },
                );
            }
            drop(projects);
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
                            tracing::info!(status, %reason, "PTY exited");
                            break;
                        }
                        TabOutput::Error(reason) => {
                            tracing::warn!(reason, "PTY stream error");
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

    /// Dispatch a daemon Event. Cross-client convergence: a CLI
    /// `tab open / close / reorder` reflects here within ~1s.
    fn handle_event(self: &Rc<Self>, event: roost_proto::v1::Event) {
        let Some(kind) = event.kind else {
            return;
        };
        match kind {
            EventKind::ProjectCreated(p) => {
                if let Some(project) = p.project {
                    self.add_project_ui(&project);
                }
            }
            EventKind::ProjectRenamed(r) => {
                let mut projects = self.projects.borrow_mut();
                if let Some(ui) = projects.get_mut(&r.project_id) {
                    ui.name = r.name.clone();
                    // M9: update the Stack's label child directly
                    // rather than hunting it through `sidebar_row.child()`.
                    // If the user is currently mid-rename (Stack
                    // showing the entry), the label text still
                    // updates but the visible child stays on entry —
                    // honours "do not clobber in-progress edits"
                    // (per `goal-linux-gtk-parity-2026-05-17.md` M9
                    // race-guard).
                    ui.sidebar_label.set_text(&r.name);
                    if *self.active_project_id.borrow() == r.project_id {
                        self.window_title.set_title(&r.name);
                    }
                }
            }
            EventKind::ProjectDeleted(d) => {
                let mut projects = self.projects.borrow_mut();
                if let Some(ui) = projects.remove(&d.project_id) {
                    self.sidebar.remove(&ui.sidebar_row);
                    self.tab_stack.remove(
                        &self
                            .tab_stack
                            .child_by_name(&stack_name(d.project_id))
                            .expect("tab stack child for project"),
                    );
                }
            }
            EventKind::TabOpened(opened) => {
                if let Some(tab) = opened.tab {
                    self.attach_existing_tab(tab);
                }
            }
            EventKind::TabDeleted(d) => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    let mut tabs = ui.tabs.borrow_mut();
                    if let Some(tab_ui) = tabs.remove(&d.tab_id) {
                        ui.tab_view.close_page(&tab_ui.page);
                    }
                }
            }
            EventKind::TabTitle(t) => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&t.tab_id) {
                        tab_ui.page.set_title(&t.title);
                    }
                }
            }
            EventKind::TabsReordered(r) => {
                let projects = self.projects.borrow();
                if let Some(ui) = projects.get(&r.project_id) {
                    // Rebuild the page order. Build a tab_id → page
                    // map first, then call set_page() / append in the
                    // target order. adw::TabView doesn't expose a
                    // direct reorder API, so we use `reorder_page` to
                    // walk each target slot.
                    let tabs = ui.tabs.borrow();
                    for (target_index, tab_id) in r.tab_ids.iter().enumerate() {
                        if let Some(tab_ui) = tabs.get(tab_id) {
                            ui.tab_view.reorder_page(&tab_ui.page, target_index as i32);
                        }
                    }
                }
            }
            EventKind::ProjectsReordered(_r) => {
                // Sidebar row order rebuild. Cheap to defer to
                // commit 11's polish pass when the row widgets get a
                // dedicated `ProjectRow` type. Today the order
                // matters less because there's no drag-source on the
                // sidebar yet.
            }
            EventKind::TabCwd(c) => {
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&c.tab_id) {
                        // Mirror the Mac UI's tilde abbreviation so
                        // the pill label doesn't bloat with full
                        // home-prefixed paths.
                        let label = tilde_abbreviate(&c.cwd);
                        // OSC 7 doesn't override an OSC 0/1/2 title;
                        // only update the label when the page still
                        // shows a daemon-default tab title.
                        let current = tab_ui.page.title().to_string();
                        if current.starts_with("Tab ") {
                            tab_ui.page.set_title(&label);
                        }
                        // Persist for headerbar subtitle. M2 will
                        // surface this through `refresh_window_subtitle`.
                        *tab_ui.cwd.borrow_mut() = c.cwd.clone();
                    }
                }
                drop(projects);
                self.refresh_window_subtitle();
            }
            EventKind::TabNotification(n) => {
                // Light up the page's "needs attention" indicator so
                // inactive tabs surface the pending notification.
                let projects = self.projects.borrow();
                for ui in projects.values() {
                    if let Some(tab_ui) = ui.tabs.borrow().get(&n.tab_id) {
                        tab_ui.page.set_needs_attention(n.has_pending);
                    }
                }
            }
            EventKind::Notification(n) => {
                self.fire_desktop_notification(n.tab_id, &n.title, &n.body);
            }
            EventKind::TabState(s) => {
                // M6: per-tab agent state. Update both the per-tab
                // indicator icon (M7) and the project rollup CSS.
                let state = TabState::from_proto(s.state);
                let mut affected_project: Option<i64> = None;
                {
                    let projects = self.projects.borrow();
                    for (project_id, ui) in projects.iter() {
                        let tabs = ui.tabs.borrow();
                        if let Some(tab_ui) = tabs.get(&s.tab_id) {
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
                tracing::debug!(tab_id = s.tab_id, ?state, "TabState applied");
            }
            EventKind::HookActive(h) => {
                // M6: suppress this tab from the rollup while the
                // Claude hook owns the surface. Indicator icon stays
                // mapped to the underlying state — the hook usually
                // pulses its own visual; we just don't double-promote
                // via the sidebar stripe.
                let mut affected_project: Option<i64> = None;
                {
                    let projects = self.projects.borrow();
                    for (project_id, ui) in projects.iter() {
                        let tabs = ui.tabs.borrow();
                        if let Some(tab_ui) = tabs.get(&h.tab_id) {
                            *tab_ui.hook_active.borrow_mut() = h.active;
                            affected_project = Some(*project_id);
                            break;
                        }
                    }
                }
                if let Some(project_id) = affected_project {
                    self.refresh_rollup_for(project_id);
                }
                tracing::debug!(
                    tab_id = h.tab_id,
                    hook_active = h.active,
                    "HookActive applied"
                );
            }
            EventKind::Active(a) => {
                // M6: daemon-driven active selection (e.g. a CLI
                // `tab focus` from a sibling terminal). Sync the GTK
                // UI's notion so cross-client convergence works.
                // Zero values mean "no selection" (e.g. last tab
                // closed); skip in that case rather than dropping
                // the user's current focus.
                if a.project_id != 0 {
                    self.set_active_project(a.project_id);
                }
                if a.tab_id != 0 {
                    let projects = self.projects.borrow();
                    if let Some(ui) = projects.get(&a.project_id) {
                        if let Some(tab_ui) = ui.tabs.borrow().get(&a.tab_id) {
                            ui.tab_view.set_selected_page(&tab_ui.page);
                        }
                    }
                }
            }
        }
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
        match action {
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
        let target = ((current + delta).rem_euclid(n)) as u32;
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
        let Some(mut client) = self.client.borrow().clone() else {
            return Ok(());
        };
        let rt = self.rt.clone();
        let cwd_owned = cwd.to_string();
        let project = rt
            .spawn(async move { client.create_project("", &cwd_owned).await })
            .await
            .context("create_project join")??;
        // WatchEvents will also deliver ProjectCreated; the
        // `add_project_ui` idempotent check prevents duplicate rows.
        self.add_project_ui(&project);
        self.set_active_project(project.id);
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
        let Some(mut client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Err(err) = client.report_osc(tab_id, command, &payload).await {
                tracing::warn!(?err, tab_id, command, "ReportOsc failed");
            }
        });
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

    /// Fire CloseTab on the daemon for the given tab. Async so we
    /// don't block the GTK main loop.
    ///
    /// `NotFound` is treated as an expected race: when the daemon
    /// cascade-deletes a tab (M5 shell-exit cascade, project delete,
    /// CLI `tab close`), the resulting `TabDeletedEvent` lands in
    /// `handle_event::TabDeleted` which calls `tab_view.close_page`
    /// → which fires the `close-page` signal → which invokes this
    /// method. By that time the daemon-side tab is already gone, so
    /// the RPC's `NotFound` is the expected steady state, not a bug
    /// to log. Other status codes (Internal, FailedPrecondition,
    /// etc.) still surface as warnings.
    fn close_tab_async(self: &Rc<Self>, _project_id: i64, tab_id: i64) {
        let Some(mut client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            match client
                .inner()
                .close_tab(tonic::Request::new(roost_proto::v1::CloseTabRequest {
                    tab_id,
                }))
                .await
            {
                Ok(_) => {}
                Err(status) if status.code() == tonic::Code::NotFound => {
                    tracing::debug!(tab_id, "CloseTab: tab already deleted server-side");
                }
                Err(err) => {
                    tracing::warn!(?err, tab_id, "CloseTab RPC failed");
                }
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
        let fallback = gtk4::ShortcutTrigger::parse_string("<Control><Shift>F24")
            .expect("fallback shortcut trigger parse");
        fallback
    })
}

#[cfg(test)]
mod tests {
    use super::tilde_abbreviate_with_home;

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
}
