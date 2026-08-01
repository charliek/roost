use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iced::keyboard::{self, key::Named, Key};
use iced::widget::Id;
use iced::widget::{button, canvas, column, container, row, scrollable, stack, text, text_input};
use iced::{window, Color, Element, Fill, Size, Task};
use roost_engine::git_metrics;
use roost_engine::ipc::{
    ClipboardOp, DumpData, ExpandSelectionData, IpcHandler, ResolvedCellData, ResolvedCellsData,
    SelectionData, UiRequest,
};
use roost_engine::osc::{ClipboardTarget, OscAction, OscColorSnapshot, OscRouter};
use roost_engine::pointer::{MotionEmitter, PointerAction, PointerButton};
use roost_engine::process::{self, ProcessRequest};
use roost_engine::session::{InputCapture, TabOutput, TabSession};
use roost_engine::single_instance::InstanceLock;
use roost_engine::{LocalClient, PtySupervisor, RestoreTab, Workspace, WorkspaceEvent};
use roost_ipc::messages::{
    PaletteItemView, PalettePresentResult, PaletteStateResult, Project, SidebarDumpAgentRow,
    SidebarDumpProject, SidebarDumpResult,
};
use roost_ipc::paths::BundleProfile;
use roost_ipc::IpcServer;
use roost_ui_model::theme::Theme;
use roost_ui_model::{
    agent_palette,
    config::{self, RoostConfig},
    custom_command, notification_inbox, palette, provider,
};
use roost_vt::{
    mouse_action, mouse_button, KeyEncoder, MouseEncoder, MouseEvent, RenderState, Terminal,
    TerminalOptions, TerminalSelection,
};

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

fn sidebar_width(collapsed: bool) -> f32 {
    if collapsed {
        0.0
    } else {
        SIDEBAR_WIDTH
    }
}

fn command_palette_frame(notification_count: usize, has_providers: bool) -> palette::PaletteFrame {
    let mut items = palette::command_items(|_| None);
    let index = items
        .iter()
        .position(|item| item.id == palette::PaletteCommands::SELECT_FONT_ID)
        .map_or(items.len(), |index| index + 1);
    let mut dynamic = vec![palette::PaletteItem::new(
        palette::PaletteCommands::VIEW_AGENTS_ID,
        "Go to Agent…",
    )];
    dynamic.extend(notification_inbox::command_items(notification_count));
    items.splice(index..index, dynamic);
    if has_providers {
        items.push(palette::PaletteItem::new(
            "custom_commands",
            "Custom Commands…",
        ));
    }
    palette::PaletteFrame::new("commands", "Execute a command…", items)
}

fn provider_palette_frame(providers: &[provider::Provider]) -> palette::PaletteFrame {
    palette::PaletteFrame::new(
        "custom",
        "Custom commands…",
        provider::provider_items(providers),
    )
}

fn launcher_palette_frame(config: &RoostConfig) -> palette::PaletteFrame {
    palette::PaletteFrame::new(
        "launcher",
        "Run a command…",
        custom_command::launcher_items(&config.commands),
    )
}

fn theme_palette_frame(active_theme_name: &str) -> palette::PaletteFrame {
    let names = Theme::bundled_names();
    let selection = names
        .iter()
        .position(|name| name == active_theme_name)
        .unwrap_or(0);
    let items = names
        .into_iter()
        .map(|name| palette::PaletteItem::new(name.clone(), name))
        .collect();
    palette::PaletteFrame::new("themes", "Select a theme…", items).with_selection(selection)
}

fn font_palette_frame() -> palette::PaletteFrame {
    palette::PaletteFrame::new(
        "fonts",
        "Select a font…",
        vec![palette::PaletteItem::new("font:monospace", "Monospace")],
    )
}

pub enum UiTask {
    None,
    Focus(window::Id),
    FocusWidget(Id),
    Resize(window::Id, Size),
}

struct AgentMetricsResult {
    session: u64,
    claimed: Vec<String>,
    outcomes: Result<Vec<git_metrics::ProbeOutcome>, String>,
}

struct ProviderRunResult {
    palette_session: u64,
    request: u64,
    provider: provider::Provider,
    phase: provider::Phase,
    outcome: Result<provider::ProviderOutput, String>,
}

struct TerminalTab {
    terminal: Terminal,
    render_state: RenderState,
    encoder: KeyEncoder,
    mouse_encoder: MouseEncoder,
    motion_emitter: MotionEmitter,
    tracking_pointer: Option<PointerButton>,
    selection_drag_active: bool,
    selection: TerminalSelection,
    input_started_at: Instant,
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
    fn attach(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        test_mode: bool,
        theme: Theme,
    ) -> Result<Self> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: 2_000,
        })?;
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
        let mouse_encoder = MouseEncoder::new()?;
        let input_capture = test_mode.then(|| Arc::new(Mutex::new(Vec::new())));
        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
        let session = TabSession::attach(supervisor, tab_id, output_tx, input_capture.clone())?;
        Ok(Self {
            terminal,
            render_state,
            encoder,
            mouse_encoder,
            motion_emitter: MotionEmitter::new(),
            tracking_pointer: None,
            selection_drag_active: false,
            selection: TerminalSelection::new(),
            input_started_at: Instant::now(),
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

    fn dispatch_pointer(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<()> {
        let col = col.min(u32::from(self.cols.saturating_sub(1)));
        let row = row.min(u32::from(self.rows.saturating_sub(1)));
        let motion_without_button = action == PointerAction::Motion && button.is_none();
        let now = self.input_started_at.elapsed().as_secs_f64();
        if motion_without_button && !self.motion_emitter.would_emit(col, row, now) {
            return Ok(());
        }

        let cell_width = CELL_WIDTH.round().max(1.0) as u32;
        let cell_height = CELL_HEIGHT.round().max(1.0) as u32;
        self.mouse_encoder.sync_from_terminal(&self.terminal);
        self.mouse_encoder.set_size(
            u32::from(self.cols) * cell_width,
            u32::from(self.rows) * cell_height,
            cell_width,
            cell_height,
        );
        let mut event = MouseEvent::new().context("allocate terminal mouse event")?;
        event
            .set_action(pointer_action(action))
            .set_mods(mods)
            .set_position(
                (col * cell_width) as f32 + cell_width as f32 / 2.0,
                (row * cell_height) as f32 + cell_height as f32 / 2.0,
            );
        if let Some(button) = button {
            event.set_button(pointer_button(button));
        } else {
            event.clear_button();
        }
        let bytes = self
            .mouse_encoder
            .encode(&event)
            .context("encode terminal mouse event")?;
        if bytes.is_empty() {
            return Ok(());
        }
        if motion_without_button {
            self.motion_emitter.commit(col, row, now);
        }
        self.session.send_input(bytes);
        Ok(())
    }

    /// Route a native pointer gesture with terminal mouse reporting taking
    /// precedence over local selection for the lifetime of the press.
    fn handle_native_pointer(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<()> {
        let col = col.min(u32::from(self.cols.saturating_sub(1)));
        let row = row.min(u32::from(self.rows.saturating_sub(1)));
        let cell = (col as u16, row as u16);
        match action {
            PointerAction::Press if self.terminal.mouse_tracking() => {
                self.selection_drag_active = false;
                if matches!(
                    button,
                    Some(PointerButton::Left | PointerButton::Right | PointerButton::Middle)
                ) {
                    self.tracking_pointer = button;
                }
                self.dispatch_pointer(action, button, col, row, mods)
            }
            PointerAction::Motion if self.tracking_pointer.is_some() => {
                self.dispatch_pointer(action, self.tracking_pointer, col, row, mods)
            }
            PointerAction::Release if self.tracking_pointer.is_some() => {
                let captured = self.tracking_pointer.take();
                self.dispatch_pointer(action, captured, col, row, mods)
            }
            PointerAction::Motion if self.selection_drag_active => {
                self.selection.update(&self.terminal, cell.0, cell.1);
                Ok(())
            }
            PointerAction::Release if self.selection_drag_active => {
                self.selection_drag_active = false;
                self.selection.update(&self.terminal, cell.0, cell.1);
                Ok(())
            }
            PointerAction::Motion if self.terminal.mouse_tracking() => {
                self.dispatch_pointer(action, button, col, row, mods)
            }
            PointerAction::Press if button == Some(PointerButton::Left) => {
                self.selection_drag_active = self.selection.begin(&self.terminal, cell.0, cell.1);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn selection_dump(&mut self) -> Result<Option<SelectionData>> {
        Ok(self
            .selection
            .snapshot(&self.terminal, &mut self.render_state, self.cols, self.rows)?
            .map(|snapshot| SelectionData {
                text: snapshot.text,
                anchor_visible: snapshot.anchor_visible,
                cursor_visible: snapshot.cursor_visible,
            }))
    }

    fn expand_selection_at(
        &mut self,
        col: u16,
        row: u16,
        click_count: u8,
    ) -> Result<Option<ExpandSelectionData>> {
        let row_text = TerminalSelection::row_text(&self.terminal, &mut self.render_state, row)?;
        let span = match click_count {
            2 => roost_ui_model::word_selection::expand_word(
                &row_text,
                col,
                roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS,
            ),
            _ => Some(roost_ui_model::word_selection::expand_line(&row_text)),
        };
        let Some(span) = span else {
            return Ok(None);
        };
        if !self
            .selection
            .set(&self.terminal, (span.col0, row), (span.col1, row))
        {
            return Ok(None);
        }
        let text = self.selection.selected_text(
            &self.terminal,
            &mut self.render_state,
            self.cols,
            self.rows,
        )?;
        Ok(Some(ExpandSelectionData {
            col0: span.col0,
            col1: span.col1,
            text,
        }))
    }

    fn set_window_focus(&self, focused: bool) {
        let bytes = self.terminal.encode_focus(focused);
        if !bytes.is_empty() {
            self.session.send_input(bytes);
        }
    }

    fn set_theme(&mut self, theme: Theme) -> Result<()> {
        self.terminal.set_color_foreground(theme.foreground)?;
        self.terminal.set_color_background(theme.background)?;
        self.terminal.set_color_cursor(theme.cursor)?;
        self.terminal.set_color_palette(&theme.palette)?;
        if self.terminal.mode_get(2031) {
            self.session.send_input(if theme.background.is_light() {
                b"\x1b[?997;2n".to_vec()
            } else {
                b"\x1b[?997;1n".to_vec()
            });
        }
        self.theme = theme;
        self.refresh_snapshot()
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
            selection_background: self.theme.selection_background,
            selection_spans: self
                .selection
                .visible_spans(&self.terminal, self.cols, self.rows),
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
    sidebar_agents: HashMap<i64, Vec<SidebarDumpAgentRow>>,
    notification_inbox: notification_inbox::NotificationInbox,
    workspace_events: tokio::sync::broadcast::Receiver<WorkspaceEvent>,
    ui_rx: tokio::sync::mpsc::UnboundedReceiver<UiRequest>,
    window_id: Option<window::Id>,
    pending_window_resize: Option<Size>,
    window_size: Size,
    modifiers: keyboard::Modifiers,
    test_mode: bool,
    status: Option<String>,
    config: RoostConfig,
    active_theme_name: String,
    palette: Option<palette::PaletteState>,
    palette_session: u64,
    palette_theme_at_open: Option<String>,
    palette_input_id: Id,
    palette_focus_requested: bool,
    git_probe: Arc<git_metrics::GitProbe>,
    metrics_cache: git_metrics::MetricsCache,
    metrics_tx: tokio::sync::mpsc::UnboundedSender<AgentMetricsResult>,
    metrics_rx: tokio::sync::mpsc::UnboundedReceiver<AgentMetricsResult>,
    provider_request: u64,
    provider_tx: tokio::sync::mpsc::UnboundedSender<ProviderRunResult>,
    provider_rx: tokio::sync::mpsc::UnboundedReceiver<ProviderRunResult>,
    provider_frames: HashMap<String, provider::Provider>,
    palette_present_reply:
        Option<tokio::sync::oneshot::Sender<Result<PalettePresentResult, String>>>,
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
        let config = RoostConfig::load_default();
        let active_theme_name = config
            .theme_name
            .clone()
            .unwrap_or_else(|| "roost-dark".into());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build Iced engine runtime")?;
        let workspace = Arc::new(Workspace::open(profile.state_json_path()));
        let workspace_events = workspace.subscribe();
        let supervisor = Arc::new(PtySupervisor::new());
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            profile.socket_path.clone(),
        );

        hydrate_workspace(&runtime, &client)?;

        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let (metrics_tx, metrics_rx) = tokio::sync::mpsc::unbounded_channel();
        let (provider_tx, provider_rx) = tokio::sync::mpsc::unbounded_channel();
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
            sidebar_agents: HashMap::new(),
            notification_inbox: notification_inbox::NotificationInbox::new(),
            workspace_events,
            ui_rx,
            window_id: None,
            pending_window_resize: None,
            window_size: Size::new(1100.0, 720.0),
            modifiers: keyboard::Modifiers::default(),
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            status: None,
            config,
            active_theme_name,
            palette: None,
            palette_session: 0,
            palette_theme_at_open: None,
            palette_input_id: Id::unique(),
            palette_focus_requested: false,
            git_probe: Arc::new(git_metrics::GitProbe::new()),
            metrics_cache: git_metrics::MetricsCache::default(),
            metrics_tx,
            metrics_rx,
            provider_request: 0,
            provider_tx,
            provider_rx,
            provider_frames: HashMap::new(),
            palette_present_reply: None,
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

    pub fn window_opened(&mut self, id: window::Id) -> UiTask {
        self.window_id = Some(id);
        self.pending_window_resize
            .take()
            .map_or(UiTask::None, |size| UiTask::Resize(id, size))
    }

    pub fn window_resized(&mut self, id: window::Id, size: Size) -> UiTask {
        let task = self.window_opened(id);
        if matches!(task, UiTask::None) {
            self.resize(size);
        }
        task
    }

    pub fn tick(&mut self) -> UiTask {
        let task = self.service_ui_requests();
        self.service_agent_metrics();
        self.service_provider_results();
        self.service_workspace_events();
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
        let width = (size.width
            - sidebar_width(self.workspace.sidebar_collapsed())
            - 2.0 * TERMINAL_PADDING)
            .max(CELL_WIDTH * 2.0);
        let height = (size.height - TAB_BAR_HEIGHT - 2.0 * TERMINAL_PADDING).max(CELL_HEIGHT * 2.0);
        let cols = ((width / CELL_WIDTH).floor() as u16).max(2);
        let rows = ((height / CELL_HEIGHT).floor() as u16).max(2);
        for tab in self.tabs.values_mut() {
            tab.resize(cols, rows);
        }
    }

    pub fn keyboard(&mut self, event: keyboard::Event) -> UiTask {
        if let keyboard::Event::ModifiersChanged(modifiers) = &event {
            self.modifiers = *modifiers;
        }
        if let keyboard::Event::KeyPressed { key, modifiers, .. } = &event {
            let palette_modifier = if cfg!(target_os = "macos") {
                modifiers.logo()
            } else {
                modifiers.alt()
            };
            let character = match key.as_ref() {
                Key::Character(value) => Some(value),
                _ => None,
            };
            if self.palette.is_none()
                && palette_modifier
                && modifiers.shift()
                && character.is_some_and(|value| value.eq_ignore_ascii_case("p"))
            {
                let _ = self.open_palette("commands");
                return self.take_palette_focus_task();
            }
            if self.palette.is_none()
                && palette_modifier
                && modifiers.shift()
                && character.is_some_and(|value| value.eq_ignore_ascii_case("t"))
            {
                let _ = self.open_palette("launcher");
                return self.take_palette_focus_task();
            }
            if self.palette.is_none()
                && palette_modifier
                && modifiers.shift()
                && character.is_some_and(|value| value.eq_ignore_ascii_case("e"))
            {
                let _ = self.open_palette("custom");
                return self.take_palette_focus_task();
            }
            if self.palette.is_none()
                && palette_modifier
                && modifiers.shift()
                && character.is_some_and(|value| value.eq_ignore_ascii_case("o"))
            {
                let _ = self.open_palette("agents");
                return self.take_palette_focus_task();
            }
            if self.palette.is_some() {
                match key.as_ref() {
                    Key::Named(Named::Escape) => self.palette_back_or_dismiss(),
                    Key::Named(Named::ArrowUp) => self.move_palette_selection(-1),
                    Key::Named(Named::ArrowDown) => self.move_palette_selection(1),
                    Key::Named(Named::Enter) => {
                        if let Err(error) = self.confirm_palette_selection() {
                            self.status = Some(error);
                        }
                    }
                    _ => {}
                }
                // The text input widget consumes printable events. Never let
                // a palette keystroke leak through to the active PTY.
                return UiTask::None;
            }
        } else if self.palette.is_some() {
            return UiTask::None;
        }
        let (_, active_tab) = self.workspace.active();
        let Some(tab) = self.tabs.get_mut(&active_tab) else {
            return UiTask::None;
        };
        let bytes = input::encode_press(&mut tab.encoder, &tab.terminal, event);
        tab.session.send_input(bytes);
        UiTask::None
    }

    pub fn pointer(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
    ) {
        let (_, active_tab) = self.workspace.active();
        let Some(tab) = self.tabs.get_mut(&active_tab) else {
            return;
        };
        if let Err(error) = tab.handle_native_pointer(
            action,
            button,
            col,
            row,
            input::ghostty_modifiers(self.modifiers),
        ) {
            tracing::warn!(?error, active_tab, "terminal pointer dispatch failed");
        }
        if let Err(error) = tab.refresh_snapshot() {
            tracing::warn!(?error, active_tab, "terminal selection refresh failed");
        }
    }

    pub fn set_window_focus(&mut self, focused: bool) {
        self.workspace.set_window_focused(focused);
        if let Some(tab) = self.tabs.get(&self.workspace.active().1) {
            tab.set_window_focus(focused);
        }
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
            let mut project_group = column![button(text(label).size(14))
                .width(Fill)
                .on_press(Message::ProjectSelected(project.id))]
            .spacing(2);
            if self.config.show_sidebar_agents {
                for agent in self.sidebar_agents.get(&project.id).into_iter().flatten() {
                    let name = if agent.is_active {
                        format!("{}  ←", agent.name)
                    } else {
                        agent.name.clone()
                    };
                    let detail = format!("{}  ·  {}", agent.status_text, agent.time_text);
                    let row = row![
                        text("●").size(12).color(agent_color(agent.lifecycle)),
                        column![
                            text(name).size(12),
                            text(detail).size(10).color(Color::from_rgb8(160, 164, 176))
                        ]
                        .spacing(1)
                    ]
                    .spacing(7);
                    project_group = project_group.push(
                        button(row)
                            .width(Fill)
                            .padding([5, 8])
                            .on_press(Message::AgentSelected(agent.tab_id)),
                    );
                }
            }
            sidebar = sidebar.push(project_group);
        }
        if let Some(status) = &self.status {
            sidebar = sidebar.push(text(status).size(11).color(Color::from_rgb8(238, 120, 120)));
        }
        sidebar = sidebar.push(
            button(text("Hide Sidebar").size(11))
                .width(Fill)
                .on_press(Message::ToggleSidebar),
        );
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
        let collapsed = self.workspace.sidebar_collapsed();
        let mut tabs = row![].spacing(6).padding([7, 10]);
        if collapsed {
            tabs = tabs.push(button(text("☰")).on_press(Message::ToggleSidebar));
        }
        for tab in active_project_tabs {
            let title = if tab.title.is_empty() {
                "shell"
            } else {
                &tab.title
            };
            let label = if tab.id == active_tab {
                format!("● {title}")
            } else if tab.has_notification {
                format!("• {title}")
            } else {
                title.to_string()
            };
            tabs = tabs.push(button(text(label).size(13)).on_press(Message::TabSelected(tab.id)));
        }
        tabs = tabs.push(button(text("+")).on_press(Message::NewTab));
        let notification_count = self.notification_inbox.count();
        let notification_label = if notification_count == 0 {
            "Notifications".to_string()
        } else {
            format!("Notifications ({notification_count})")
        };
        tabs = tabs
            .push(button(text(notification_label).size(11)).on_press(Message::OpenNotifications));
        let tab_bar = container(tabs)
            .height(TAB_BAR_HEIGHT)
            .width(Fill)
            .style(container::dark);

        let terminal: Element<'_, Message> = match self.tabs.get(&active_tab) {
            Some(tab) => canvas(TerminalCanvas {
                tab_id: active_tab,
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
        let main = column![tab_bar, terminal].width(Fill).height(Fill);
        let content: Element<'_, Message> = if collapsed {
            main.width(Fill).height(Fill).into()
        } else {
            row![sidebar, main].width(Fill).height(Fill).into()
        };
        let Some(palette) = &self.palette else {
            return content;
        };

        let frame = palette.current();
        let input = text_input(&frame.placeholder, &frame.query)
            .id(self.palette_input_id.clone())
            .on_input(Message::PaletteQueryChanged)
            .on_submit(Message::PaletteConfirm)
            .padding(12)
            .size(16);
        let mut items = column![].spacing(4);
        for (index, matched) in palette.matches().into_iter().enumerate() {
            let selected = index == frame.selection;
            let marker = if selected { "› " } else { "  " };
            let mut label = column![text(format!("{marker}{}", matched.item.title)).size(14)];
            if let Some(agent) = matched.item.agent {
                let metrics = agent.metrics_text.as_deref().unwrap_or("…");
                label = label.push(
                    text(format!(
                        "{}  ·  {}  ·  {}",
                        agent.status_text, agent.time_text, metrics
                    ))
                    .size(11)
                    .color(agent_color(agent.effective_lifecycle)),
                );
            }
            if let Some(subtitle) = matched.item.subtitle {
                label = label.push(
                    text(subtitle)
                        .size(11)
                        .color(Color::from_rgb8(160, 164, 176)),
                );
            }
            if let Some(trailing) = matched.item.trailing_text {
                label = label.push(
                    text(trailing)
                        .size(10)
                        .color(Color::from_rgb8(132, 136, 148)),
                );
            }
            items = items.push(
                button(label)
                    .width(Fill)
                    .padding([8, 10])
                    .on_press(Message::PaletteActivate(matched.item.id)),
            );
        }
        let panel = container(column![input, scrollable(items).height(420)].spacing(8))
            .width(560)
            .padding(12)
            .style(container::dark);
        let overlay = container(panel)
            .width(Fill)
            .height(Fill)
            .padding([60, 40])
            .center_x(Fill);
        stack![content, overlay].width(Fill).height(Fill).into()
    }

    pub fn select_project(&mut self, project_id: i64) {
        let tab_id = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.first())
            .map(|tab| tab.id);
        if let Some(tab_id) = tab_id {
            let _ = self.focus_tab_and_clear(tab_id, false);
        }
    }

    pub fn select_tab(&mut self, tab_id: i64) {
        let _ = self.focus_tab_and_clear(tab_id, false);
    }

    pub fn select_agent(&mut self, tab_id: i64) {
        let _ = self.focus_tab_and_clear(tab_id, true);
    }

    pub fn open_notifications(&mut self) {
        if let Err(error) = self.open_palette("notifications") {
            self.status = Some(error);
        }
    }

    pub fn toggle_sidebar(&mut self) {
        self.set_sidebar_collapsed(!self.workspace.sidebar_collapsed());
    }

    fn set_sidebar_collapsed(&mut self, collapsed: bool) {
        self.workspace.set_sidebar_collapsed(collapsed);
        self.resize(self.window_size);
    }

    fn toggle_sidebar_agents(&mut self) {
        self.config.show_sidebar_agents = !self.config.show_sidebar_agents;
        let value = if self.config.show_sidebar_agents {
            "true"
        } else {
            "false"
        };
        if let Some(path) = config::config_path() {
            if let Err(error) = config::set_key(&path, "show-sidebar-agents", value) {
                self.status = Some(format!("persist show-sidebar-agents: {error}"));
            }
        }
    }

    pub fn new_tab(&mut self) {
        let (project_id, _) = self.workspace.active();
        if project_id == 0 {
            return;
        }
        let cwd = self.launch_cwd(project_id);
        if let Err(error) = self.runtime.block_on(self.client.open_tab(
            project_id,
            &cwd,
            "",
            &[],
            u32::from(DEFAULT_COLS),
            u32::from(DEFAULT_ROWS),
        )) {
            self.status = Some(error.to_string());
        }
        self.reconcile();
    }

    pub fn palette_query_changed(&mut self, query: &str) {
        let _ = self.query_palette(query);
    }

    pub fn palette_activate(&mut self, id: &str) {
        if let Err(error) = self.activate_palette(id) {
            self.status = Some(error);
        }
    }

    pub fn palette_confirm(&mut self) {
        if let Err(error) = self.confirm_palette_selection() {
            self.status = Some(error);
        }
    }

    fn open_palette(&mut self, kind: &str) -> Result<(), String> {
        let frame = match kind {
            "" | "commands" => command_palette_frame(
                self.notification_inbox.count(),
                !self.config.providers.is_empty(),
            ),
            "launcher" => launcher_palette_frame(&self.config),
            "agents" => {
                agent_palette::agent_frame(&self.workspace.snapshot(), agent_palette::now_unix())
            }
            "notifications" => notification_inbox::frame(&self.notification_inbox),
            "custom" => provider_palette_frame(&self.config.providers),
            _ => return Err(format!("unknown palette kind {kind:?}")),
        };
        self.dismiss_palette();
        self.palette_session = self.palette_session.wrapping_add(1).max(1);
        self.palette_theme_at_open = Some(self.active_theme_name.clone());
        self.palette = Some(palette::PaletteState::new(frame));
        self.palette_focus_requested = true;
        self.refresh_agent_palette();
        Ok(())
    }

    fn present_palette(
        &mut self,
        title: String,
        placeholder: String,
        items: Vec<(String, String, Option<String>)>,
        reply: tokio::sync::oneshot::Sender<Result<PalettePresentResult, String>>,
    ) {
        self.dismiss_palette();
        self.palette_session = self.palette_session.wrapping_add(1).max(1);
        let placeholder = if !placeholder.is_empty() {
            placeholder
        } else if !title.is_empty() {
            title
        } else {
            "Select…".to_string()
        };
        let items = items
            .into_iter()
            .map(|(id, title, subtitle)| {
                palette::PaletteItem::new(id, title).with_subtitle(subtitle)
            })
            .collect();
        self.palette = Some(palette::PaletteState::new(palette::PaletteFrame::new(
            "present",
            placeholder,
            items,
        )));
        self.palette_present_reply = Some(reply);
        self.palette_focus_requested = true;
    }

    fn take_palette_focus_task(&mut self) -> UiTask {
        if std::mem::take(&mut self.palette_focus_requested) {
            UiTask::FocusWidget(self.palette_input_id.clone())
        } else {
            UiTask::None
        }
    }

    fn palette_state_result(&self) -> PaletteStateResult {
        let Some(state) = &self.palette else {
            return PaletteStateResult::default();
        };
        let frame = state.current();
        PaletteStateResult {
            open: true,
            frame: Some(frame.id.clone()),
            query: frame.query.clone(),
            selection: u32::try_from(frame.selection).unwrap_or(u32::MAX),
            items: state
                .matches()
                .into_iter()
                .map(|matched| PaletteItemView {
                    id: matched.item.id,
                    title: matched.item.title,
                    subtitle: matched.item.subtitle,
                    agent: matched.item.agent,
                })
                .collect(),
            // Iced's scrollable does not expose row geometry. `None` is
            // the contract's explicit honest value for that adapter.
            selected_in_view: None,
        }
    }

    fn query_palette(&mut self, query: &str) -> Result<PaletteStateResult, String> {
        let state = self
            .palette
            .as_mut()
            .ok_or_else(|| "no palette open".to_string())?;
        state.set_query(query);
        self.preview_selected_theme()?;
        Ok(self.palette_state_result())
    }

    fn move_palette_selection(&mut self, delta: isize) {
        if let Some(state) = &mut self.palette {
            state.move_selection(delta);
        }
        if let Err(error) = self.preview_selected_theme() {
            self.status = Some(error);
        }
    }

    fn confirm_palette_selection(&mut self) -> Result<PaletteStateResult, String> {
        let id = self
            .palette
            .as_ref()
            .and_then(palette::PaletteState::selected_item)
            .ok_or_else(|| "no actionable palette row selected".to_string())?
            .id;
        self.activate_palette(&id)
    }

    fn activate_palette(&mut self, id: &str) -> Result<PaletteStateResult, String> {
        let (frame_id, item) = {
            let state = self
                .palette
                .as_mut()
                .ok_or_else(|| "no palette open".to_string())?;
            let matches = state.matches();
            let index = matches
                .iter()
                .position(|matched| matched.item.id == id)
                .ok_or_else(|| format!("no palette row with id {id:?}"))?;
            state.set_selection(index);
            let item = matches[index].item.clone();
            (state.current().id.clone(), item)
        };

        if !item.actionable {
            return Ok(self.palette_state_result());
        }

        match frame_id.as_str() {
            "commands" => match item.id.as_str() {
                palette::PaletteCommands::SELECT_THEME_ID => {
                    if let Some(state) = &mut self.palette {
                        state.push(theme_palette_frame(&self.active_theme_name));
                    }
                }
                palette::PaletteCommands::SELECT_FONT_ID => {
                    if let Some(state) = &mut self.palette {
                        state.push(font_palette_frame());
                    }
                }
                palette::PaletteCommands::VIEW_AGENTS_ID => {
                    if let Some(state) = &mut self.palette {
                        state.push(agent_palette::agent_frame(
                            &self.workspace.snapshot(),
                            agent_palette::now_unix(),
                        ));
                    }
                    self.refresh_agent_palette();
                }
                palette::PaletteCommands::VIEW_NOTIFICATIONS_ID => {
                    if let Some(state) = &mut self.palette {
                        state.push(notification_inbox::frame(&self.notification_inbox));
                    }
                }
                palette::PaletteCommands::CLEAR_NOTIFICATIONS_ID => {
                    let tab_ids = self.notification_inbox.tab_ids();
                    let mut first_error = None;
                    for tab_id in tab_ids {
                        if let Err(error) = self.workspace.set_tab_has_notification(tab_id, false) {
                            first_error.get_or_insert_with(|| error.to_string());
                        }
                    }
                    self.palette = None;
                    self.palette_theme_at_open = None;
                    if let Some(error) = first_error {
                        return Err(error);
                    }
                }
                "new_tab" => {
                    self.palette = None;
                    self.palette_theme_at_open = None;
                    self.new_tab();
                }
                "close_tab" => {
                    let tab_id = self.workspace.active().1;
                    self.palette = None;
                    self.palette_theme_at_open = None;
                    self.runtime
                        .block_on(self.client.close_tab(tab_id))
                        .map_err(|error| error.to_string())?;
                    self.reconcile();
                }
                "cycle_tab_next" => {
                    self.cycle_tab(1)?;
                    self.palette = None;
                    self.palette_theme_at_open = None;
                }
                "cycle_tab_prev" => {
                    self.cycle_tab(-1)?;
                    self.palette = None;
                    self.palette_theme_at_open = None;
                }
                "toggle_sidebar" => {
                    self.palette = None;
                    self.palette_theme_at_open = None;
                    self.toggle_sidebar();
                }
                "toggle_sidebar_agents" => {
                    self.palette = None;
                    self.palette_theme_at_open = None;
                    self.toggle_sidebar_agents();
                }
                "jump_to_unread" => {
                    let active_project_id = self.workspace.active().0;
                    let target = notification_inbox::next_unread(
                        &self.notification_inbox,
                        active_project_id,
                    );
                    if let Some(tab_id) = target {
                        self.focus_tab_and_clear(tab_id, true)?;
                    }
                    self.palette = None;
                    self.palette_theme_at_open = None;
                }
                "custom_commands" => {
                    if let Some(state) = &mut self.palette {
                        state.push(provider_palette_frame(&self.config.providers));
                    }
                }
                command => {
                    return Err(format!(
                        "palette command {command:?} is not implemented by Iced"
                    ));
                }
            },
            "launcher" => {
                let index = custom_command::launch_index(&item.id)
                    .filter(|index| *index < self.config.commands.len())
                    .ok_or_else(|| format!("launcher row {:?} cannot be run", item.id))?;
                let command = self.config.commands[index].clone();
                let (project_id, _) = self.workspace.active();
                let cwd = self.launch_cwd(project_id);
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                let argv = custom_command::launch_argv(&shell, &command);
                self.palette = None;
                self.palette_theme_at_open = None;
                self.runtime
                    .block_on(self.client.open_tab(
                        project_id,
                        &cwd,
                        &command.title,
                        &argv,
                        u32::from(DEFAULT_COLS),
                        u32::from(DEFAULT_ROWS),
                    ))
                    .map_err(|error| error.to_string())?;
                self.reconcile();
            }
            "themes" => {
                self.apply_theme_name(&item.id)?;
                self.palette = None;
                self.palette_theme_at_open = None;
            }
            "fonts" => {
                self.palette = None;
                self.palette_theme_at_open = None;
            }
            agent_palette::FRAME_ID => {
                let tab_id = agent_palette::agent_tab_id(&item.id)
                    .ok_or_else(|| format!("agent row {:?} cannot be activated", item.id))?;
                self.focus_tab_and_clear(tab_id, true)?;
                self.palette = None;
                self.palette_theme_at_open = None;
            }
            "notifications" => {
                let tab_id = notification_inbox::tab_id(&item.id)
                    .ok_or_else(|| format!("notification row {:?} cannot be activated", item.id))?;
                self.focus_tab_and_clear(tab_id, true)?;
                self.palette = None;
                self.palette_theme_at_open = None;
            }
            "custom" => {
                let index = provider::provider_index(&item.id)
                    .filter(|index| *index < self.config.providers.len())
                    .ok_or_else(|| format!("provider row {:?} cannot be activated", item.id))?;
                self.spawn_provider(
                    self.config.providers[index].clone(),
                    provider::Phase::List,
                    None,
                );
            }
            "present" => {
                if let Some(reply) = self.palette_present_reply.take() {
                    let _ = reply.send(Ok(PalettePresentResult {
                        selected_id: Some(item.id),
                        dismissed: false,
                    }));
                }
                self.palette = None;
                self.palette_theme_at_open = None;
            }
            frame if frame.starts_with("provider:items:") => {
                let provider = self
                    .provider_frames
                    .get(frame)
                    .cloned()
                    .ok_or_else(|| format!("provider frame {frame:?} is stale"))?;
                self.spawn_provider(provider, provider::Phase::Activate, Some(item.id));
            }
            frame => {
                return Err(format!(
                    "palette frame {frame:?} is not implemented by Iced"
                ))
            }
        }
        Ok(self.palette_state_result())
    }

    fn preview_selected_theme(&mut self) -> Result<(), String> {
        let selected = self.palette.as_ref().and_then(|state| {
            (state.current().id == "themes")
                .then(|| state.selected_item().map(|item| item.id))
                .flatten()
        });
        if let Some(name) = selected {
            self.apply_theme_name(&name)?;
        }
        Ok(())
    }

    fn apply_theme_name(&mut self, name: &str) -> Result<(), String> {
        if self.active_theme_name == name {
            return Ok(());
        }
        let theme = Theme::load_bundled(name);
        for (tab_id, tab) in &mut self.tabs {
            tab.set_theme(theme.clone())
                .map_err(|error| format!("apply theme to tab {tab_id}: {error}"))?;
        }
        self.active_theme_name = name.to_string();
        Ok(())
    }

    fn palette_back_or_dismiss(&mut self) {
        let is_root = self
            .palette
            .as_ref()
            .is_none_or(palette::PaletteState::is_root);
        if is_root {
            self.dismiss_palette();
            return;
        }
        let was_theme = self
            .palette
            .as_ref()
            .is_some_and(|state| state.current().id == "themes");
        if let Some(state) = &mut self.palette {
            let _ = state.pop();
        }
        if was_theme {
            self.restore_palette_theme();
        }
    }

    fn dismiss_palette(&mut self) {
        self.restore_palette_theme();
        self.palette = None;
        self.palette_theme_at_open = None;
        self.provider_request = self.provider_request.wrapping_add(1);
        self.provider_frames.clear();
        if let Some(reply) = self.palette_present_reply.take() {
            let _ = reply.send(Ok(PalettePresentResult {
                selected_id: None,
                dismissed: true,
            }));
        }
    }

    fn restore_palette_theme(&mut self) {
        if let Some(name) = self.palette_theme_at_open.clone() {
            if let Err(error) = self.apply_theme_name(&name) {
                self.status = Some(error);
            }
        }
    }

    fn cycle_tab(&mut self, delta: isize) -> Result<(), String> {
        let (project_id, active_tab) = self.workspace.active();
        let tabs = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.tabs.as_slice())
            .unwrap_or_default();
        if tabs.is_empty() {
            return Ok(());
        }
        let current = tabs
            .iter()
            .position(|tab| tab.id == active_tab)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(tabs.len() as isize) as usize;
        self.focus_tab_and_clear(tabs[next].id, false)?;
        Ok(())
    }

    fn focus_tab_and_clear(&mut self, tab_id: i64, reveal_sidebar: bool) -> Result<(), String> {
        self.workspace
            .focus_tab(tab_id)
            .map_err(|error| error.to_string())?;
        self.workspace
            .set_tab_has_notification(tab_id, false)
            .map_err(|error| error.to_string())?;
        if reveal_sidebar {
            self.set_sidebar_collapsed(false);
        }
        Ok(())
    }

    fn launch_cwd(&self, project_id: i64) -> String {
        let active_tab = self.workspace.active().1;
        if let Some(native) = self.supervisor.foreground_cwd(active_tab) {
            if !native.is_empty() {
                return native;
            }
        }
        self.projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.iter().find(|tab| tab.id == active_tab))
            .map(|tab| tab.cwd.clone())
            .unwrap_or_default()
    }

    fn reconcile(&mut self) {
        // Full authoritative snapshot on every UI tick is the recovery path
        // for a slow consumer: deltas are an optimization, never UI truth.
        self.projects = self.workspace.snapshot();
        self.reconcile_notification_inbox();
        self.refresh_notification_palette();
        self.refresh_sidebar_agents();
        self.refresh_agent_palette();
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
                TerminalTab::attach(
                    Arc::clone(&self.supervisor),
                    tab_id,
                    self.test_mode,
                    Theme::load_bundled(&self.active_theme_name),
                )
            };
            match attached {
                Ok(mut tab) => {
                    let size = self.window_size;
                    let width = (size.width
                        - sidebar_width(self.workspace.sidebar_collapsed())
                        - 2.0 * TERMINAL_PADDING)
                        .max(CELL_WIDTH * 2.0);
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

    fn service_workspace_events(&mut self) {
        loop {
            match self.workspace_events.try_recv() {
                Ok(WorkspaceEvent::NotificationFired {
                    tab_id,
                    title: _,
                    body,
                }) => {
                    if let Some((project_id, title)) = self.notification_title(tab_id) {
                        self.notification_inbox.upsert(
                            notification_inbox::NotificationRecord::new(
                                tab_id, project_id, title, body,
                            ),
                        );
                    }
                }
                Ok(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: false,
                })
                | Ok(WorkspaceEvent::TabClosed { tab_id }) => {
                    self.notification_inbox.remove(tab_id);
                }
                Ok(WorkspaceEvent::ProjectDeleted { project_id }) => {
                    let stale: Vec<i64> = self
                        .notification_inbox
                        .snapshot()
                        .iter()
                        .filter(|record| record.project_id == project_id)
                        .map(|record| record.tab_id)
                        .collect();
                    for tab_id in stale {
                        self.notification_inbox.remove(tab_id);
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(dropped)) => {
                    tracing::warn!(dropped, "Iced workspace event consumer lagged; resyncing");
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
    }

    fn notification_title(&self, tab_id: i64) -> Option<(i64, String)> {
        self.workspace.snapshot().into_iter().find_map(|project| {
            project.tabs.into_iter().find_map(|tab| {
                (tab.id == tab_id).then(|| {
                    let tab_title = if !tab.title.is_empty() {
                        tab.title
                    } else if !tab.cwd.is_empty() {
                        tab.cwd
                    } else {
                        "Tab".to_string()
                    };
                    (
                        project.id,
                        notification_inbox::compose_title(&project.name, &tab_title),
                    )
                })
            })
        })
    }

    fn reconcile_notification_inbox(&mut self) {
        let pending_rows: Vec<(i64, i64, String)> = self
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .tabs
                    .iter()
                    .filter(|tab| tab.has_notification)
                    .map(move |tab| {
                        let tab_title = if !tab.title.is_empty() {
                            tab.title.clone()
                        } else if !tab.cwd.is_empty() {
                            tab.cwd.clone()
                        } else {
                            "Tab".to_string()
                        };
                        (
                            tab.id,
                            project.id,
                            notification_inbox::compose_title(&project.name, &tab_title),
                        )
                    })
            })
            .collect();
        let pending_ids: HashSet<i64> = pending_rows.iter().map(|row| row.0).collect();
        let stale: Vec<i64> = self
            .notification_inbox
            .tab_ids()
            .into_iter()
            .filter(|tab_id| !pending_ids.contains(tab_id))
            .collect();
        for tab_id in stale {
            self.notification_inbox.remove(tab_id);
        }

        let existing: HashSet<i64> = self.notification_inbox.tab_ids().into_iter().collect();
        let slots = notification_inbox::CAP.saturating_sub(self.notification_inbox.count());
        let mut additions = Vec::with_capacity(slots);
        for row in pending_rows
            .into_iter()
            .filter(|row| !existing.contains(&row.0))
            .take(slots)
        {
            additions.push(row);
        }
        // Insert in reverse snapshot order so the first deterministic
        // project/tab fallback remains at the front after repeated prepends.
        while let Some((tab_id, project_id, title)) = additions.pop() {
            self.notification_inbox
                .upsert(notification_inbox::NotificationRecord::new(
                    tab_id, project_id, title, "",
                ));
        }
    }

    fn refresh_notification_palette(&mut self) {
        let has_notifications = self.palette.as_ref().is_some_and(|state| {
            state
                .frames()
                .iter()
                .any(|frame| frame.id == "notifications")
        });
        if !has_notifications {
            return;
        }
        let items = notification_inbox::frame(&self.notification_inbox).items;
        if let Some(state) = &mut self.palette {
            state.update_items("notifications", items);
        }
    }

    fn refresh_sidebar_agents(&mut self) {
        let active_tab = self.workspace.active().1;
        let now = agent_palette::now_unix();
        self.sidebar_agents = self
            .projects
            .iter()
            .map(|project| {
                let rows = agent_palette::sidebar_agents(project, now)
                    .into_iter()
                    .map(|row| SidebarDumpAgentRow {
                        tab_id: row.tab_id,
                        name: row.name,
                        lifecycle: row.lifecycle,
                        status_text: row.status_text,
                        time_text: row.time_text,
                        is_active: row.tab_id == active_tab,
                    })
                    .collect();
                (project.id, rows)
            })
            .collect();
    }

    fn sidebar_dump(&self) -> SidebarDumpResult {
        let projects = self
            .workspace
            .snapshot()
            .into_iter()
            .map(|project| SidebarDumpProject {
                project_id: project.id,
                agents: self
                    .sidebar_agents
                    .get(&project.id)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        SidebarDumpResult {
            agents_visible: self.config.show_sidebar_agents,
            projects,
        }
    }

    fn refresh_agent_palette(&mut self) {
        let has_agents = self.palette.as_ref().is_some_and(|state| {
            state
                .frames()
                .iter()
                .any(|frame| frame.id == agent_palette::FRAME_ID)
        });
        if !has_agents {
            return;
        }
        // Do not rebuild from `self.projects`: an IPC workspace mutation and
        // `palette.open` can be serviced in the same event-loop turn, before
        // the adapter's next general reconcile. The engine snapshot is the
        // authoritative resync source for this live frame.
        let projects = self.workspace.snapshot();
        let cwds = agent_palette::agent_tab_cwds(&projects);
        let mut items = agent_palette::agent_items(&projects, agent_palette::now_unix());
        self.apply_metrics_cache(&cwds, &mut items);
        if let Some(state) = &mut self.palette {
            state.update_items(agent_palette::FRAME_ID, items);
        }
        self.spawn_agent_metrics(&cwds);
    }

    fn apply_metrics_cache(&self, cwds: &HashMap<i64, String>, items: &mut [palette::PaletteItem]) {
        for item in items {
            let Some(tab_id) = agent_palette::agent_tab_id(&item.id) else {
                continue;
            };
            let (Some(agent), Some(cwd)) = (item.agent.as_mut(), cwds.get(&tab_id)) else {
                continue;
            };
            agent.metrics_text = self
                .metrics_cache
                .text_for_session(self.palette_session, cwd)
                .map(str::to_string);
        }
    }

    fn spawn_agent_metrics(&mut self, cwds: &HashMap<i64, String>) {
        self.metrics_cache.begin_session(self.palette_session);
        let claimed = self.metrics_cache.claim_unprobed(cwds.values().cloned());
        if claimed.is_empty() {
            return;
        }
        let known = self.metrics_cache.known_roots();
        let probe = Arc::clone(&self.git_probe);
        let tx = self.metrics_tx.clone();
        let session = self.palette_session;
        let failed_claims = claimed.clone();
        let task = self
            .runtime
            .spawn(git_metrics::probe_batch(probe, claimed, known));
        self.runtime.spawn(async move {
            let outcomes = task.await.map_err(|error| error.to_string());
            let _ = tx.send(AgentMetricsResult {
                session,
                claimed: failed_claims,
                outcomes,
            });
        });
    }

    fn service_agent_metrics(&mut self) {
        while let Ok(result) = self.metrics_rx.try_recv() {
            if self.palette.is_none()
                || result.session != self.palette_session
                || self.metrics_cache.session() != result.session
            {
                continue;
            }
            match result.outcomes {
                Ok(outcomes) => {
                    for outcome in outcomes {
                        let Some(root) = outcome.root.clone() else {
                            if let git_metrics::ProbeValue::Measured(Err(error)) = &outcome.value {
                                tracing::debug!(cwd = %outcome.cwd, reason = %error, "no git metrics");
                            }
                            self.metrics_cache.store_unresolved(&outcome.cwd);
                            continue;
                        };
                        let text = match outcome.value {
                            git_metrics::ProbeValue::Reused(text) => text,
                            git_metrics::ProbeValue::Measured(Ok(metrics)) => metrics.text(),
                            git_metrics::ProbeValue::Measured(Err(error)) => {
                                tracing::debug!(cwd = %outcome.cwd, reason = %error, "no git metrics");
                                git_metrics::UNKNOWN.to_string()
                            }
                        };
                        self.metrics_cache.store_root(&outcome.cwd, &root, text);
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "git metrics task failed");
                    for cwd in result.claimed {
                        self.metrics_cache.store_unresolved(&cwd);
                    }
                }
            }
        }
    }

    fn spawn_provider(
        &mut self,
        provider: provider::Provider,
        phase: provider::Phase,
        selected_id: Option<String>,
    ) {
        self.provider_request = self.provider_request.wrapping_add(1).max(1);
        let request = self.provider_request;
        let palette_session = self.palette_session;
        let context = self.provider_context(selected_id);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let argv =
            provider::invocation_argv(&shell, &provider.run, provider.shell_interpret, phase);
        let env = provider::invocation_env(phase, &context);
        let has_roostctl = env.iter().any(|(key, _)| key == "ROOST_ROOSTCTL");
        let process_request = ProcessRequest {
            argv,
            env,
            env_remove: (!has_roostctl)
                .then(|| "ROOST_ROOSTCTL".to_string())
                .into_iter()
                .collect(),
            stdin: provider::invocation_stdin(phase, &context).into_bytes(),
            cwd: (!context.active_cwd.is_empty()).then(|| context.active_cwd.into()),
            timeout: Duration::from_secs(provider.timeout_secs),
        };
        let tx = self.provider_tx.clone();
        let result_provider = provider;
        let task = self.runtime.spawn(async move {
            let stdout = process::run(process_request)
                .await
                .map_err(|error| error.to_string())?
                .stdout;
            match phase {
                provider::Phase::List => provider::parse_provider_output(&stdout),
                provider::Phase::Activate => provider::parse_activate_output(&stdout),
            }
        });
        self.runtime.spawn(async move {
            let outcome = task
                .await
                .map_err(|error| format!("provider task failed: {error}"))
                .and_then(|outcome| outcome);
            let _ = tx.send(ProviderRunResult {
                palette_session,
                request,
                provider: result_provider,
                phase,
                outcome,
            });
        });
    }

    fn provider_context(&self, selected_id: Option<String>) -> provider::ProviderContext {
        let (project_id, tab_id) = self.workspace.active();
        let active_title = self
            .workspace
            .snapshot()
            .into_iter()
            .flat_map(|project| project.tabs)
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.title)
            .unwrap_or_default();
        provider::ProviderContext {
            socket: self.client.socket_path.to_string_lossy().into_owned(),
            query: self
                .palette
                .as_ref()
                .map(|state| state.current().query.clone())
                .unwrap_or_default(),
            selected_id,
            active_tab_id: (tab_id != 0).then_some(tab_id),
            active_project_id: (project_id != 0).then_some(project_id),
            active_cwd: if project_id != 0 {
                self.launch_cwd(project_id)
            } else {
                String::new()
            },
            active_title,
            roostctl: process::sibling_executable("roostctl"),
        }
    }

    fn service_provider_results(&mut self) {
        while let Ok(result) = self.provider_rx.try_recv() {
            if self.palette.is_none()
                || result.palette_session != self.palette_session
                || result.request != self.provider_request
            {
                continue;
            }
            match result.outcome {
                Ok(output)
                    if result.phase == provider::Phase::Activate && output.items.is_empty() =>
                {
                    self.dismiss_palette();
                }
                Ok(output) => {
                    let placeholder = if output.placeholder.is_empty() {
                        format!("{}…", result.provider.title)
                    } else {
                        output.placeholder.clone()
                    };
                    let items = provider::output_palette_items(&output, result.provider.limit);
                    let frame_id = format!("provider:items:{}", result.request);
                    self.provider_frames
                        .insert(frame_id.clone(), result.provider);
                    if let Some(state) = &mut self.palette {
                        state.push(palette::PaletteFrame::new(frame_id, placeholder, items));
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "provider run failed");
                    let frame_id = format!("provider:items:{}", result.request);
                    let items =
                        vec![
                            palette::PaletteItem::new("provider:_error", "Provider failed")
                                .with_subtitle(Some(error))
                                .with_actionable(false),
                        ];
                    if let Some(state) = &mut self.palette {
                        state.push(palette::PaletteFrame::new(
                            frame_id,
                            "Provider error",
                            items,
                        ));
                    }
                }
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
                    let collapsed = self.workspace.sidebar_collapsed();
                    let _ = reply.send(Ok((
                        f64::from(self.window_size.width),
                        f64::from(self.window_size.height),
                        f64::from(sidebar_width(collapsed)),
                        collapsed,
                    )));
                }
                UiRequest::WindowResize {
                    width,
                    height,
                    reply,
                } => {
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else {
                        let size = Size::new(width as f32, height as f32);
                        // Some Wayland compositors retain authority over the
                        // toplevel size and may ignore a client request. Apply
                        // the requested logical geometry immediately for the
                        // deterministic test port; a compositor Resized event
                        // remains authoritative if it sends one afterward.
                        self.resize(size);
                        if let Some(id) = self.window_id {
                            task = UiTask::Resize(id, size);
                        } else {
                            // IPC can become reachable just before Iced emits
                            // WindowOpened. Preserve the native resize until an
                            // ID exists instead of rejecting a ready server.
                            self.pending_window_resize = Some(size);
                        }
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                UiRequest::AppSetWindowFocus { focused, reply } => {
                    let result = if self.test_mode {
                        self.set_window_focus(focused);
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
                UiRequest::PaletteOpen { kind, reply } => {
                    let result = self
                        .open_palette(&kind)
                        .map(|()| self.palette_state_result());
                    if result.is_ok() {
                        task = self.take_palette_focus_task();
                    }
                    let _ = reply.send(result);
                }
                UiRequest::PaletteState { reply } => {
                    let _ = reply.send(Ok(self.palette_state_result()));
                }
                UiRequest::PaletteQuery { query, reply } => {
                    let _ = reply.send(self.query_palette(&query));
                }
                UiRequest::PaletteActivate { id, reply } => {
                    let _ = reply.send(self.activate_palette(&id));
                }
                UiRequest::PaletteDismiss { reply } => {
                    self.dismiss_palette();
                    let _ = reply.send(Ok(self.palette_state_result()));
                }
                UiRequest::PalettePresent {
                    title,
                    placeholder,
                    items,
                    reply,
                } => {
                    self.present_palette(title, placeholder, items, reply);
                    task = self.take_palette_focus_task();
                }
                UiRequest::SelectionSet {
                    tab_id,
                    anchor,
                    cursor,
                    reply,
                } => {
                    let result = self
                        .tabs
                        .get_mut(&tab_id)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| {
                            if !tab.selection.set(&tab.terminal, anchor, cursor) {
                                return Err(format!(
                                    "selection coordinates are outside tab {tab_id}'s viewport"
                                ));
                            }
                            tab.refresh_snapshot().map_err(|error| error.to_string())
                        });
                    let _ = reply.send(result);
                }
                UiRequest::SelectionClear { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get_mut(&tab_id)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| {
                            tab.selection.clear();
                            tab.refresh_snapshot().map_err(|error| error.to_string())
                        });
                    let _ = reply.send(result);
                }
                UiRequest::SelectionDump { tab_id, reply } => {
                    let result = self
                        .tabs
                        .get_mut(&tab_id)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| tab.selection_dump().map_err(|error| error.to_string()));
                    let _ = reply.send(result);
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
                UiRequest::TabExpandSelectionAt {
                    tab_id,
                    col,
                    row,
                    click_count,
                    reply,
                } => {
                    let result = if !self.test_mode {
                        Err(
                            "tab.expand_selection_at requires ROOST_TEST_MODE=1 at UI launch"
                                .into(),
                        )
                    } else {
                        self.tabs
                            .get_mut(&tab_id)
                            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                            .and_then(|tab| {
                                tab.expand_selection_at(col, row, click_count)
                                    .map_err(|error| error.to_string())?
                                    .ok_or_else(|| {
                                        format!(
                                            "no word/line span at ({col}, {row}) on tab {tab_id} \
                                             (whitespace double-click, or row out of range)"
                                        )
                                    })
                            })
                    };
                    let _ = reply.send(result);
                }
                UiRequest::SidebarDump { reply } => {
                    let _ = reply.send(Ok(self.sidebar_dump()));
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
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else {
                        u16::try_from(mods)
                            .map_err(|_| format!("modifier mask {mods} exceeds u16"))
                            .and_then(|mods| {
                                self.tabs
                                    .get_mut(&tab_id)
                                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?
                                    .dispatch_pointer(kind, button, cell_x, cell_y, mods)
                                    .map_err(|error| error.to_string())
                            })
                    };
                    let _ = reply.send(result);
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

fn agent_color(lifecycle: roost_ipc::agent::AgentLifecycle) -> Color {
    use roost_ipc::agent::AgentLifecycle;
    match lifecycle {
        AgentLifecycle::Working => Color::from_rgb8(0x5f, 0xa3, 0xf0),
        AgentLifecycle::Waiting => Color::from_rgb8(0xf0, 0xa0, 0x40),
        AgentLifecycle::Finished => Color::from_rgb8(0x7a, 0x7a, 0x7a),
        AgentLifecycle::Failed => Color::from_rgb8(0xe0, 0x52, 0x52),
        AgentLifecycle::Inactive => Color::from_rgba8(0x7a, 0x7a, 0x7a, 0.5),
    }
}

fn pointer_action(action: PointerAction) -> roost_vt::MouseAction {
    match action {
        PointerAction::Press => mouse_action::PRESS,
        PointerAction::Release => mouse_action::RELEASE,
        PointerAction::Motion => mouse_action::MOTION,
    }
}

fn pointer_button(button: PointerButton) -> roost_vt::MouseButton {
    match button {
        PointerButton::Left => mouse_button::LEFT,
        PointerButton::Right => mouse_button::RIGHT,
        PointerButton::Middle => mouse_button::MIDDLE,
        PointerButton::Four => mouse_button::FOUR,
        PointerButton::Five => mouse_button::FIVE,
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
            Self::AgentSelected(tab_id) => app.select_agent(tab_id),
            Self::TabSelected(tab_id) => app.select_tab(tab_id),
            Self::NewTab => app.new_tab(),
            Self::ToggleSidebar => app.toggle_sidebar(),
            Self::OpenNotifications => app.open_notifications(),
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

    #[test]
    fn collapsed_sidebar_has_no_layout_width() {
        assert_eq!(sidebar_width(false), SIDEBAR_WIDTH);
        assert_eq!(sidebar_width(true), 0.0);
    }

    #[test]
    fn agent_colors_match_the_shipped_gtk_and_appkit_palette() {
        use roost_ipc::agent::AgentLifecycle;
        assert_eq!(
            agent_color(AgentLifecycle::Working),
            Color::from_rgb8(0x5f, 0xa3, 0xf0)
        );
        assert_eq!(
            agent_color(AgentLifecycle::Waiting),
            Color::from_rgb8(0xf0, 0xa0, 0x40)
        );
        assert_eq!(
            agent_color(AgentLifecycle::Finished),
            Color::from_rgb8(0x7a, 0x7a, 0x7a)
        );
        assert_eq!(
            agent_color(AgentLifecycle::Failed),
            Color::from_rgb8(0xe0, 0x52, 0x52)
        );
    }

    #[test]
    fn command_palette_uses_shared_ids_and_ranking() {
        let mut state = palette::PaletteState::new(command_palette_frame(2, true));
        let ids: Vec<String> = state
            .matches()
            .into_iter()
            .map(|matched| matched.item.id)
            .collect();
        assert!(ids.iter().any(|id| id == "new_tab"));
        let font = ids
            .iter()
            .position(|id| id == palette::PaletteCommands::SELECT_FONT_ID)
            .expect("font drill-in");
        assert_eq!(
            ids.get(font + 1).map(String::as_str),
            Some(palette::PaletteCommands::VIEW_AGENTS_ID)
        );
        assert_eq!(
            ids.get(font + 2).map(String::as_str),
            Some(palette::PaletteCommands::VIEW_NOTIFICATIONS_ID)
        );
        assert!(ids.iter().any(|id| id == "custom_commands"));
        state.set_query("theme");
        assert_eq!(
            state.selected_item().map(|item| item.id),
            Some(palette::PaletteCommands::SELECT_THEME_ID.to_string())
        );
    }

    #[test]
    fn theme_frame_selects_the_active_theme() {
        let frame = theme_palette_frame("roost-dark");
        assert_eq!(frame.id, "themes");
        assert!(frame.items.len() > 1);
        assert_eq!(frame.items[frame.selection].id, "roost-dark");
    }

    #[test]
    fn launcher_frame_resolves_configured_command_ids() {
        let config =
            RoostConfig::parse(r#"command = label="Echo Marker" run="printf marker" hold=true"#);
        let state = palette::PaletteState::new(launcher_palette_frame(&config));
        let item = state.selected_item().expect("configured launcher row");
        assert_eq!(item.id, "launch:0");
        assert_eq!(custom_command::launch_index(&item.id), Some(0));
    }

    #[test]
    fn provider_frame_uses_shared_provider_ids() {
        let config = RoostConfig::parse(r#"provider = label="Fixture" run="fixture.sh""#);
        let state = palette::PaletteState::new(provider_palette_frame(&config.providers));
        let item = state.selected_item().expect("provider row");
        assert_eq!(item.id, "provider:0");
        assert_eq!(provider::provider_index(&item.id), Some(0));
    }
}
