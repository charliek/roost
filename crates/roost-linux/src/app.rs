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
use libadwaita::{ApplicationWindow, HeaderBar, TabView};
use tokio::runtime::Handle;

use roost_common::default_socket_path;
use roost_proto::v1::event::Kind as EventKind;
use roost_proto::v1::{Project, Tab};

use crate::client::RoostClient;
use crate::config::RoostConfig;
use crate::events;
use crate::keybind::{canonicalize_bindings, default_bindings, Accel, AccelMods, KeybindAction};
use crate::tab_session::{TabOutput, TabSession};
use crate::terminal_view::TerminalView;
use crate::theme::Theme;

/// One per project: sidebar row + tab strip + tab content stack.
struct ProjectUi {
    name: String,
    sidebar_row: gtk4::ListBoxRow,
    tab_view: TabView,
    /// Tab id → (TerminalView, TabSession).
    tabs: RefCell<HashMap<i64, TabUi>>,
}

#[allow(dead_code)] // view + session held to keep the TerminalView + StreamPty alive for the tab's lifetime.
struct TabUi {
    view: Rc<TerminalView>,
    session: Rc<TabSession>,
    page: libadwaita::TabPage,
}

pub struct App {
    window: ApplicationWindow,
    /// `None` before `bootstrap()` connects; closures that need the
    /// client must read this and bail (no-op) if `None`.
    client: RefCell<Option<RoostClient>>,
    rt: Handle,
    sidebar: gtk4::ListBox,
    /// `gtk::Stack` of TabView widgets, one entry per project id.
    /// Switching the sidebar selection flips the visible child.
    tab_stack: gtk4::Stack,
    title_label: gtk4::Label,
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

        let header = HeaderBar::new();
        let title_label = gtk4::Label::new(Some("Roost — connecting…"));
        title_label.add_css_class("title");
        header.set_title_widget(Some(&title_label));

        // Sidebar — single-level ListBox of projects.
        let sidebar = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Browse)
            .css_classes(["navigation-sidebar"])
            .build();
        let sidebar_scroll = gtk4::ScrolledWindow::builder()
            .child(&sidebar)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .width_request(220)
            .build();

        // Right pane: a Stack of per-project AdwTabView widgets.
        let tab_stack = gtk4::Stack::builder().hexpand(true).vexpand(true).build();

        let paned = gtk4::Paned::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .resize_start_child(false)
            .shrink_start_child(false)
            .position(220)
            .start_child(&sidebar_scroll)
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
            tab_stack: tab_stack.clone(),
            title_label: title_label.clone(),
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
        }

        app_struct.window.present();

        // Boot the daemon round-trip + WatchEvents subscription on
        // the GTK main loop's async executor.
        let app_for_boot = app_struct.clone();
        glib::spawn_future_local(async move {
            if let Err(err) = app_for_boot.bootstrap().await {
                app_for_boot.title_label.set_text(&format!("Roost — {err}"));
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

        self.title_label
            .set_text(&format!("Roost — daemon v{}", id.daemon_version));

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
        }
        // Open a tab in the first project so the user lands inside
        // a shell — same shape as the Mac UI's bootstrap.
        if let Some(first) = projects.first() {
            self.set_active_project(first.id);
            if first.tabs.is_empty() {
                self.open_new_tab_in(first.id).await?;
            } else {
                for tab in &first.tabs {
                    self.attach_existing_tab(tab.clone());
                }
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
        let label = gtk4::Label::builder()
            .label(&project.name)
            .halign(gtk4::Align::Start)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&label));
        row.set_widget_name(&format!("project-{}", project.id));
        self.sidebar.append(&row);

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

        let tab_bar = libadwaita::TabBar::builder().view(&tab_view).build();
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
                tab_view,
                tabs: RefCell::new(HashMap::new()),
            },
        );
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
        // Update the headerbar title to the project name.
        self.title_label.set_text(&format!("Roost — {}", ui.name));
        // Sync sidebar selection without re-firing the handler.
        self.sidebar.select_row(Some(&ui.sidebar_row));
        drop(projects);
        *self.active_project_id.borrow_mut() = project_id;
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
                    },
                );
            }
            drop(projects);

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
                    if let Some(child) = ui.sidebar_row.child() {
                        if let Some(label) = child.downcast_ref::<gtk4::Label>() {
                            label.set_text(&r.name);
                        }
                    }
                    if *self.active_project_id.borrow() == r.project_id {
                        self.title_label.set_text(&format!("Roost — {}", r.name));
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
                    }
                }
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
            _ => {
                // TabState, HookActive, Active — currently informational
                // only on the Linux UI. Status-icon polish on the tab
                // page comes in commit 11.
            }
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
        // The sidebar lives inside the Paned's start child. Toggle
        // its visibility — Paned auto-adjusts the divider position.
        let visible = self.sidebar.is_visible();
        self.sidebar.set_visible(!visible);
        if let Some(scroll) = self.sidebar.parent() {
            scroll.set_visible(!visible);
        }
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
        let Some(mut client) = self.client.borrow().clone() else {
            return Ok(());
        };
        let rt = self.rt.clone();
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let project = rt
            .spawn(async move { client.create_project("", &cwd).await })
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

/// Replace `$HOME` prefix in a path with `~`. Used by the OSC 7
/// (cwd) tab pill label so home-rooted dirs don't dominate the
/// chrome.
fn tilde_abbreviate(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) else {
        return path.to_string();
    };
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
