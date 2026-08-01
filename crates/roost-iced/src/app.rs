use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use iced::keyboard;
use iced::widget::{button, canvas, column, container, row, text};
use iced::{window, Color, Element, Fill, Size, Task};
use roost_engine::ipc::{
    ClipboardOp, DumpData, IpcHandler, ResolvedCellData, ResolvedCellsData, UiRequest,
};
use roost_engine::osc::{ClipboardTarget, OscAction, OscColorSnapshot, OscRouter};
use roost_engine::session::{InputCapture, TabOutput, TabSession};
use roost_engine::single_instance::InstanceLock;
use roost_engine::{LocalClient, PtySupervisor, RestoreTab, Workspace};
use roost_ipc::messages::Project;
use roost_ipc::paths::BundleProfile;
use roost_ipc::IpcServer;
use roost_ui_model::theme::Theme;
use roost_vt::{KeyEncoder, RenderState, Terminal, TerminalOptions};

use crate::input;
use crate::terminal_canvas::{
    resolve_colors, DrawCell, TerminalCanvas, TerminalSnapshot, CELL_HEIGHT, CELL_WIDTH,
    TERMINAL_PADDING,
};
use crate::Message;

const SIDEBAR_WIDTH: f32 = 220.0;
const TAB_BAR_HEIGHT: f32 = 44.0;
const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 32;
const UNSUPPORTED: &str = "not implemented by the Iced walking skeleton";

pub enum UiTask {
    None,
    Focus(window::Id),
    Resize(window::Id, Size),
}

struct TerminalTab {
    terminal: Terminal,
    render_state: RenderState,
    encoder: KeyEncoder,
    session: TabSession,
    output_rx: tokio::sync::mpsc::UnboundedReceiver<TabOutput>,
    reply_buffer: Arc<Mutex<Vec<u8>>>,
    input_capture: Option<InputCapture>,
    osc_router: OscRouter,
    pointer_shape: String,
    theme: Theme,
    snapshot: TerminalSnapshot,
    cols: u16,
    rows: u16,
}

impl TerminalTab {
    fn attach(supervisor: Arc<PtySupervisor>, tab_id: i64, test_mode: bool) -> Result<Self> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: 2_000,
        })?;
        let theme = Theme::default();
        terminal.set_color_foreground(theme.foreground)?;
        terminal.set_color_background(theme.background)?;
        terminal.set_color_cursor(theme.cursor)?;
        terminal.set_color_palette(&theme.palette)?;
        let reply_buffer = Arc::new(Mutex::new(Vec::new()));
        terminal
            .set_write_pty_buffer(Arc::clone(&reply_buffer))
            .context("install libghostty PTY reply buffer")?;
        let render_state = RenderState::new()?;
        let encoder = KeyEncoder::new()?;
        let input_capture = test_mode.then(|| Arc::new(Mutex::new(Vec::new())));
        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
        let session = TabSession::attach(supervisor, tab_id, output_tx, input_capture.clone())?;
        Ok(Self {
            terminal,
            render_state,
            encoder,
            session,
            output_rx,
            reply_buffer,
            input_capture,
            osc_router: OscRouter::new(),
            pointer_shape: "default".into(),
            theme,
            snapshot: TerminalSnapshot::blank(DEFAULT_COLS, DEFAULT_ROWS),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        })
    }

    fn write_vt(&mut self, bytes: &[u8]) -> Vec<OscAction> {
        let colors = self.osc_colors();
        let actions = self.osc_router.feed(bytes, &colors);
        self.terminal.vt_write(bytes);
        self.drain_terminal_replies();
        actions
    }

    fn osc_colors(&self) -> OscColorSnapshot {
        let fallback_foreground = self.theme.foreground;
        let fallback_background = self.theme.background;
        let (foreground, background, cursor) = self
            .terminal
            .live_colors()
            .map(|colors| {
                (
                    colors.foreground,
                    colors.background,
                    colors.cursor.unwrap_or(self.theme.cursor),
                )
            })
            .unwrap_or((fallback_foreground, fallback_background, self.theme.cursor));
        let palette = self.terminal.live_palette().unwrap_or(self.theme.palette);
        let rgb = |color: roost_vt::ColorRgb| (color.r, color.g, color.b);
        OscColorSnapshot::new(
            rgb(foreground),
            rgb(background),
            rgb(cursor),
            palette.map(rgb),
        )
    }

    fn drain_terminal_replies(&self) {
        let bytes = self
            .reply_buffer
            .lock()
            .map(|mut buffer| std::mem::take(&mut *buffer))
            .unwrap_or_default();
        self.session.send_input(bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        if self.cols == cols && self.rows == rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        if let Err(error) = self.terminal.resize(
            cols,
            rows,
            CELL_WIDTH.round() as u32,
            CELL_HEIGHT.round() as u32,
        ) {
            tracing::warn!(?error, cols, rows, "libghostty terminal resize failed");
        }
        self.drain_terminal_replies();
        self.session.send_resize(cols, rows);
    }

    fn refresh_snapshot(&mut self) -> Result<()> {
        self.render_state.update(&self.terminal)?;
        let colors = self.render_state.colors()?;
        let cursor = self.render_state.cursor();
        let mut cells = Vec::new();
        let mut rows = vec![vec![String::new(); usize::from(self.cols)]; usize::from(self.rows)];
        self.render_state.walk(&self.terminal, |row, cell| {
            if row >= u32::from(self.rows) || cell.col >= self.cols {
                return;
            }
            let text = if cell.text.is_empty() {
                " ".to_string()
            } else {
                cell.text
            };
            rows[row as usize][usize::from(cell.col)] = text.clone();
            let (foreground, background) = resolve_colors(
                cell.fg,
                cell.bg,
                (colors.foreground, colors.background),
                cell.style.inverse,
            );
            if text != " " || cell.bg.is_some() || cell.style.inverse {
                cells.push(DrawCell {
                    row,
                    col: cell.col,
                    text,
                    foreground,
                    background,
                    explicit_background: cell.bg.is_some() || cell.style.inverse,
                    bold: cell.style.bold,
                    italic: cell.style.italic,
                    inverse: cell.style.inverse,
                });
            }
        })?;
        let rows_text = rows
            .into_iter()
            .map(|row| row.concat().trim_end().to_string())
            .collect();
        self.snapshot = TerminalSnapshot {
            cols: self.cols,
            rows: self.rows,
            foreground: colors.foreground,
            background: colors.background,
            cursor,
            cells,
            rows_text,
        };
        Ok(())
    }

    fn dump(&self) -> DumpData {
        DumpData {
            cols: u32::from(self.snapshot.cols),
            rows: u32::from(self.snapshot.rows),
            cursor: self
                .snapshot
                .cursor
                .filter(|cursor| cursor.visible)
                .map(|cursor| (cursor.row, cursor.col, cursor.visible)),
            rows_text: self.snapshot.rows_text.clone(),
        }
    }

    fn resolved_cells(&self) -> ResolvedCellsData {
        let mut by_position: HashMap<(u32, u16), &DrawCell> = self
            .snapshot
            .cells
            .iter()
            .map(|cell| ((cell.row, cell.col), cell))
            .collect();
        let mut cells = Vec::with_capacity(usize::from(self.cols) * usize::from(self.rows));
        for row in 0..u32::from(self.rows) {
            for col in 0..self.cols {
                let cell = by_position.remove(&(row, col));
                let foreground = cell.map_or(self.snapshot.foreground, |cell| cell.foreground);
                let background = cell.map_or(self.snapshot.background, |cell| cell.background);
                cells.push(ResolvedCellData {
                    row,
                    col,
                    text: cell.map_or_else(|| " ".into(), |cell| cell.text.clone()),
                    fg: (foreground.r, foreground.g, foreground.b),
                    bg: (background.r, background.g, background.b),
                    has_explicit_bg: cell.is_some_and(|cell| cell.explicit_background),
                    bold: cell.is_some_and(|cell| cell.bold),
                    italic: cell.is_some_and(|cell| cell.italic),
                    inverse: cell.is_some_and(|cell| cell.inverse),
                });
            }
        }
        ResolvedCellsData {
            cols: self.cols,
            rows: self.rows,
            cells,
        }
    }
}

pub struct App {
    workspace: Arc<Workspace>,
    supervisor: Arc<PtySupervisor>,
    client: LocalClient,
    tabs: HashMap<i64, TerminalTab>,
    projects: Vec<Project>,
    ui_rx: tokio::sync::mpsc::UnboundedReceiver<UiRequest>,
    window_id: Option<window::Id>,
    window_size: Size,
    test_mode: bool,
    status: Option<String>,
    system_clipboard: Option<String>,
    selection_clipboard: Option<String>,
    // Field order is intentional: terminal sessions and IPC receivers are
    // dropped before the runtime; the lock is held until every runtime task
    // has been cancelled and joined by Runtime::drop.
    runtime: tokio::runtime::Runtime,
    _lock: InstanceLock,
}

impl App {
    pub fn bootstrap(profile: &BundleProfile, lock: InstanceLock) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build Iced engine runtime")?;
        let workspace = Arc::new(Workspace::open(profile.state_json_path()));
        let supervisor = Arc::new(PtySupervisor::new());
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            profile.socket_path.clone(),
        );

        hydrate_workspace(&runtime, &client)?;

        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler = IpcHandler::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            profile.socket_path.clone(),
            profile.app_label,
            profile.app_id,
        )
        .with_ui(ui_tx);
        let server = runtime
            .block_on(IpcServer::bind(&profile.socket_path, handler))
            .context("bind Iced IPC server")?;
        runtime.spawn(async move {
            if let Err(error) = server.run().await {
                tracing::warn!(?error, "Iced IPC server stopped");
            }
        });

        let mut app = Self {
            workspace,
            supervisor,
            client,
            tabs: HashMap::new(),
            projects: Vec::new(),
            ui_rx,
            window_id: None,
            window_size: Size::new(1100.0, 720.0),
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            status: None,
            system_clipboard: None,
            selection_clipboard: None,
            runtime,
            _lock: lock,
        };
        app.reconcile();
        app.resize(app.window_size);
        tracing::info!(socket = %profile.socket_path.display(), "Iced walking skeleton ready");
        Ok(app)
    }

    pub fn set_window_id(&mut self, id: window::Id) {
        self.window_id = Some(id);
    }

    pub fn tick(&mut self) -> UiTask {
        let task = self.service_ui_requests();
        self.reconcile();
        let mut exited = Vec::new();
        let mut osc_actions = Vec::new();
        for (tab_id, tab) in &mut self.tabs {
            while let Ok(output) = tab.output_rx.try_recv() {
                match output {
                    TabOutput::Bytes(bytes) => {
                        osc_actions.push((*tab_id, tab.write_vt(&bytes)));
                    }
                    TabOutput::Exit { status, reason } => {
                        tracing::info!(tab_id, status, %reason, "PTY exited");
                        exited.push(*tab_id);
                    }
                    TabOutput::Error(error) => {
                        // Broadcast lag cannot be reconstructed. Surface it and
                        // keep the workspace alive so IPC/UI state still resyncs.
                        self.status = Some(format!("tab {tab_id}: {error}"));
                        tracing::error!(tab_id, %error, "PTY output stream lost bytes");
                    }
                }
            }
            if let Err(error) = tab.refresh_snapshot() {
                tracing::warn!(tab_id, ?error, "terminal snapshot refresh failed");
            }
        }
        for (tab_id, actions) in osc_actions {
            self.apply_osc_actions(tab_id, actions);
        }
        for tab_id in exited {
            let _ = self.workspace.close_tab(tab_id);
            self.tabs.remove(&tab_id);
        }
        task
    }

    pub fn resize(&mut self, size: Size) {
        self.window_size = size;
        let width = (size.width - SIDEBAR_WIDTH - 2.0 * TERMINAL_PADDING).max(CELL_WIDTH * 2.0);
        let height = (size.height - TAB_BAR_HEIGHT - 2.0 * TERMINAL_PADDING).max(CELL_HEIGHT * 2.0);
        let cols = ((width / CELL_WIDTH).floor() as u16).max(2);
        let rows = ((height / CELL_HEIGHT).floor() as u16).max(2);
        for tab in self.tabs.values_mut() {
            tab.resize(cols, rows);
        }
    }

    pub fn keyboard(&mut self, event: keyboard::Event) {
        let (_, active_tab) = self.workspace.active();
        let Some(tab) = self.tabs.get_mut(&active_tab) else {
            return;
        };
        let bytes = input::encode_press(&mut tab.encoder, &tab.terminal, event);
        tab.session.send_input(bytes);
    }

    pub fn view(&self) -> Element<'_, Message> {
        let (active_project, active_tab) = self.workspace.active();
        let mut sidebar = column![text("ROOST").size(13)].spacing(8).padding(12);
        for project in &self.projects {
            let label = if project.id == active_project {
                format!("●  {}", project.name)
            } else {
                format!("   {}", project.name)
            };
            sidebar = sidebar.push(
                button(text(label).size(14))
                    .width(Fill)
                    .on_press(Message::ProjectSelected(project.id)),
            );
        }
        if let Some(status) = &self.status {
            sidebar = sidebar.push(text(status).size(11).color(Color::from_rgb8(238, 120, 120)));
        }
        let sidebar = container(sidebar)
            .width(SIDEBAR_WIDTH)
            .height(Fill)
            .style(container::dark);

        let active_project_tabs = self
            .projects
            .iter()
            .find(|project| project.id == active_project)
            .map(|project| project.tabs.as_slice())
            .unwrap_or(&[]);
        let mut tabs = row![].spacing(6).padding([7, 10]);
        for tab in active_project_tabs {
            let title = if tab.title.is_empty() {
                "shell"
            } else {
                &tab.title
            };
            let label = if tab.id == active_tab {
                format!("● {title}")
            } else {
                title.to_string()
            };
            tabs = tabs.push(button(text(label).size(13)).on_press(Message::TabSelected(tab.id)));
        }
        tabs = tabs.push(button(text("+")).on_press(Message::NewTab));
        let tab_bar = container(tabs)
            .height(TAB_BAR_HEIGHT)
            .width(Fill)
            .style(container::dark);

        let terminal: Element<'_, Message> = match self.tabs.get(&active_tab) {
            Some(tab) => canvas(TerminalCanvas {
                snapshot: tab.snapshot.clone(),
            })
            .width(Fill)
            .height(Fill)
            .into(),
            None => container(text("Starting terminal…"))
                .center(Fill)
                .width(Fill)
                .height(Fill)
                .into(),
        };
        row![sidebar, column![tab_bar, terminal].width(Fill).height(Fill)]
            .width(Fill)
            .height(Fill)
            .into()
    }

    pub fn select_project(&mut self, project_id: i64) {
        if let Some(tab_id) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.first())
            .map(|tab| tab.id)
        {
            let _ = self.workspace.focus_tab(tab_id);
        }
    }

    pub fn select_tab(&mut self, tab_id: i64) {
        let _ = self.workspace.focus_tab(tab_id);
    }

    pub fn new_tab(&mut self) {
        let (project_id, _) = self.workspace.active();
        if project_id == 0 {
            return;
        }
        if let Err(error) = self.runtime.block_on(self.client.open_tab(
            project_id,
            "",
            "",
            &[],
            u32::from(DEFAULT_COLS),
            u32::from(DEFAULT_ROWS),
        )) {
            self.status = Some(error.to_string());
        }
        self.reconcile();
    }

    fn reconcile(&mut self) {
        // Full authoritative snapshot on every UI tick is the recovery path
        // for a slow consumer: deltas are an optimization, never UI truth.
        self.projects = self.workspace.snapshot();
        let live_ids: HashSet<i64> = self
            .projects
            .iter()
            .flat_map(|project| project.tabs.iter().map(|tab| tab.id))
            .collect();
        self.tabs.retain(|tab_id, _| live_ids.contains(tab_id));
        for tab_id in live_ids {
            if self.tabs.contains_key(&tab_id) {
                continue;
            }
            let attached = {
                let _guard = self.runtime.enter();
                TerminalTab::attach(Arc::clone(&self.supervisor), tab_id, self.test_mode)
            };
            match attached {
                Ok(mut tab) => {
                    let size = self.window_size;
                    let width =
                        (size.width - SIDEBAR_WIDTH - 2.0 * TERMINAL_PADDING).max(CELL_WIDTH * 2.0);
                    let height = (size.height - TAB_BAR_HEIGHT - 2.0 * TERMINAL_PADDING)
                        .max(CELL_HEIGHT * 2.0);
                    tab.resize(
                        ((width / CELL_WIDTH).floor() as u16).max(2),
                        ((height / CELL_HEIGHT).floor() as u16).max(2),
                    );
                    self.tabs.insert(tab_id, tab);
                }
                Err(error) => tracing::debug!(tab_id, ?error, "PTY not ready for UI attach"),
            }
        }
    }

    fn service_ui_requests(&mut self) -> UiTask {
        let mut task = UiTask::None;
        while let Ok(request) = self.ui_rx.try_recv() {
            match request {
                UiRequest::Activate => {
                    if let Some(id) = self.window_id {
                        task = UiTask::Focus(id);
                    }
                }
                UiRequest::Dump { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get(&tab_id)
                        .map(TerminalTab::dump)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                    let _ = reply.send(result);
                }
                UiRequest::TabFeedPtyBytes {
                    tab_id,
                    data,
                    reply,
                } => {
                    let mut actions = None;
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else if let Some(tab) = self.tabs.get_mut(&tab_id) {
                        actions = Some(tab.write_vt(&data));
                        tab.refresh_snapshot().map_err(|error| error.to_string())
                    } else {
                        Err(format!("tab {tab_id} has no live terminal"))
                    };
                    if let Some(actions) = actions {
                        self.apply_osc_actions(tab_id, actions);
                    }
                    let _ = reply.send(result);
                }
                UiRequest::TabCapturePtyInput {
                    tab_id,
                    drain,
                    reply,
                } => {
                    let result = self
                        .tabs
                        .get(&tab_id)
                        .and_then(|tab| tab.input_capture.as_ref())
                        .ok_or_else(|| {
                            "ROOST_TEST_MODE=1 is required or tab is missing".to_string()
                        })
                        .and_then(|capture| {
                            capture
                                .lock()
                                .map(|mut bytes| {
                                    if drain {
                                        std::mem::take(&mut *bytes)
                                    } else {
                                        bytes.clone()
                                    }
                                })
                                .map_err(|_| "PTY input capture lock poisoned".to_string())
                        });
                    let _ = reply.send(result);
                }
                UiRequest::TabDumpResolved { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get(&tab_id)
                        .map(TerminalTab::resolved_cells)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                    let _ = reply.send(result);
                }
                UiRequest::WindowMetrics { reply } => {
                    let _ = reply.send(Ok((
                        f64::from(self.window_size.width),
                        f64::from(self.window_size.height),
                        f64::from(SIDEBAR_WIDTH),
                        false,
                    )));
                }
                UiRequest::WindowResize {
                    width,
                    height,
                    reply,
                } => {
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else if let Some(id) = self.window_id {
                        let size = Size::new(width as f32, height as f32);
                        // Some Wayland compositors retain authority over the
                        // toplevel size and may ignore a client request. Apply
                        // the requested logical geometry immediately for the
                        // deterministic test port; a compositor Resized event
                        // remains authoritative if it sends one afterward.
                        self.resize(size);
                        task = UiTask::Resize(id, size);
                        Ok(())
                    } else {
                        Err("Iced window is not open yet".into())
                    };
                    let _ = reply.send(result);
                }
                UiRequest::AppSetWindowFocus { focused, reply } => {
                    let result = if self.test_mode {
                        self.workspace.set_window_focused(focused);
                        Ok(())
                    } else {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    };
                    let _ = reply.send(result);
                }
                UiRequest::AppCursorShape { reply } => {
                    let shape = self
                        .tabs
                        .get(&self.workspace.active().1)
                        .map_or("default", |tab| tab.pointer_shape.as_str());
                    let _ = reply.send(Ok(shape.into()));
                }
                UiRequest::AppActiveTerminalFocused { reply } => {
                    let _ = reply.send(Ok(true));
                }
                UiRequest::AppSelectedTabId { reply } => {
                    let _ = reply.send(Ok(self.workspace.active().1));
                }
                UiRequest::Screenshot { reply, .. } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
                UiRequest::PaletteOpen { reply, .. }
                | UiRequest::PaletteState { reply }
                | UiRequest::PaletteQuery { reply, .. }
                | UiRequest::PaletteActivate { reply, .. }
                | UiRequest::PaletteDismiss { reply } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
                UiRequest::PalettePresent { reply, .. } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
                UiRequest::SelectionSet { reply, .. } | UiRequest::SelectionClear { reply, .. } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
                UiRequest::SelectionDump { reply, .. } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
                UiRequest::ClipboardDump { target, reply } => {
                    let value = match target {
                        ClipboardOp::System => self.system_clipboard.clone(),
                        ClipboardOp::Selection => self.selection_clipboard.clone(),
                    };
                    let _ = reply.send(Ok(value));
                }
                UiRequest::ClipboardWrite { target, text } => match target {
                    ClipboardOp::System => self.system_clipboard = Some(text),
                    ClipboardOp::Selection => self.selection_clipboard = Some(text),
                },
                UiRequest::TabExpandSelectionAt { reply, .. } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
                UiRequest::SidebarDump { reply } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
                UiRequest::TabDispatchMouseEvent { reply, .. } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
                }
            }
        }
        task
    }

    fn apply_osc_actions(&mut self, tab_id: i64, actions: Vec<OscAction>) {
        for action in actions {
            match action {
                OscAction::Workspace { command, payload } => {
                    self.client.apply_osc(tab_id, command, &payload);
                }
                OscAction::PtyInput(bytes) => {
                    if let Some(tab) = self.tabs.get(&tab_id) {
                        tab.session.send_input(bytes);
                    }
                }
                OscAction::ClipboardWrite { target, text } => match target {
                    ClipboardTarget::System => self.system_clipboard = Some(text),
                    ClipboardTarget::Selection => self.selection_clipboard = Some(text),
                },
                OscAction::PointerShape(name) => {
                    if let Some(tab) = self.tabs.get_mut(&tab_id) {
                        tab.pointer_shape = canonical_pointer_shape(&name).into();
                    }
                }
            }
        }
    }
}

fn canonical_pointer_shape(name: &str) -> &str {
    match name {
        "default" | "pointer" | "text" | "crosshair" | "grab" | "grabbing" | "not-allowed"
        | "col-resize" | "row-resize" | "n-resize" | "s-resize" | "e-resize" | "w-resize"
        | "ne-resize" | "nw-resize" | "se-resize" | "sw-resize" => name,
        _ => "default",
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Freeze and fsync the authoritative layout before PTY-exit tasks can
        // observe teardown and attempt a later persistence write.
        self.workspace.flush();
    }
}

fn hydrate_workspace(runtime: &tokio::runtime::Runtime, client: &LocalClient) -> Result<()> {
    let mut projects = runtime.block_on(client.list_projects())?;
    if projects.is_empty() {
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        projects.push(runtime.block_on(client.create_project("Roost", &cwd))?);
    }
    let restore = client.workspace.take_restore_layout();
    for project in &projects {
        let saved = restore
            .as_ref()
            .and_then(|layout| {
                layout
                    .projects
                    .iter()
                    .find(|item| item.project_id == project.id)
            })
            .map(|item| item.tabs.as_slice())
            .unwrap_or(&[]);
        let fallback;
        let specs = if saved.is_empty() {
            fallback = vec![RestoreTab {
                cwd: project.cwd.clone(),
                title: String::new(),
                user_titled: false,
            }];
            fallback.as_slice()
        } else {
            saved
        };
        for spec in specs {
            match runtime.block_on(client.open_tab(
                project.id,
                &spec.cwd,
                &spec.title,
                &[],
                u32::from(DEFAULT_COLS),
                u32::from(DEFAULT_ROWS),
            )) {
                Ok(tab) if spec.user_titled && !spec.title.is_empty() => {
                    client.workspace.set_tab_title(tab.id, &spec.title)?;
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(project_id = project.id, ?error, "restore tab failed"),
            }
        }
    }

    let snapshot = client.workspace.snapshot();
    let active_project = restore
        .as_ref()
        .map(|layout| layout.active_project_id)
        .filter(|id| snapshot.iter().any(|project| project.id == *id))
        .or_else(|| snapshot.first().map(|project| project.id));
    if let Some(project_id) = active_project {
        let position = restore
            .as_ref()
            .map_or(0, |layout| layout.active_tab_position.max(0) as usize);
        if let Some(tab_id) = snapshot
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.get(position).or_else(|| project.tabs.first()))
            .map(|tab| tab.id)
        {
            client.workspace.focus_tab(tab_id)?;
        }
    }
    Ok(())
}

impl Message {
    pub(crate) fn apply(self, app: &mut App) -> Task<Message> {
        match self {
            Self::ProjectSelected(project_id) => app.select_project(project_id),
            Self::TabSelected(tab_id) => app.select_tab(tab_id),
            Self::NewTab => app.new_tab(),
            _ => {}
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_geometry_never_produces_zero_grid() {
        let size = Size::new(1.0, 1.0);
        let width = (size.width - SIDEBAR_WIDTH - 2.0 * TERMINAL_PADDING).max(CELL_WIDTH * 2.0);
        let height = (size.height - TAB_BAR_HEIGHT - 2.0 * TERMINAL_PADDING).max(CELL_HEIGHT * 2.0);
        assert_eq!(((width / CELL_WIDTH).floor() as u16).max(2), 2);
        assert_eq!(((height / CELL_HEIGHT).floor() as u16).max(2), 2);
    }
}
