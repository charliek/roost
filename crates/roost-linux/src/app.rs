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
use crate::events;
use crate::tab_session::{TabOutput, TabSession};
use crate::terminal_view::TerminalView;

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

        let app_struct = Rc::new(App {
            window,
            client: RefCell::new(None),
            rt: rt.clone(),
            sidebar: sidebar.clone(),
            tab_stack: tab_stack.clone(),
            title_label: title_label.clone(),
            projects: RefCell::new(HashMap::new()),
            active_project_id: RefCell::new(0),
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
        let terminal = Rc::new(TerminalView::new());
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

            // Output drain: PTY bytes → renderer.
            glib::spawn_future_local(async move {
                while let Some(msg) = output_rx.recv().await {
                    match msg {
                        TabOutput::Bytes(data) => terminal_for_drain.vt_write(&data),
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
            _ => {
                // TabCwd, TabState, TabNotification, Notification,
                // HookActive, Active — wired up in commit 10
                // (OSC + notifications).
            }
        }
    }

    /// Fire CloseTab on the daemon for the given tab. Async so we
    /// don't block the GTK main loop.
    fn close_tab_async(self: &Rc<Self>, _project_id: i64, tab_id: i64) {
        let Some(mut client) = self.client.borrow().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Err(err) = client
                .inner()
                .close_tab(tonic::Request::new(roost_proto::v1::CloseTabRequest {
                    tab_id,
                }))
                .await
            {
                tracing::warn!(?err, tab_id, "CloseTab RPC failed");
            }
        });
    }
}

fn stack_name(project_id: i64) -> String {
    format!("project-{project_id}")
}

fn parse_tab_id_from_page(page: &libadwaita::TabPage) -> Option<i64> {
    let name = page.child().widget_name().to_string();
    name.strip_prefix("tab-").and_then(|n| n.parse().ok())
}
