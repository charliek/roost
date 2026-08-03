use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iced::keyboard::{self, key::Named, Key};
use iced::widget::Id;
use iced::widget::{
    button, column, container, mouse_area, row, scrollable, stack, text, text_input,
};
use iced::{font, window, Alignment, Color, Element, Fill, Font, Shrink, Size};
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
use roost_engine::{
    LocalClient, PtySupervisor, RestoreTab, Workspace, WorkspaceError, WorkspaceEvent,
};
use roost_ipc::agent;
use roost_ipc::messages::{
    PaletteItemView, PalettePresentResult, PaletteStateResult, Project, SidebarDumpAgentRow,
    SidebarDumpProject, SidebarDumpResult,
};
use roost_ipc::paths::BundleProfile;
use roost_ipc::IpcServer;
use roost_ui_model::theme::Theme;
use roost_ui_model::typography::{self, FamilyApply, TerminalTypography};
use roost_ui_model::{
    agent_palette,
    config::{self, RoostConfig},
    custom_command,
    keybind::{self, Accel, AccelMods, KeybindAction},
    notification_inbox, palette, provider,
    rollup::project_rollup,
};
use roost_url::HoverUrl;
use roost_vt::{
    key_action, mouse_action, mouse_button, KeyEncoder, KeyEvent, MouseEncoder, MouseEvent,
    RenderState, ScrollDirection, ScrollRoute, Terminal, TerminalOptions, TerminalScroll,
    TerminalSelection,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::font_registry::{system_font_registry, FontRegistry};
use crate::palette_scroll::Visibility;
use crate::tab_reorder::{TabStrip, TabStripEvent};
use crate::terminal_widget::{
    resolve_colors, DrawCell, TerminalMetrics, TerminalPointerEvent, TerminalSnapshot,
    TerminalWheelEvent, TerminalWidget, TERMINAL_PADDING,
};
use crate::Message;
use crate::{chrome, input};

// `mod palette` would collide with the `roost_ui_model::palette` import in
// this module's namespace, so the palette-overlay half of App lives in
// `palettes` (it hosts the command/agent/provider/notification palettes).
mod interactions;
mod palettes;
mod servicing;
mod terminal_tab;

use self::interactions::{
    arm_rename_completion_for_open_editor, consume_rename_completion_key,
    enqueue_osc_clipboard_write, native_file_drop_origin, paste_bytes, same_stable_ids,
    ClipboardQueue, FileDropQueue, RenameCompletionKey, RenameEditor, RenameTarget,
    ScreenshotQueue, TabDragPreview,
};
use self::palettes::{
    apply_with_rollback, ellipsize_palette_text, palette_agent_left_text, palette_row_id,
    palette_title_runs, FontSizeTransition, PaletteVisibilityRequest, ProviderRunResult,
    PALETTE_AGENT_PROJECT_MAX_COLUMNS,
};
use self::servicing::AgentMetricsResult;
use self::terminal_tab::{
    apply_geometry_batch, pointer_origin_tab, terminal_grid, GeometryBatchOperation,
    NativePointerDispatch, TerminalTab,
};
#[cfg(test)]
use self::terminal_tab::{
    GeometryBatchFailure, GeometryChange, LocalPointerGesture, NativePointerOutcome,
    TerminalGeometry,
};

const SIDEBAR_WIDTH: f32 = 220.0;
const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 32;
const STATUS_BANNER_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct StatusBanner {
    message: Option<String>,
    expires_at: Option<Instant>,
}

impl StatusBanner {
    fn set_at(&mut self, message: impl Into<String>, now: Instant) {
        self.message = Some(message.into());
        self.expires_at = Some(now + STATUS_BANNER_DURATION);
    }

    fn clear(&mut self) {
        self.message = None;
        self.expires_at = None;
    }

    fn expire_at(&mut self, now: Instant) {
        if self.expires_at.is_some_and(|deadline| now >= deadline) {
            self.clear();
        }
    }

    fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseTabOutcome {
    Closed,
    AlreadyGone,
}

fn close_tab_by_id(
    runtime: &tokio::runtime::Runtime,
    client: &LocalClient,
    tab_id: i64,
) -> Result<CloseTabOutcome> {
    match runtime.block_on(client.close_tab(tab_id)) {
        Ok(()) => Ok(CloseTabOutcome::Closed),
        Err(error)
            if matches!(
                error.downcast_ref::<WorkspaceError>(),
                Some(WorkspaceError::TabNotFound(id)) if *id == tab_id
            ) =>
        {
            Ok(CloseTabOutcome::AlreadyGone)
        }
        Err(error) => Err(error),
    }
}

fn create_project_flow(
    runtime: &tokio::runtime::Runtime,
    client: &LocalClient,
) -> Result<(i64, i64), String> {
    let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let project = runtime
        .block_on(client.create_project("", &cwd))
        .map_err(|error| error.to_string())?;
    let tab = runtime
        .block_on(client.open_tab(
            project.id,
            &cwd,
            "",
            &[],
            u32::from(DEFAULT_COLS),
            u32::from(DEFAULT_ROWS),
        ))
        .map_err(|error| error.to_string())?;
    // `open_tab` already steals the selection, but create's activation must
    // not depend on another op's side effect.
    client
        .workspace
        .focus_tab(tab.id)
        .map_err(|error| error.to_string())?;
    Ok((project.id, tab.id))
}

fn sidebar_width(collapsed: bool) -> f32 {
    if collapsed {
        0.0
    } else {
        SIDEBAR_WIDTH
    }
}

fn clamped_tab_index(current: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 || current >= len {
        return None;
    }
    Some((current as isize + delta).clamp(0, len as isize - 1) as usize)
}

fn project_id_at_index(projects: &[Project], index: u8) -> Option<i64> {
    index
        .checked_sub(1)
        .and_then(|index| projects.get(usize::from(index)))
        .map(|project| project.id)
}

fn active_project_tab_at_index(projects: &[Project], project_id: i64, index: u8) -> Option<i64> {
    index.checked_sub(1).and_then(|index| {
        projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.tabs.get(usize::from(index)))
            .map(|tab| tab.id)
    })
}

fn dispatch_keybind_once_unless_repeat<T>(repeat: bool, dispatch: impl FnOnce() -> T) -> Option<T> {
    (!repeat).then(dispatch)
}

fn accel_label(accel: &Accel) -> Option<String> {
    if accel.key.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    let mut label = String::new();
    #[cfg(not(target_os = "macos"))]
    let mut parts = Vec::new();

    #[cfg(target_os = "macos")]
    {
        if accel.modifiers.contains(AccelMods::CTRL) {
            label.push('⌃');
        }
        if accel.modifiers.contains(AccelMods::ALT) {
            label.push('⌥');
        }
        if accel.modifiers.contains(AccelMods::SHIFT) {
            label.push('⇧');
        }
        if accel.modifiers.contains(AccelMods::SUPER) {
            label.push('⌘');
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if accel.modifiers.contains(AccelMods::CTRL) {
            parts.push("Ctrl".to_string());
        }
        if accel.modifiers.contains(AccelMods::ALT) {
            parts.push("Alt".to_string());
        }
        if accel.modifiers.contains(AccelMods::SHIFT) {
            parts.push("Shift".to_string());
        }
        if accel.modifiers.contains(AccelMods::SUPER) {
            parts.push("Super".to_string());
        }
    }

    let key = match accel.key.as_str() {
        "bracketleft" => "[".to_string(),
        "bracketright" => "]".to_string(),
        "braceleft" => "{".to_string(),
        "braceright" => "}".to_string(),
        "equal" => "=".to_string(),
        "minus" => "-".to_string(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => key.to_string(),
    };
    #[cfg(target_os = "macos")]
    {
        label.push_str(&key);
        Some(label)
    }
    #[cfg(not(target_os = "macos"))]
    {
        parts.push(key);
        Some(parts.join("+"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyboardRoute {
    None,
    Editor,
    Palette,
    Terminal(i64),
}

fn resolve_keyboard_route(
    editor_open: bool,
    palette_open: bool,
    active_tab: i64,
    active_terminal_live: bool,
) -> KeyboardRoute {
    if editor_open {
        KeyboardRoute::Editor
    } else if palette_open {
        KeyboardRoute::Palette
    } else if active_terminal_live {
        KeyboardRoute::Terminal(active_tab)
    } else {
        KeyboardRoute::None
    }
}

pub enum UiTask {
    None,
    Then(Box<UiTask>, Box<UiTask>),
    Focus(window::Id),
    FocusWidget(Id),
    SelectAllWidget(Id),
    Resize(window::Id, Size),
    Screenshot(window::Id),
    ClipboardRead {
        request_id: u64,
        target: ClipboardOp,
    },
    ClipboardWrite {
        request_id: u64,
        target: ClipboardOp,
        text: String,
    },
    OpenUrl {
        url: String,
    },
    PaletteVisibility {
        scroll_id: Id,
        row_id: Id,
        session: u64,
        revision: u64,
        measurement_generation: u64,
        reveal: bool,
    },
}

impl UiTask {
    fn then(self, next: Self) -> Self {
        match (self, next) {
            (Self::None, next) => next,
            (task, Self::None) => task,
            (task, next) => Self::Then(Box::new(task), Box::new(next)),
        }
    }
}

struct WindowOpenResult {
    task: UiTask,
    retained_resize_scheduled: bool,
}

fn prepare_window_opened(
    window_id: &mut Option<window::Id>,
    pending_window_resize: &mut Option<Size>,
    screenshots: &mut ScreenshotQueue,
    id: window::Id,
) -> WindowOpenResult {
    *window_id = Some(id);
    let resize = pending_window_resize
        .take()
        .map(|size| UiTask::Resize(id, size));
    let retained_resize_scheduled = resize.is_some();
    let task = resize
        .unwrap_or(UiTask::None)
        .then(screenshots.start_next(*window_id));
    WindowOpenResult {
        task,
        retained_resize_scheduled,
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
    screenshots: ScreenshotQueue,
    window_size: Size,
    modifiers: keyboard::Modifiers,
    test_mode: bool,
    status: StatusBanner,
    rename_editor: Option<RenameEditor>,
    rename_input_id: Id,
    rename_focus_requested: bool,
    rename_completion_key: Option<RenameCompletionKey>,
    tab_drag_preview: Option<TabDragPreview>,
    tab_strip_generation: u64,
    file_drops: FileDropQueue,
    config: RoostConfig,
    typography: TerminalTypography,
    font_registry: &'static FontRegistry,
    terminal_metrics: TerminalMetrics,
    metric_generation: u64,
    keybindings: HashMap<Accel, KeybindAction>,
    active_theme_name: String,
    palette: Option<palette::PaletteState>,
    palette_session: u64,
    palette_theme_at_open: Option<String>,
    palette_family_at_open: Option<Option<String>>,
    palette_resolved_family_at_open: Option<String>,
    palette_input_id: Id,
    palette_scroll_id: Id,
    palette_focus_requested: bool,
    palette_layout_revision: u64,
    palette_measurement_generation: u64,
    palette_reveal_required: bool,
    palette_reveal_attempts: u8,
    palette_selected_in_view: Option<bool>,
    palette_visibility_request: PaletteVisibilityRequest,
    palette_visibility_retries: u8,
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
    clipboard: ClipboardQueue,
    // Field order is intentional: terminal sessions and IPC receivers are
    // dropped before the runtime; the lock is held until every runtime task
    // has been cancelled and joined by Runtime::drop.
    runtime: tokio::runtime::Runtime,
    _lock: InstanceLock,
}

impl App {
    pub fn bootstrap(profile: &BundleProfile, lock: InstanceLock) -> Result<Self> {
        let config = RoostConfig::load_default();
        let font_registry = system_font_registry();
        let configured_typography =
            TerminalTypography::new(config.font_family.clone(), config.font_size);
        let configured_font = font_registry.resolve(configured_typography.effective_family());
        let (typography, terminal_metrics) = match TerminalMetrics::measure_with_font(
            configured_typography.current_size_pt(),
            configured_font.font,
        ) {
            Ok(metrics) => (configured_typography, metrics),
            Err(error) => {
                tracing::warn!(
                    configured_size = configured_typography.current_size_pt(),
                    %error,
                    "configured font size cannot be rendered by Iced; using Rust UI default"
                );
                let fallback = TerminalTypography::new(None, None);
                let fallback_font = font_registry.resolve(fallback.effective_family());
                let metrics = TerminalMetrics::measure_with_font(
                    fallback.current_size_pt(),
                    fallback_font.font,
                )
                .map_err(anyhow::Error::msg)
                .context("measure default Iced terminal font")?;
                (fallback, metrics)
            }
        };
        let keybindings = keybind::canonicalize_bindings(
            keybind::default_bindings(),
            config.keybinds.clone(),
            |warning| tracing::warn!("keybind: {warning}"),
        );
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
            screenshots: ScreenshotQueue::default(),
            window_size: Size::new(1100.0, 720.0),
            modifiers: keyboard::Modifiers::default(),
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            status: StatusBanner::default(),
            rename_editor: None,
            rename_input_id: Id::unique(),
            rename_focus_requested: false,
            rename_completion_key: None,
            tab_drag_preview: None,
            tab_strip_generation: 1,
            file_drops: FileDropQueue::default(),
            config,
            typography,
            font_registry,
            terminal_metrics,
            metric_generation: 1,
            keybindings,
            active_theme_name,
            palette: None,
            palette_session: 0,
            palette_theme_at_open: None,
            palette_family_at_open: None,
            palette_resolved_family_at_open: None,
            palette_input_id: Id::unique(),
            palette_scroll_id: Id::unique(),
            palette_focus_requested: false,
            palette_layout_revision: 0,
            palette_measurement_generation: 0,
            palette_reveal_required: false,
            palette_reveal_attempts: 0,
            palette_selected_in_view: None,
            palette_visibility_request: PaletteVisibilityRequest::None,
            palette_visibility_retries: 0,
            git_probe: Arc::new(git_metrics::GitProbe::new()),
            metrics_cache: git_metrics::MetricsCache::default(),
            metrics_tx,
            metrics_rx,
            provider_request: 0,
            provider_tx,
            provider_rx,
            provider_frames: HashMap::new(),
            palette_present_reply: None,
            clipboard: ClipboardQueue::default(),
            runtime,
            _lock: lock,
        };
        app.reconcile();
        app.resize(app.window_size);
        tracing::info!(socket = %profile.socket_path.display(), "Iced walking skeleton ready");
        Ok(app)
    }

    pub fn window_opened(&mut self, id: window::Id) -> UiTask {
        prepare_window_opened(
            &mut self.window_id,
            &mut self.pending_window_resize,
            &mut self.screenshots,
            id,
        )
        .task
    }

    pub fn window_resized(&mut self, id: window::Id, size: Size) -> UiTask {
        let opened = prepare_window_opened(
            &mut self.window_id,
            &mut self.pending_window_resize,
            &mut self.screenshots,
            id,
        );
        if !opened.retained_resize_scheduled {
            self.resize(size);
        }
        // A native resize event confirms that Iced has rebuilt the widget
        // viewport. The test IPC path may already have stored the same logical
        // size, so invalidate even when `resize` sees no numeric change.
        if self.palette.is_some() {
            self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        }
        opened.task
    }

    pub fn tick(&mut self) -> UiTask {
        let now = Instant::now();
        self.status.expire_at(now);
        let mut task = self.service_ui_requests();
        self.service_agent_metrics();
        self.service_provider_results();
        self.service_workspace_events();
        self.reconcile();
        if let Some(batch) = self.file_drops.take_ready_at(now) {
            self.deliver_file_drop(batch);
        }
        let mut exited = Vec::new();
        let mut osc_actions = Vec::new();
        let mut output_error = None;
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
                        output_error = Some(format!("tab {tab_id}: {error}"));
                        tracing::error!(tab_id, %error, "PTY output stream lost bytes");
                    }
                }
            }
            if let Err(error) = tab.refresh_snapshot() {
                tracing::warn!(tab_id, ?error, "terminal snapshot refresh failed");
            }
        }
        if let Some(error) = output_error {
            self.set_status(error);
        }
        for (tab_id, actions) in osc_actions {
            task = task.then(self.apply_osc_actions(tab_id, actions));
        }
        for tab_id in exited {
            let _ = self.workspace.close_tab(tab_id);
            self.tabs.remove(&tab_id);
        }
        task = task.then(self.take_rename_focus_task());
        task = task.then(self.take_palette_visibility_task());
        task = task.then(self.screenshots.start_next(self.window_id));
        task
    }

    pub fn file_dropped(&mut self, window_id: window::Id, path: PathBuf) {
        if self.window_id != Some(window_id) {
            tracing::debug!("ignored native file drop for an unowned window");
            return;
        }
        let new_origin = native_file_drop_origin(self.window_id, window_id, self.keyboard_route());
        let (ready, accepted) = self.file_drops.push_at(new_origin, path, Instant::now());
        if let Some(batch) = ready {
            self.deliver_file_drop(batch);
        }
        if !accepted {
            tracing::debug!("ignored native file drop without an active terminal input route");
        }
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status.set_at(message, Instant::now());
    }

    pub fn resize(&mut self, size: Size) {
        let changed = self.window_size != size;
        self.window_size = size;
        let (cols, rows) = terminal_grid(
            size,
            self.workspace.sidebar_collapsed(),
            self.terminal_metrics,
        );
        for (tab_id, tab) in &mut self.tabs {
            match tab.apply_geometry(cols, rows, self.terminal_metrics, self.metric_generation) {
                Ok(Some(change)) => tab.commit_geometry(change),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(?error, tab_id, cols, rows, "terminal resize failed")
                }
            }
        }
        if changed && self.palette.is_some() {
            self.invalidate_palette_geometry(PaletteVisibilityRequest::Reveal);
        }
    }

    pub fn keyboard(&mut self, event: keyboard::Event) -> UiTask {
        if consume_rename_completion_key(&mut self.rename_completion_key, &event) {
            return UiTask::None;
        }
        if let keyboard::Event::ModifiersChanged(modifiers) = &event {
            self.modifiers = *modifiers;
            let held = self.link_modifier_held();
            let tab_id = self.workspace.active().1;
            if let Some(tab) = self.tabs.get_mut(&tab_id) {
                if let Err(error) = tab
                    .set_link_modifier_held(held)
                    .and_then(|()| tab.refresh_snapshot())
                {
                    tracing::warn!(?error, tab_id, "terminal link hover refresh failed");
                }
            }
        }
        if matches!(self.keyboard_route(), KeyboardRoute::Editor) {
            if let keyboard::Event::KeyPressed {
                key: Key::Named(Named::Escape),
                repeat: false,
                ..
            } = &event
            {
                self.rename_completion_key = Some(RenameCompletionKey::Escape);
                self.cancel_rename_editor();
            }
            // TextInput owns printable input and on_submit owns Enter. The
            // global listener consumes every editor event so no accelerator or
            // terminal encoder can observe the same key.
            return UiTask::None;
        }
        if let keyboard::Event::KeyPressed { key, .. } = &event {
            if matches!(self.keyboard_route(), KeyboardRoute::Palette) {
                match key.as_ref() {
                    Key::Named(Named::Escape) => self.palette_back_or_dismiss(),
                    Key::Named(Named::ArrowUp) => self.move_palette_selection(-1),
                    Key::Named(Named::ArrowDown) => self.move_palette_selection(1),
                    Key::Named(Named::Enter) => self.palette_confirm(),
                    _ => {}
                }
                // The text input widget consumes printable events. Never let
                // a palette keystroke leak through to the active PTY.
                return self.take_palette_focus_task();
            }
        } else if matches!(self.keyboard_route(), KeyboardRoute::Palette) {
            return UiTask::None;
        }

        if let Some(action) = input::accelerator(&event)
            .and_then(|accelerator| self.keybindings.get(&accelerator).copied())
        {
            let repeat = matches!(&event, keyboard::Event::KeyPressed { repeat: true, .. });
            return self.dispatch_keybind_action(action, repeat);
        }

        let KeyboardRoute::Terminal(active_tab) = self.keyboard_route() else {
            return UiTask::None;
        };
        let Some(tab) = self.tabs.get_mut(&active_tab) else {
            return UiTask::None;
        };
        if input::should_snap_for_terminal_input(&event) {
            if let Err(error) = tab.snap_to_bottom_for_input() {
                tracing::warn!(?error, active_tab, "terminal snap-to-bottom failed");
            }
        }
        let bytes = input::encode_press(&mut tab.encoder, &tab.terminal, event);
        tab.session.send_input(bytes);
        UiTask::None
    }

    /// Handle the one captured text-input key that belongs to application
    /// surface state. Never route a captured Escape into the terminal if the
    /// editor or palette disappeared before this queued message is applied.
    pub fn captured_escape(&mut self) -> UiTask {
        match self.keyboard_route() {
            KeyboardRoute::Editor => {
                self.rename_completion_key = Some(RenameCompletionKey::Escape);
                self.cancel_rename_editor();
                UiTask::None
            }
            KeyboardRoute::Palette => {
                self.palette_back_or_dismiss();
                self.take_palette_focus_task()
            }
            KeyboardRoute::None | KeyboardRoute::Terminal(_) => UiTask::None,
        }
    }

    pub fn captured_enter_release(&mut self) {
        if self.rename_completion_key == Some(RenameCompletionKey::Enter) {
            self.rename_completion_key = None;
        }
    }

    fn dispatch_keybind_action(&mut self, action: KeybindAction, repeat: bool) -> UiTask {
        // Once an accelerator matches, it belongs to Roost and must never be
        // encoded into the PTY. Suppress repeats for all application actions:
        // this prevents held destructive shortcuts from cascading while still
        // consuming every repeated event.
        let Some(result) = dispatch_keybind_once_unless_repeat(repeat, || {
            self.status.clear();
            self.dispatch_keybind_action_once(action)
        }) else {
            return UiTask::None;
        };
        match result {
            Ok(task) => task,
            Err(error) => {
                self.set_status(error);
                UiTask::None
            }
        }
    }

    fn dispatch_keybind_action_once(&mut self, action: KeybindAction) -> Result<UiTask, String> {
        match action {
            KeybindAction::NewTab => {
                self.new_tab_result()?;
                Ok(UiTask::None)
            }
            KeybindAction::CloseTab => {
                let tab_id = self.workspace.active().1;
                if tab_id != 0 {
                    self.close_tab_result(tab_id)?;
                }
                Ok(UiTask::None)
            }
            KeybindAction::NewProject => {
                self.new_project_result()?;
                Ok(UiTask::None)
            }
            KeybindAction::RenameProject => {
                self.begin_rename_target(RenameTarget::Project(self.workspace.active().0))?;
                Ok(UiTask::None)
            }
            KeybindAction::RenameTab => {
                self.begin_rename_target(RenameTarget::Tab(self.workspace.active().1))?;
                Ok(UiTask::None)
            }
            KeybindAction::CloseProject => Err("Close Project is not available in Iced yet".into()),
            KeybindAction::JumpToUnread => {
                let active_project_id = self.workspace.active().0;
                if let Some(tab_id) =
                    notification_inbox::next_unread(&self.notification_inbox, active_project_id)
                {
                    self.focus_tab_and_clear(tab_id, true)?;
                }
                Ok(UiTask::None)
            }
            KeybindAction::CycleTabPrev => {
                self.cycle_tab(-1)?;
                Ok(UiTask::None)
            }
            KeybindAction::CycleTabNext => {
                self.cycle_tab(1)?;
                Ok(UiTask::None)
            }
            KeybindAction::Copy => Ok(self.copy_active_selection()),
            KeybindAction::Paste => Ok(self.paste_into_active(ClipboardOp::System)),
            KeybindAction::ToggleSidebar => {
                self.toggle_sidebar();
                Ok(UiTask::None)
            }
            KeybindAction::ToggleSidebarAgents => {
                self.toggle_sidebar_agents();
                Ok(UiTask::None)
            }
            KeybindAction::FontIncrease => {
                self.apply_font_size_transition(FontSizeTransition::Adjust(1.0))?;
                Ok(UiTask::None)
            }
            KeybindAction::FontDecrease => {
                self.apply_font_size_transition(FontSizeTransition::Adjust(-1.0))?;
                Ok(UiTask::None)
            }
            KeybindAction::FontReset => {
                self.apply_font_size_transition(FontSizeTransition::Reset)?;
                Ok(UiTask::None)
            }
            KeybindAction::CommandPalette => self.open_bound_palette_result("commands"),
            KeybindAction::CommandLauncher => self.open_bound_palette_result("launcher"),
            KeybindAction::CustomPalette => self.open_bound_palette_result("custom"),
            KeybindAction::AgentPalette => self.open_bound_palette_result("agents"),
            KeybindAction::Unbind => Ok(UiTask::None),
            KeybindAction::SwitchProject(index) => {
                self.switch_project_by_index(index)?;
                Ok(UiTask::None)
            }
            KeybindAction::SwitchTab(index) => {
                self.switch_tab_by_index(index)?;
                Ok(UiTask::None)
            }
        }
    }

    fn keyboard_route(&self) -> KeyboardRoute {
        let active_tab = self.workspace.active().1;
        resolve_keyboard_route(
            self.rename_editor.is_some(),
            self.palette.is_some(),
            active_tab,
            self.tabs.contains_key(&active_tab),
        )
    }

    pub fn set_window_focus(&mut self, focused: bool) {
        if !focused {
            self.rename_completion_key = None;
            self.cancel_tab_drag();
        }
        self.workspace.set_window_focused(focused);
        if let Some(tab) = self.tabs.get(&self.workspace.active().1) {
            tab.set_window_focus(focused);
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let (active_project, active_tab) = self.workspace.active();
        let mut sidebar_body = column![].spacing(2).padding([4, 0]);
        for project in &self.projects {
            let rollup = project_rollup(
                project
                    .tabs
                    .iter()
                    .map(|tab| agent::effective_lifecycle(&tab.agent_state())),
            );
            let stripe = container(
                iced::widget::Space::new()
                    .width(chrome::PROJECT_STRIPE_WIDTH)
                    .height(chrome::ROW_HEIGHT),
            )
            .style(move |_| {
                let color = if rollup == roost_ipc::agent::AgentLifecycle::Inactive {
                    Color::TRANSPARENT
                } else {
                    agent_color(rollup)
                };
                iced::widget::container::Style::default()
                    .background(color)
                    .border(iced::border::rounded(2))
            });
            let project_label: Element<'_, Message> = match self.rename_editor.as_ref() {
                Some(editor) if editor.target == RenameTarget::Project(project.id) => {
                    text_input("Project name", &editor.draft)
                        .id(self.rename_input_id.clone())
                        .on_input(Message::RenameDraftChanged)
                        .on_submit(Message::RenameSubmit)
                        .size(13)
                        .padding([1, 3])
                        .style(chrome::inline_rename_input)
                        .into()
                }
                _ => text(&project.name).size(13).into(),
            };
            let project_pill = container(project_label)
                .width(Fill)
                .height(chrome::ROW_HEIGHT)
                .padding([3.0, chrome::PROJECT_LABEL_INSET])
                .style(chrome::project_pill(project.id == active_project));
            let project_row = mouse_area(
                container(
                    row![
                        stripe,
                        iced::widget::Space::new().width(chrome::PROJECT_STRIPE_GAP),
                        project_pill,
                        iced::widget::Space::new().width(chrome::PROJECT_RIGHT_INSET)
                    ]
                    .align_y(Alignment::Center),
                )
                .width(Fill)
                .height(chrome::ROW_HEIGHT),
            )
            .on_press(Message::ProjectSelected(project.id))
            .on_double_click(Message::BeginRenameProject(project.id));
            let mut project_group = column![project_row].spacing(2);
            if self.config.show_sidebar_agents {
                for agent in self.sidebar_agents.get(&project.id).into_iter().flatten() {
                    let name = agent.name.clone();
                    let detail = format!("{} · {}", agent.status_text, agent.time_text);
                    let dot_color = agent_color(agent.lifecycle);
                    let dot =
                        container(iced::widget::Space::new().width(8).height(8)).style(move |_| {
                            iced::widget::container::Style::default()
                                .background(dot_color)
                                .border(iced::border::rounded(3))
                        });
                    let agent_row = row![
                        dot,
                        text(name).size(11),
                        text(detail).size(9).color(chrome::MUTED_TEXT)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center);
                    project_group = project_group.push(
                        button(agent_row)
                            .width(Fill)
                            .height(chrome::ROW_HEIGHT)
                            .padding(iced::Padding {
                                top: 3.0,
                                right: 8.0,
                                bottom: 3.0,
                                left: chrome::AGENT_DOT_INSET,
                            })
                            .style(chrome::agent_button(agent.is_active))
                            .on_press(Message::AgentSelected(agent.tab_id)),
                    );
                }
            }
            sidebar_body = sidebar_body.push(project_group);
        }
        let sidebar_header = container(
            row![
                text("PROJECTS").size(11).color(chrome::MUTED_TEXT),
                iced::widget::Space::new().width(Fill),
                button(text("«").size(11))
                    .width(chrome::PILL_HEIGHT)
                    .height(chrome::PILL_HEIGHT)
                    .padding(2)
                    .style(chrome::transparent_button)
                    .on_press(Message::ToggleSidebar)
            ]
            .align_y(Alignment::Center),
        )
        .height(chrome::BAND_HEIGHT)
        .width(Fill)
        .padding([5, 12])
        .style(chrome::surface);
        let sidebar_footer = container(
            button(text("+ New Project").size(11))
                .width(Fill)
                .height(chrome::PILL_HEIGHT)
                .padding([2, 8])
                .style(chrome::transparent_button)
                .on_press(Message::NewProject),
        )
        .height(chrome::BAND_HEIGHT)
        .width(Fill)
        .padding([5, 8])
        .style(chrome::surface);
        let sidebar = container(column![
            sidebar_header,
            scrollable(sidebar_body).height(Fill),
            sidebar_footer
        ])
        .width(SIDEBAR_WIDTH)
        .height(Fill)
        .style(chrome::surface);

        let active_project_model = self
            .projects
            .iter()
            .find(|project| project.id == active_project);
        let authoritative_tab_ids = active_project_model
            .map(|project| project.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>())
            .unwrap_or_default();
        let visual_tab_ids = self
            .tab_drag_preview
            .as_ref()
            .filter(|preview| {
                preview.context.project_id == active_project
                    && preview.original_ids == authoritative_tab_ids
                    && same_stable_ids(&preview.ordered_ids, &authoritative_tab_ids)
            })
            .map(|preview| preview.ordered_ids.clone())
            .unwrap_or_else(|| authoritative_tab_ids.clone());
        let active_project_tabs = visual_tab_ids
            .iter()
            .filter_map(|tab_id| {
                active_project_model
                    .and_then(|project| project.tabs.iter().find(|tab| tab.id == *tab_id))
            })
            .collect::<Vec<_>>();
        let collapsed = self.workspace.sidebar_collapsed();
        let mut tab_pills = row![].spacing(6);
        for tab in active_project_tabs {
            let title = if tab.title.is_empty() {
                "shell"
            } else {
                &tab.title
            };
            let active = tab.id == active_tab;
            let lifecycle = agent::effective_lifecycle(&tab.agent_state());
            let status_color = tab_status_color(lifecycle);
            let dot = container(
                iced::widget::Space::new()
                    .width(chrome::TAB_STATUS_SIZE)
                    .height(chrome::TAB_STATUS_SIZE),
            )
            .style(move |_| {
                iced::widget::container::Style::default()
                    .background(status_color)
                    .border(iced::border::rounded(4))
            });
            let select: Element<'_, Message> = if let Some(editor) = self
                .rename_editor
                .as_ref()
                .filter(|editor| editor.target == RenameTarget::Tab(tab.id))
            {
                container(
                    row![
                        dot,
                        text_input("Tab name", &editor.draft)
                            .id(self.rename_input_id.clone())
                            .on_input(Message::RenameDraftChanged)
                            .on_submit(Message::RenameSubmit)
                            .width(140)
                            .size(12)
                            .padding([1, 3])
                            .style(chrome::inline_rename_input)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .height(chrome::PILL_HEIGHT)
                .padding([2, 7])
                .into()
            } else {
                container(
                    row![
                        dot,
                        text(title).size(12).color(if active {
                            chrome::TEXT
                        } else {
                            chrome::MUTED_TEXT
                        })
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .height(chrome::PILL_HEIGHT)
                .padding([2, 7])
                .into()
            };
            let mut pill = row![select].align_y(Alignment::Center);
            if tab.has_notification && !active {
                pill = pill.push(
                    container(
                        iced::widget::Space::new()
                            .width(chrome::NOTIFICATION_DOT_SIZE)
                            .height(chrome::NOTIFICATION_DOT_SIZE),
                    )
                    .style(chrome::badge),
                );
            }
            if active {
                pill = pill.push(
                    button(text("×").size(13))
                        .width(chrome::PILL_HEIGHT)
                        .height(chrome::PILL_HEIGHT)
                        .padding(2)
                        .style(chrome::transparent_button)
                        .on_press(Message::CloseTab(tab.id)),
                );
            }
            tab_pills = tab_pills.push(
                container(pill)
                    .height(chrome::PILL_HEIGHT)
                    .padding([0, 2])
                    .style(chrome::tab_pill(
                        active,
                        self.tab_drag_preview
                            .as_ref()
                            .is_some_and(|preview| preview.context.source_id == tab.id),
                    )),
            );
        }
        let tab_strip = TabStrip::new(
            tab_pills,
            active_project,
            visual_tab_ids,
            self.tab_strip_generation,
            self.rename_editor.is_none(),
        );
        let tab_scroller = scrollable(tab_strip)
            .horizontal()
            .width(Fill)
            .height(chrome::PILL_HEIGHT);
        let mut tabs = row![].spacing(5).align_y(Alignment::Center);
        if collapsed {
            tabs = tabs.push(
                button(text("☰").size(13))
                    .width(chrome::PILL_HEIGHT)
                    .height(chrome::PILL_HEIGHT)
                    .padding(2)
                    .style(chrome::transparent_button)
                    .on_press(Message::ToggleSidebar),
            );
        }
        tabs = tabs.push(tab_scroller).push(
            button(text("+").size(15))
                .width(chrome::PILL_HEIGHT)
                .height(chrome::PILL_HEIGHT)
                .padding(1)
                .style(chrome::transparent_button)
                .on_press(Message::NewTab),
        );
        let notification_count = self.notification_inbox.count();
        let notification_label = if notification_count == 0 {
            "○".to_string()
        } else {
            format!("•{}", notification_count.min(99))
        };
        tabs = tabs.push(
            button(
                text(notification_label)
                    .size(11)
                    .color(if notification_count == 0 {
                        chrome::MUTED_TEXT
                    } else {
                        chrome::NOTIFICATION
                    }),
            )
            .width(chrome::PILL_HEIGHT)
            .height(chrome::PILL_HEIGHT)
            .padding(2)
            .style(chrome::transparent_button)
            .on_press(Message::OpenNotifications),
        );
        let tab_bar = container(tabs)
            .height(chrome::BAND_HEIGHT)
            .width(Fill)
            .padding([5, 8])
            .style(chrome::dark_surface);

        let terminal: Element<'_, Message> = match self.tabs.get(&active_tab) {
            Some(tab) if tab.applied_metrics.is_some() => TerminalWidget {
                tab_id: active_tab,
                snapshot: tab.snapshot.clone(),
                metrics: tab.applied_metrics.unwrap_or(self.terminal_metrics),
                metric_generation: tab.metric_generation,
            }
            .into(),
            _ => container(text("Starting terminal…"))
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
        let content: Element<'_, Message> = if let Some(status) = self.status.message() {
            let toast = container(text(status).size(12).color(chrome::ERROR_TEXT))
                .max_width(520)
                .padding([8, 12])
                .style(chrome::status_toast);
            let overlay = container(toast)
                .width(Fill)
                .height(Fill)
                .padding(16)
                .align_x(Alignment::End)
                .align_y(Alignment::End);
            stack![content, overlay].width(Fill).height(Fill).into()
        } else {
            content
        };
        let Some(palette) = &self.palette else {
            return if self.rename_editor.is_some() {
                mouse_area(content)
                    .on_press(Message::RenamePointerDismiss)
                    .into()
            } else {
                content
            };
        };

        let frame = palette.current();
        let input = text_input(&frame.placeholder, &frame.query)
            .id(self.palette_input_id.clone())
            .on_input(Message::PaletteQueryChanged)
            .on_submit(Message::PaletteConfirm)
            .padding([4, 8])
            .size(17)
            .style(chrome::palette_input);
        let mut items = column![].spacing(0);
        for (index, matched) in palette.matches().into_iter().enumerate() {
            let selected = index == frame.selection;
            let item = matched.item;
            let actionable = item.actionable;
            let primary_color = if actionable {
                chrome::TEXT
            } else {
                chrome::PALETTE_PLACEHOLDER.scale_alpha(0.6)
            };
            let label: Element<'_, Message> = if let Some(agent) = item.agent {
                let lifecycle_color = if actionable {
                    agent_color(agent.effective_lifecycle)
                } else {
                    chrome::PALETTE_PLACEHOLDER.scale_alpha(0.6)
                };
                let dot =
                    container(iced::widget::Space::new().width(8).height(8)).style(move |_| {
                        iced::widget::container::Style::default()
                            .background(lifecycle_color)
                            .border(iced::border::rounded(4))
                    });
                let metrics = agent.metrics_text.unwrap_or_default();
                let project =
                    ellipsize_palette_text(&agent.project, PALETTE_AGENT_PROJECT_MAX_COLUMNS);
                let (name, status) = palette_agent_left_text(&agent.name, &agent.status_text);
                row![
                    dot,
                    container(
                        text(project)
                            .size(14)
                            .font(Font {
                                weight: font::Weight::Bold,
                                ..Font::default()
                            })
                            .color(primary_color)
                            .wrapping(iced::widget::text::Wrapping::None)
                    )
                    .max_width(140),
                    container(
                        text(name)
                            .size(14)
                            .color(primary_color)
                            .wrapping(iced::widget::text::Wrapping::None)
                    ),
                    text(status)
                        .size(13)
                        .color(lifecycle_color)
                        .wrapping(iced::widget::text::Wrapping::None),
                    iced::widget::Space::new().width(Fill),
                    text(metrics)
                        .size(12)
                        .font(Font::MONOSPACE)
                        .color(chrome::MUTED_TEXT),
                    text(agent.time_text)
                        .size(12)
                        .font(Font::MONOSPACE)
                        .color(chrome::MUTED_TEXT)
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into()
            } else {
                let spans = palette_title_runs(&item.title, &matched.ranges)
                    .into_iter()
                    .map(|run| {
                        let mut span = iced::widget::span::<(), Font>(run.text).color(
                            if run.matched && actionable {
                                chrome::PALETTE_MATCH
                            } else {
                                primary_color
                            },
                        );
                        if run.matched && actionable {
                            span = span.font(Font {
                                weight: font::Weight::Semibold,
                                ..Font::default()
                            });
                        }
                        span
                    })
                    .collect::<Vec<_>>();
                let title = iced::widget::rich_text(spans)
                    .size(14)
                    .width(Fill)
                    .wrapping(iced::widget::text::Wrapping::None);
                let mut leading = column![title];
                if let Some(subtitle) = item.subtitle {
                    leading = leading.push(
                        text(subtitle)
                            .size(12)
                            .color(chrome::PALETTE_PLACEHOLDER)
                            .wrapping(iced::widget::text::Wrapping::None),
                    );
                }
                let mut generic = row![leading.width(Fill)].align_y(Alignment::Center);
                if let Some(trailing) = item.trailing_text {
                    generic = generic.push(
                        text(trailing)
                            .size(12)
                            .color(chrome::PALETTE_PLACEHOLDER)
                            .wrapping(iced::widget::text::Wrapping::None),
                    );
                }
                generic.into()
            };
            let row = button(label)
                .width(Fill)
                .padding([6, 10])
                .style(chrome::palette_row(selected, actionable));
            let row = if actionable {
                row.on_press(Message::PaletteActivate(item.id))
            } else {
                row
            };
            items = items.push(container(row).id(palette_row_id(
                self.palette_session,
                self.palette_layout_revision,
                index,
            )));
        }
        let list = scrollable(items)
            .id(self.palette_scroll_id.clone())
            .on_scroll(|_| Message::PaletteScrolled)
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new()
                    .width(2)
                    .scroller_width(4)
                    .margin(2),
            ))
            .style(chrome::palette_scrollable)
            .height(Shrink);
        let divider = container(
            container(iced::widget::Space::new().height(1))
                .width(Fill)
                .style(chrome::palette_divider),
        )
        .padding([8, 2]);
        let panel = container(column![input, divider, list].spacing(0))
            .width(Fill)
            .max_width(chrome::PALETTE_WIDTH)
            .height(Shrink)
            .max_height(chrome::PALETTE_MAX_HEIGHT)
            .padding(10)
            .style(chrome::palette_panel);
        let overlay = container(mouse_area(panel).on_press(Message::PaletteCardPressed))
            .width(Fill)
            .height(Fill)
            .padding([60, 16])
            .center_x(Fill);
        let catcher = mouse_area(iced::widget::Space::new().width(Fill).height(Fill))
            .on_press(Message::PaletteDismiss);
        stack![content, catcher, overlay]
            .width(Fill)
            .height(Fill)
            .into()
    }

    pub fn select_project(&mut self, project_id: i64) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Some(tab_id) = self.workspace.preferred_tab(project_id) {
            let _ = self.focus_tab_and_clear(tab_id, false);
        }
    }

    pub fn select_tab(&mut self, tab_id: i64) {
        self.cancel_editor_for_interaction();
        let _ = self.focus_tab_and_clear(tab_id, false);
    }

    pub fn select_agent(&mut self, tab_id: i64) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        let _ = self.focus_tab_and_clear(tab_id, true);
    }

    pub fn open_notifications(&mut self) -> UiTask {
        if let Err(error) = self.open_palette("notifications") {
            self.set_status(error);
        }
        self.take_palette_focus_task()
    }

    pub fn toggle_sidebar(&mut self) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
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
                self.set_status(format!("persist show-sidebar-agents: {error}"));
            }
        }
    }

    pub fn new_tab(&mut self) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Err(error) = self.new_tab_result() {
            self.set_status(error);
        }
    }

    fn new_tab_result(&mut self) -> Result<(), String> {
        let (project_id, _) = self.workspace.active();
        if project_id == 0 {
            return Ok(());
        }
        let cwd = self.launch_cwd(project_id);
        self.runtime
            .block_on(self.client.open_tab(
                project_id,
                &cwd,
                "",
                &[],
                u32::from(DEFAULT_COLS),
                u32::from(DEFAULT_ROWS),
            ))
            .map_err(|error| error.to_string())?;
        self.reconcile();
        Ok(())
    }

    pub fn new_project(&mut self) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Err(error) = self.new_project_result() {
            self.set_status(error);
        }
    }

    fn new_project_result(&mut self) -> Result<(), String> {
        self.set_sidebar_collapsed(false);
        create_project_flow(&self.runtime, &self.client)?;
        self.reconcile();
        Ok(())
    }

    pub fn close_tab(&mut self, tab_id: i64) {
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        if let Err(error) = self.close_tab_result(tab_id) {
            self.set_status(error);
        }
    }

    fn close_tab_result(&mut self, tab_id: i64) -> Result<(), String> {
        match close_tab_by_id(&self.runtime, &self.client, tab_id) {
            Ok(CloseTabOutcome::Closed) => {}
            Ok(CloseTabOutcome::AlreadyGone) => {
                tracing::debug!(tab_id, "close_tab: rendered tab already gone");
            }
            Err(error) => {
                tracing::warn!(?error, tab_id, "close_tab failed");
                return Err(error.to_string());
            }
        }
        self.reconcile();
        Ok(())
    }

    fn cycle_tab(&mut self, delta: isize) -> Result<(), String> {
        let (project_id, active_tab) = self.workspace.active();
        let projects = self.workspace.snapshot();
        let tabs = projects
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
        let Some(next) = clamped_tab_index(current, tabs.len(), delta) else {
            return Ok(());
        };
        if next == current {
            return Ok(());
        }
        self.focus_tab_and_clear(tabs[next].id, false)?;
        Ok(())
    }

    fn switch_project_by_index(&mut self, index: u8) -> Result<(), String> {
        let projects = self.workspace.snapshot();
        let Some(project_id) = project_id_at_index(&projects, index) else {
            return Ok(());
        };
        let Some(tab_id) = self.workspace.preferred_tab(project_id) else {
            return Ok(());
        };
        self.focus_tab_and_clear(tab_id, false)
    }

    fn switch_tab_by_index(&mut self, index: u8) -> Result<(), String> {
        let project_id = self.workspace.active().0;
        let projects = self.workspace.snapshot();
        let Some(tab_id) = active_project_tab_at_index(&projects, project_id, index) else {
            return Ok(());
        };
        self.focus_tab_and_clear(tab_id, false)
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
}

fn canonical_pointer_shape(name: &str) -> &str {
    match name {
        "default" | "pointer" | "text" | "crosshair" | "grab" | "grabbing" | "not-allowed"
        | "col-resize" | "row-resize" | "n-resize" | "s-resize" | "e-resize" | "w-resize"
        | "ne-resize" | "nw-resize" | "se-resize" | "sw-resize" | "wait" | "progress" | "help"
        | "move" => name,
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

fn tab_status_color(lifecycle: roost_ipc::agent::AgentLifecycle) -> Color {
    if lifecycle == roost_ipc::agent::AgentLifecycle::Inactive {
        Color::TRANSPARENT
    } else {
        agent_color(lifecycle)
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
    pub(crate) fn apply(self, app: &mut App) -> UiTask {
        match self {
            Self::ProjectSelected(project_id) => app.select_project(project_id),
            Self::BeginRenameProject(project_id) => app.begin_rename_project(project_id),
            Self::AgentSelected(tab_id) => app.select_agent(tab_id),
            Self::TabSelected(tab_id) => app.select_tab(tab_id),
            Self::BeginRenameTab(tab_id) => app.begin_rename_tab(tab_id),
            Self::CloseTab(tab_id) => app.close_tab(tab_id),
            Self::NewTab => app.new_tab(),
            Self::NewProject => app.new_project(),
            Self::ToggleSidebar => app.toggle_sidebar(),
            Self::OpenNotifications => return app.open_notifications(),
            _ => {}
        }
        UiTask::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_geometry_never_produces_zero_grid() {
        let size = Size::new(1.0, 1.0);
        let metrics = TerminalMetrics::measure(13.0).expect("test metrics");
        assert_eq!(terminal_grid(size, false, metrics), (2, 2));
    }

    #[test]
    fn collapsed_sidebar_has_no_layout_width() {
        assert_eq!(sidebar_width(false), SIDEBAR_WIDTH);
        assert_eq!(sidebar_width(true), 0.0);
    }

    #[test]
    fn status_banner_replaces_clears_and_expires_deterministically() {
        let now = Instant::now();
        let mut status = StatusBanner::default();
        status.set_at("first", now);
        assert_eq!(status.message(), Some("first"));
        status.set_at("replacement", now + Duration::from_secs(1));
        assert_eq!(status.message(), Some("replacement"));
        status.expire_at(now + Duration::from_secs(1) + STATUS_BANNER_DURATION);
        assert_eq!(status.message(), None);

        status.set_at("clear me", now);
        status.clear();
        assert_eq!(status.message(), None);
    }

    #[test]
    fn tab_cycle_clamps_at_both_ends_instead_of_wrapping() {
        assert_eq!(clamped_tab_index(0, 3, -1), Some(0));
        assert_eq!(clamped_tab_index(0, 3, 1), Some(1));
        assert_eq!(clamped_tab_index(2, 3, 1), Some(2));
        assert_eq!(clamped_tab_index(2, 3, -1), Some(1));
        assert_eq!(clamped_tab_index(0, 0, 1), None);
        assert_eq!(clamped_tab_index(3, 3, -1), None);
    }

    #[test]
    fn repeated_keybinds_are_consumed_without_dispatching_the_action() {
        let mut calls = 0;
        let repeated = dispatch_keybind_once_unless_repeat(true, || {
            calls += 1;
            KeybindAction::CloseProject
        });
        assert_eq!(repeated, None);
        assert_eq!(calls, 0);

        let pressed = dispatch_keybind_once_unless_repeat(false, || {
            calls += 1;
            KeybindAction::CloseProject
        });
        assert_eq!(pressed, Some(KeybindAction::CloseProject));
        assert_eq!(calls, 1);
    }

    #[test]
    fn numeric_switch_helpers_follow_authoritative_snapshot_order() {
        let workspace = Workspace::new();
        let first = workspace.create_project("first", "/tmp").unwrap();
        let first_tab = workspace.open_tab(first.id, "/tmp", "one").unwrap();
        let second_tab = workspace.open_tab(first.id, "/tmp", "two").unwrap();
        let second = workspace.create_project("second", "/tmp").unwrap();
        let second_project_tab = workspace.open_tab(second.id, "/tmp", "three").unwrap();

        let snapshot = workspace.snapshot();
        assert_eq!(project_id_at_index(&snapshot, 1), Some(first.id));
        assert_eq!(project_id_at_index(&snapshot, 2), Some(second.id));
        assert_eq!(project_id_at_index(&snapshot, 0), None);
        assert_eq!(project_id_at_index(&snapshot, 10), None);
        assert_eq!(
            active_project_tab_at_index(&snapshot, first.id, 2),
            Some(second_tab.id)
        );
        assert_eq!(active_project_tab_at_index(&snapshot, first.id, 0), None);
        assert_eq!(active_project_tab_at_index(&snapshot, first.id, 10), None);

        workspace.reorder_projects(&[second.id, first.id]).unwrap();
        let reordered = workspace.snapshot();
        assert_eq!(project_id_at_index(&reordered, 1), Some(second.id));
        assert_eq!(workspace.preferred_tab(first.id), Some(second_tab.id));
        assert_eq!(
            workspace.preferred_tab(second.id),
            Some(second_project_tab.id)
        );

        workspace.focus_tab(first_tab.id).unwrap();
        assert_eq!(workspace.preferred_tab(first.id), Some(first_tab.id));
    }

    #[test]
    fn rendered_close_keeps_its_exact_id_and_engine_fallback_semantics() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let workspace = Arc::new(Workspace::new());
        let project = workspace.create_project("one", "/tmp").unwrap();
        let sibling = workspace.open_tab(project.id, "/tmp", "sibling").unwrap();
        let doomed = workspace.open_tab(project.id, "/tmp", "doomed").unwrap();
        workspace.focus_tab(doomed.id).unwrap();
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::new(PtySupervisor::new()),
            "/tmp/roost-iced-close-test.sock".into(),
        );

        assert_eq!(
            close_tab_by_id(&runtime, &client, doomed.id).unwrap(),
            CloseTabOutcome::Closed
        );
        assert_eq!(workspace.active(), (project.id, sibling.id));

        // A queued click message retains the ID rendered into it. If another
        // actor already removed that tab, replaying the stale message is an
        // expected no-op and cannot close the newly-active sibling.
        assert_eq!(
            close_tab_by_id(&runtime, &client, doomed.id).unwrap(),
            CloseTabOutcome::AlreadyGone
        );
        assert!(workspace.tab(sibling.id).is_ok());

        let last_project = workspace.create_project("last", "/tmp").unwrap();
        let last = workspace.open_tab(last_project.id, "/tmp", "last").unwrap();
        workspace.focus_tab(last.id).unwrap();
        assert_eq!(
            close_tab_by_id(&runtime, &client, last.id).unwrap(),
            CloseTabOutcome::Closed
        );
        assert!(
            workspace
                .snapshot()
                .iter()
                .all(|project| project.id != last_project.id),
            "closing a project's last tab must remove the project"
        );
        assert_eq!(workspace.active(), (project.id, sibling.id));
    }

    #[test]
    fn created_project_is_named_seeded_with_one_tab_and_activated() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let workspace = Arc::new(Workspace::new());
        workspace.create_project("existing", "/tmp").unwrap();
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::new(PtySupervisor::new()),
            "/tmp/roost-iced-create-project-test.sock".into(),
        );

        let (project_id, tab_id) = create_project_flow(&runtime, &client).unwrap();

        let snapshot = workspace.snapshot();
        let created = snapshot
            .iter()
            .find(|project| project.id == project_id)
            .expect("created project in snapshot");
        assert_eq!(created.name, "Untitled 2");
        assert_eq!(created.tabs.len(), 1);
        assert_eq!(created.tabs[0].id, tab_id);
        assert_eq!(created.tabs[0].cwd, created.cwd);
        assert_eq!(workspace.active(), (project_id, tab_id));
    }

    #[test]
    fn keyboard_route_requires_a_live_terminal_and_gives_editor_precedence() {
        assert_eq!(
            resolve_keyboard_route(false, false, 7, false),
            KeyboardRoute::None
        );
        assert_eq!(
            resolve_keyboard_route(false, false, 7, true),
            KeyboardRoute::Terminal(7)
        );
        assert_eq!(
            resolve_keyboard_route(false, true, 7, true),
            KeyboardRoute::Palette
        );
        assert_eq!(
            resolve_keyboard_route(true, true, 7, true),
            KeyboardRoute::Editor
        );
    }

    #[test]
    fn ui_tasks_compose_without_overwriting_an_earlier_focus_request() {
        let task = UiTask::FocusWidget(Id::unique()).then(UiTask::Resize(
            window::Id::unique(),
            Size::new(800.0, 600.0),
        ));
        let UiTask::Then(first, second) = task else {
            panic!("both UI tasks must be retained");
        };
        assert!(matches!(*first, UiTask::FocusWidget(_)));
        assert!(matches!(*second, UiTask::Resize(_, _)));
    }

    #[test]
    fn window_open_tasks_keep_resize_bookkeeping_separate_from_screenshot_work() {
        let id = window::Id::unique();
        let retained_size = Size::new(820.0, 520.0);
        let mut window_id = None;
        let mut pending_resize = Some(retained_size);
        let mut screenshots = ScreenshotQueue::default();
        let (reply, _result) = tokio::sync::oneshot::channel();
        screenshots.enqueue(1, reply);

        let opened =
            prepare_window_opened(&mut window_id, &mut pending_resize, &mut screenshots, id);
        assert!(opened.retained_resize_scheduled);
        let UiTask::Then(first, second) = opened.task else {
            panic!("retained resize and screenshot must both be scheduled");
        };
        assert!(matches!(
            *first,
            UiTask::Resize(scheduled, size) if scheduled == id && size == retained_size
        ));
        assert!(matches!(*second, UiTask::Screenshot(scheduled) if scheduled == id));

        // A later open/focus/resize notification cannot duplicate the active
        // capture and has no retained resize to suppress native geometry.
        let reopened =
            prepare_window_opened(&mut window_id, &mut pending_resize, &mut screenshots, id);
        assert!(!reopened.retained_resize_scheduled);
        assert!(matches!(reopened.task, UiTask::None));

        let mut screenshot_only = ScreenshotQueue::default();
        let (reply, _result) = tokio::sync::oneshot::channel();
        screenshot_only.enqueue(1, reply);
        let opened = prepare_window_opened(
            &mut window_id,
            &mut pending_resize,
            &mut screenshot_only,
            id,
        );
        assert!(!opened.retained_resize_scheduled);
        assert!(matches!(opened.task, UiTask::Screenshot(scheduled) if scheduled == id));
    }

    #[test]
    fn pointer_origin_routing_never_substitutes_the_active_or_a_closed_tab() {
        let mut tabs = HashMap::from([(41, "origin"), (42, "active")]);
        assert_eq!(
            pointer_origin_tab(&mut tabs, 41).map(|value| *value),
            Some("origin")
        );
        tabs.remove(&41);
        assert_eq!(pointer_origin_tab(&mut tabs, 41), None);
        assert_eq!(
            pointer_origin_tab(&mut tabs, 42).map(|value| *value),
            Some("active")
        );
    }

    #[test]
    fn configured_copy_can_replace_a_former_palette_trigger() {
        let bindings = keybind::canonicalize_bindings(
            keybind::default_bindings(),
            vec![
                ("alt+shift+p".into(), "copy".into()),
                ("ctrl+shift+v".into(), "unbind".into()),
                ("ctrl+alt+v".into(), "paste".into()),
            ],
            |_| {},
        );
        assert_eq!(
            bindings.get(&keybind::parse_trigger("alt+shift+p").unwrap()),
            Some(&KeybindAction::Copy)
        );
        assert!(!bindings.contains_key(&keybind::parse_trigger("ctrl+shift+v").unwrap()));
        assert_eq!(
            bindings.get(&keybind::parse_trigger("ctrl+alt+v").unwrap()),
            Some(&KeybindAction::Paste)
        );
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
        assert_eq!(
            tab_status_color(AgentLifecycle::Inactive),
            Color::TRANSPARENT,
            "inactive tabs reserve the status slot without painting a dot"
        );
        assert_eq!(
            tab_status_color(AgentLifecycle::Working),
            agent_color(AgentLifecycle::Working)
        );
    }

    #[test]
    fn failed_font_geometry_batch_rolls_back_in_reverse_and_does_not_persist() {
        let original_metrics = TerminalMetrics::measure(13.0).expect("original metrics");
        let next_metrics = TerminalMetrics::measure(14.0).expect("next metrics");
        let original = TerminalGeometry {
            cols: 100,
            rows: 32,
            metrics: original_metrics,
            metric_generation: 4,
        };
        let mut states = HashMap::from([(7_i64, original), (11_i64, original)]);
        let mut operations = Vec::new();
        let mut persisted = false;

        let result =
            apply_geometry_batch(
                &[7, 11],
                92,
                29,
                next_metrics,
                5,
                |operation| match operation {
                    GeometryBatchOperation::Apply {
                        tab_id,
                        cols,
                        rows,
                        metrics,
                        metric_generation,
                    } => {
                        operations.push(format!("apply:{tab_id}"));
                        if tab_id == 11 {
                            return Err("injected tab failure".to_string());
                        }
                        let previous = states.insert(
                            tab_id,
                            TerminalGeometry {
                                cols,
                                rows,
                                metrics,
                                metric_generation,
                            },
                        );
                        Ok(Some(GeometryChange {
                            previous,
                            current: states[&tab_id],
                            grid_changed: true,
                            metrics_changed: true,
                            deferred_replies: Vec::new(),
                        }))
                    }
                    GeometryBatchOperation::Rollback { tab_id, previous } => {
                        operations.push(format!("rollback:{tab_id}"));
                        states.insert(tab_id, previous);
                        Ok(None)
                    }
                },
            );
        if result.is_ok() {
            persisted = true;
        }

        assert_eq!(
            result.expect_err("second tab must fail"),
            GeometryBatchFailure {
                tab_id: 11,
                apply: "injected tab failure".to_string(),
                rollback: Vec::new(),
            }
        );
        assert_eq!(operations, ["apply:7", "apply:11", "rollback:7"]);
        assert_eq!(states[&7], original);
        assert_eq!(states[&11], original);
        assert!(
            !persisted,
            "failed live application cannot reach persistence"
        );
    }
}
