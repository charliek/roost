use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use iced::keyboard::{self, key::Named, Key};
use iced::widget::Id;
use iced::widget::{button, canvas, column, container, row, scrollable, stack, text, text_input};
use iced::{window, Color, Element, Fill, Size, Task};
use roost_engine::ipc::{
    ClipboardOp, DumpData, ExpandSelectionData, IpcHandler, ResolvedCellData, ResolvedCellsData,
    SelectionData, UiRequest,
};
use roost_engine::osc::{ClipboardTarget, OscAction, OscColorSnapshot, OscRouter};
use roost_engine::pointer::{MotionEmitter, PointerAction, PointerButton};
use roost_engine::session::{InputCapture, TabOutput, TabSession};
use roost_engine::single_instance::InstanceLock;
use roost_engine::{LocalClient, PtySupervisor, RestoreTab, Workspace};
use roost_ipc::messages::{PaletteItemView, PaletteStateResult, Project};
use roost_ipc::paths::BundleProfile;
use roost_ipc::IpcServer;
use roost_ui_model::theme::Theme;
use roost_ui_model::{config::RoostConfig, custom_command, palette};
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

fn command_palette_frame() -> palette::PaletteFrame {
    palette::PaletteFrame::new(
        "commands",
        "Execute a command…",
        palette::command_items(|_| None),
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
    palette_theme_at_open: Option<String>,
    palette_input_id: Id,
    palette_focus_requested: bool,
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
            pending_window_resize: None,
            window_size: Size::new(1100.0, 720.0),
            modifiers: keyboard::Modifiers::default(),
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            status: None,
            config,
            active_theme_name,
            palette: None,
            palette_theme_at_open: None,
            palette_input_id: Id::unique(),
            palette_focus_requested: false,
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
        let content: Element<'_, Message> =
            row![sidebar, column![tab_bar, terminal].width(Fill).height(Fill)]
                .width(Fill)
                .height(Fill)
                .into();
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
            if let Some(subtitle) = matched.item.subtitle {
                label = label.push(
                    text(subtitle)
                        .size(11)
                        .color(Color::from_rgb8(160, 164, 176)),
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
            "" | "commands" => command_palette_frame(),
            "launcher" => launcher_palette_frame(&self.config),
            "custom" | "agents" => {
                return Err(format!("palette kind {kind:?} is not implemented by Iced"));
            }
            _ => return Err(format!("unknown palette kind {kind:?}")),
        };
        self.dismiss_palette();
        self.palette_theme_at_open = Some(self.active_theme_name.clone());
        self.palette = Some(palette::PaletteState::new(frame));
        self.palette_focus_requested = true;
        Ok(())
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
            if !item.actionable {
                return Err(format!("palette row {id:?} is not actionable"));
            }
            (state.current().id.clone(), item)
        };

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
        self.workspace
            .focus_tab(tabs[next].id)
            .map_err(|error| error.to_string())?;
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
                UiRequest::PalettePresent { reply, .. } => {
                    let _ = reply.send(Err(UNSUPPORTED.into()));
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
                    let _ = reply.send(Err(UNSUPPORTED.into()));
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

    #[test]
    fn command_palette_uses_shared_ids_and_ranking() {
        let mut state = palette::PaletteState::new(command_palette_frame());
        assert!(state
            .matches()
            .iter()
            .any(|matched| matched.item.id == "new_tab"));
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
}
