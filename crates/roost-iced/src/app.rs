use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::pin::Pin;
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
    SidebarDumpProject, SidebarDumpResult, WindowMetricsResult,
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
    PageDirection, PageRoute, RenderState, ScrollDirection, ScrollRoute, Terminal, TerminalOptions,
    TerminalScroll, TerminalSelection,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::engine_feed::{self, EngineBatch, EngineFeed, EngineFeedReceiver, EngineFeedSender};
use crate::font_registry::{system_font_registry, FontRegistry};
use crate::palette_scroll::Visibility;
use crate::sidebar_resize::SidebarResizeGrip;
use crate::strip_reorder::{ReorderStrip, StripEvent};
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

pub(crate) use self::interactions::RenameTarget;
use self::interactions::{
    agent_rows_hidden, arm_rename_completion_for_open_editor, consume_rename_completion_key,
    enqueue_osc_clipboard_write, native_file_drop_origin, paste_bytes, same_stable_ids,
    ClipboardQueue, FileDropQueue, ProjectDragPreview, RenameCompletionKey, RenameEditor,
    ScreenshotQueue, TabDragPreview,
};
pub(crate) use self::palettes::ProviderRunResult;
pub(crate) use self::palettes::PALETTE_RETRY_INTERVAL;
use self::palettes::{
    apply_with_rollback, ellipsize_palette_text, palette_agent_left_text, palette_row_id,
    palette_title_runs, FontSizeTransition, PaletteReplyRoute, PaletteVisibilityRequest,
    PALETTE_AGENT_PROJECT_MAX_COLUMNS,
};
pub(crate) use self::servicing::{AgentMetricsResult, ATTACH_RETRY_INTERVAL};
use self::terminal_tab::{
    apply_geometry_batch, pointer_origin_tab, refresh_or_warn, terminal_grid,
    GeometryBatchOperation, NativePointerDispatch, TerminalTab,
};
#[cfg(test)]
use self::terminal_tab::{
    attach_test_terminal, GeometryBatchFailure, GeometryChange, LocalPointerGesture,
    NativePointerOutcome, TerminalGeometry,
};

const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 32;
const STATUS_BANNER_DURATION: Duration = Duration::from_secs(5);
/// How often the banner's expiry is checked while one is up. Coarse
/// against the five-second life it polices — the banner is allowed to
/// outlive its deadline by up to one of these.
pub(crate) const STATUS_TICK_INTERVAL: Duration = Duration::from_millis(500);
const CONFIRM_PANEL_WIDTH: f32 = 420.0;

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

    fn is_active(&self) -> bool {
        self.message.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfirmDeleteProject {
    project_id: i64,
    name: String,
}

fn confirm_delete_target(projects: &[Project], project_id: i64) -> Option<ConfirmDeleteProject> {
    projects
        .iter()
        .find(|project| project.id == project_id)
        .map(|project| ConfirmDeleteProject {
            project_id: project.id,
            name: project.name.clone(),
        })
}

fn reconcile_confirm_delete(confirm: &mut Option<ConfirmDeleteProject>, projects: &[Project]) {
    if let Some(open) = confirm.as_ref() {
        // Re-resolve rather than only checking liveness: an external
        // rename while the dialog is open must not leave the user
        // approving a deletion under a stale label.
        *confirm = confirm_delete_target(projects, open.project_id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmDialogAction {
    Delete,
    Cancel,
}

fn confirm_dialog_action(event: &keyboard::Event) -> Option<ConfirmDialogAction> {
    match event {
        keyboard::Event::KeyPressed {
            key: Key::Named(Named::Enter),
            repeat: false,
            ..
        } => Some(ConfirmDialogAction::Delete),
        keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            repeat: false,
            ..
        } => Some(ConfirmDialogAction::Cancel),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseTabOutcome {
    Closed,
    AlreadyGone,
}

/// Runs on the engine runtime, not the UI thread. The `anyhow::Error` is
/// classified and stringified here so nothing but `Send + Clone` data
/// crosses back into a message.
async fn close_tab_by_id(client: &LocalClient, tab_id: i64) -> Result<CloseTabOutcome, String> {
    match client.close_tab(tab_id).await {
        Ok(()) => Ok(CloseTabOutcome::Closed),
        Err(error)
            if matches!(
                error.downcast_ref::<WorkspaceError>(),
                Some(WorkspaceError::TabNotFound(id)) if *id == tab_id
            ) =>
        {
            Ok(CloseTabOutcome::AlreadyGone)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteProjectOutcome {
    Deleted,
    AlreadyGone,
}

async fn delete_project_flow(
    client: &LocalClient,
    project_id: i64,
) -> Result<DeleteProjectOutcome, String> {
    match client.delete_project(project_id).await {
        Ok(_) => Ok(DeleteProjectOutcome::Deleted),
        Err(error)
            if matches!(
                error.downcast_ref::<WorkspaceError>(),
                Some(WorkspaceError::ProjectNotFound(id)) if *id == project_id
            ) =>
        {
            Ok(DeleteProjectOutcome::AlreadyGone)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// What a mutation that ran off the UI thread reported back, carried by
/// `Message::EngineOp`. Every payload is `Send + Clone + 'static` and
/// every error is already a `String`: an `anyhow::Error` never rides in a
/// message.
#[derive(Debug, Clone)]
pub enum EngineOpResult {
    /// `op` is the id the dispatch allocated. A `palette.activate` that
    /// dispatched this close is parked on it — see
    /// [`settle_palette_activation`] — and every other route simply has
    /// nothing stashed under it.
    TabClosed {
        op: u64,
        tab_id: i64,
        result: Result<CloseTabOutcome, String>,
    },
    ProjectDeleted {
        project_id: i64,
        result: Result<DeleteProjectOutcome, String>,
    },
    /// One tab opened: the new-tab routes and the launcher's rows. `op`
    /// keys the deferred palette reply exactly as [`Self::TabClosed`]'s
    /// does.
    TabOpened {
        op: u64,
        project_id: i64,
        result: Result<i64, String>,
    },
    /// A project and its first tab, from the one compound op that
    /// creates both.
    ProjectCreated {
        op: u64,
        result: Result<(i64, i64), String>,
    },
    /// `op` is the id the editor recorded at dispatch: a completion that
    /// no longer matches belongs to an editor the user has already
    /// dismissed, and touches nothing.
    Renamed {
        op: u64,
        target: RenameTarget,
        result: Result<(), String>,
    },
    /// `op` is the id the drag preview recorded at dispatch, so a
    /// superseded reorder's completion cannot clear a newer drag's
    /// preview.
    TabsReordered {
        op: u64,
        project_id: i64,
        ordered_ids: Vec<i64>,
        result: Result<(), String>,
    },
    ProjectsReordered {
        op: u64,
        ordered_ids: Vec<i64>,
        result: Result<(), String>,
    },
}

/// Build the future behind [`UiTask::EngineOp`]: the op runs on the
/// engine runtime, the Iced task only awaits the join, and `complete`
/// turns whichever way it went into the message the UI thread handles.
///
/// A join failure — the op panicked, or the runtime is shutting down —
/// becomes that op's own error rather than a dropped completion: every
/// dispatch owes the UI exactly one [`EngineOpResult`].
fn spawn_engine_op<T: Send + 'static>(
    handle: tokio::runtime::Handle,
    op: impl Future<Output = Result<T, String>> + Send + 'static,
    complete: impl FnOnce(Result<T, String>) -> EngineOpResult + Send + 'static,
) -> EngineOpFuture {
    Box::pin(async move {
        let result = match handle.spawn(op).await {
            Ok(result) => result,
            Err(error) => Err(error.to_string()),
        };
        complete(result)
    })
}

/// Fold a completed engine op into what it owes the status banner, and
/// log what it owes the log.
///
/// The `AlreadyGone` outcomes are successes, not errors: the entity the
/// user asked to remove is gone, which is the state they asked for. They
/// stay silent for the same reason they did when these calls blocked the
/// UI thread — but they are also precisely the outcomes the engine
/// returns *before* committing anything, so they broadcast no workspace
/// event and the completion is the only thing that will ever trigger the
/// reconcile they need. Hence [`App::engine_op_completed`] reconciles on
/// every arm.
fn engine_op_status(result: EngineOpResult) -> Option<String> {
    match result {
        EngineOpResult::TabClosed { tab_id, result, .. } => match result {
            Ok(CloseTabOutcome::Closed) => None,
            Ok(CloseTabOutcome::AlreadyGone) => {
                tracing::debug!(tab_id, "close_tab: rendered tab already gone");
                None
            }
            Err(error) => {
                tracing::warn!(%error, tab_id, "close_tab failed");
                Some(error)
            }
        },
        EngineOpResult::ProjectDeleted { project_id, result } => match result {
            Ok(DeleteProjectOutcome::Deleted) => None,
            Ok(DeleteProjectOutcome::AlreadyGone) => {
                tracing::debug!(project_id, "confirmed delete: project already gone");
                None
            }
            Err(error) => {
                tracing::warn!(%error, project_id, "delete project failed");
                Some(error)
            }
        },
        // The op-id-guarded state machines report their own outcome:
        // whether a rename or a reorder owes the banner anything depends
        // on the op id it carries and not on the result alone, so
        // `engine_op_completed` routes those to the state machine that
        // dispatched them rather than here.
        EngineOpResult::Renamed { .. }
        | EngineOpResult::TabsReordered { .. }
        | EngineOpResult::ProjectsReordered { .. } => None,
        EngineOpResult::TabOpened {
            project_id, result, ..
        } => match result {
            Ok(tab_id) => {
                tracing::debug!(project_id, tab_id, "opened tab");
                None
            }
            Err(error) => {
                tracing::warn!(%error, project_id, "open tab failed");
                Some(error)
            }
        },
        EngineOpResult::ProjectCreated { result, .. } => match result {
            Ok((project_id, tab_id)) => {
                tracing::debug!(project_id, tab_id, "created project");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "create project failed");
                Some(error)
            }
        },
    }
}

impl EngineOpResult {
    /// The id a deferred `palette.activate` reply would be stashed
    /// under. Only the completions whose rows became asynchronous carry
    /// one; the rest can owe no IPC reply, so they answer `None` rather
    /// than probe the stash.
    fn palette_op(&self) -> Option<u64> {
        match self {
            Self::TabClosed { op, .. }
            | Self::TabOpened { op, .. }
            | Self::ProjectCreated { op, .. } => Some(*op),
            // Delete reaches the palette only through the confirm
            // overlay, which answers `palette.activate` the moment it
            // opens; renames and reorders have no palette row at all.
            Self::ProjectDeleted { .. }
            | Self::Renamed { .. }
            | Self::TabsReordered { .. }
            | Self::ProjectsReordered { .. } => None,
        }
    }
}

/// Create a project and seed it with its first tab — one op, two engine
/// calls sequential *inside* the future, so the UI thread never waits
/// between them.
///
/// A tab-open failure after the create committed rolls the project back
/// too: `LocalClient::open_tab`'s spawn-failure path closes the tab it
/// opened, and closing a project's last tab deletes the project. The
/// error is reported and the completion's reconcile shows the rollback,
/// exactly as the blocking version behaved when its second call failed.
async fn create_project_flow(client: &LocalClient) -> Result<(i64, i64), String> {
    let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let project = client
        .create_project("", &cwd)
        .await
        .map_err(|error| error.to_string())?;
    let tab_id = open_tab_flow(client, project.id, cwd, String::new(), Vec::new()).await?;
    // `open_tab` already steals the selection, but create's activation must
    // not depend on another op's side effect.
    client
        .workspace
        .focus_tab(tab_id)
        .map_err(|error| error.to_string())?;
    Ok((project.id, tab_id))
}

/// The one tab-open op behind every route that opens one: the new-tab
/// button, keybind and palette row, the launcher's command rows, and
/// create-project's seed tab.
async fn open_tab_flow(
    client: &LocalClient,
    project_id: i64,
    cwd: String,
    title: String,
    argv: Vec<String>,
) -> Result<i64, String> {
    client
        .open_tab(
            project_id,
            &cwd,
            &title,
            &argv,
            u32::from(DEFAULT_COLS),
            u32::from(DEFAULT_ROWS),
        )
        .await
        .map(|tab| tab.id)
        .map_err(|error| error.to_string())
}

/// A `palette.activate` reply channel, parked while the row's action is
/// in flight (`roost_engine::ipc`'s `PaletteReply`).
type PaletteActivateReply = tokio::sync::oneshot::Sender<Result<PaletteStateResult, String>>;

/// Answer the `palette.activate` invocation that dispatched `op`, if that
/// invocation came from IPC; the keybind and pointer routes through the
/// same rows stash nothing and this is a no-op for them.
///
/// The reply belongs to the INVOCATION, not to the palette. A client is
/// blocked on it, so it is sent whatever the palette is doing by now —
/// dismissed, reopened on another frame, or never reopened. That is also
/// why success answers with the closed state built here rather than a
/// live `palette_state_result()`: every row that dispatches an op
/// dismisses the palette at dispatch, so the closed state is what this
/// invocation produced, and whatever is open at completion time belongs
/// to someone else.
fn settle_palette_activation(
    pending: &mut HashMap<u64, PaletteActivateReply>,
    op: u64,
    error: Option<String>,
) {
    let Some(reply) = pending.remove(&op) else {
        return;
    };
    let _ = reply.send(match error {
        Some(error) => Err(error),
        None => Ok(palettes::closed_palette_state_result()),
    });
}

fn effective_sidebar_width(collapsed: bool, width: f32) -> f32 {
    if collapsed {
        0.0
    } else {
        width
    }
}

/// A published drag width is worth applying only while the grip exists — a
/// collapsed sidebar has none, so the update is stale — and only when it moves
/// the sidebar: a pointer travelling past a clamp bound would otherwise re-grid
/// every tab for an identical frame.
fn drag_width_is_actionable(collapsed: bool, live_width: f32, width: f32) -> bool {
    !collapsed && width != live_width
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
    Confirm,
    Editor,
    Palette,
    Terminal(i64),
}

fn resolve_keyboard_route(
    confirm_open: bool,
    editor_open: bool,
    palette_open: bool,
    active_tab: i64,
    active_terminal_live: bool,
) -> KeyboardRoute {
    if confirm_open {
        KeyboardRoute::Confirm
    } else if editor_open {
        KeyboardRoute::Editor
    } else if palette_open {
        KeyboardRoute::Palette
    } else if active_terminal_live {
        KeyboardRoute::Terminal(active_tab)
    } else {
        KeyboardRoute::None
    }
}

/// An engine mutation in flight. Boxed rather than generic because
/// [`UiTask`] is the one shape every `App` method returns, and the
/// Iced-side mapping is `Task::future(_).map(Message::EngineOp)`.
pub type EngineOpFuture = Pin<Box<dyn Future<Output = EngineOpResult> + Send>>;

#[derive(Default)]
pub enum UiTask {
    #[default]
    None,
    Then(Box<UiTask>, Box<UiTask>),
    /// A mutation dispatched to the engine runtime. Its completion comes
    /// back as `Message::EngineOp`, never as a return value — the UI
    /// thread does not wait for the engine.
    EngineOp(EngineOpFuture),
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
    /// One-shot: wake once the file-drop gesture's debounce window has
    /// elapsed. Scheduled where the deadline is set, never polled.
    FileDropDeadline(Duration),
    PaletteVisibility {
        scroll_id: Id,
        row_id: Id,
        session: u64,
        revision: u64,
        measurement_generation: u64,
        reveal: bool,
    },
}

/// A dispatched engine mutation: the Iced task that will deliver its
/// completion, plus the id that completion will carry.
///
/// `op` is `None` when the route settled without dispatching anything — a
/// guard turned the action into a no-op — and there is then no completion
/// for a deferred `palette.activate` reply to wait on.
#[derive(Default)]
struct EngineDispatch {
    task: UiTask,
    op: Option<u64>,
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
    window_id: Option<window::Id>,
    pending_window_resize: Option<Size>,
    screenshots: ScreenshotQueue,
    window_size: Size,
    /// Set only while the seam is being dragged; the engine holds the
    /// committed width, and persisting per pointer-move would rewrite
    /// `state.json` on every frame.
    sidebar_drag_width: Option<f32>,
    modifiers: keyboard::Modifiers,
    test_mode: bool,
    status: StatusBanner,
    rename_editor: Option<RenameEditor>,
    rename_input_id: Id,
    rename_focus_requested: bool,
    rename_completion_key: Option<RenameCompletionKey>,
    /// The rename dispatched and not yet reported back. It blocks a
    /// second submit and identifies which completion the open editor is
    /// still waiting for; a completion carrying any other id belongs to
    /// an editor that has since been dismissed.
    rename_op: Option<u64>,
    /// Monotonic source of the ids that guard in-flight engine ops.
    next_engine_op: u64,
    tab_drag_preview: Option<TabDragPreview>,
    tab_strip_generation: u64,
    project_drag_preview: Option<ProjectDragPreview>,
    project_strip_generation: u64,
    confirm_delete: Option<ConfirmDeleteProject>,
    pending_attachments: servicing::PendingAttachments,
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
    provider_request: u64,
    provider_frames: HashMap<String, provider::Provider>,
    palette_present_reply:
        Option<tokio::sync::oneshot::Sender<Result<PalettePresentResult, String>>>,
    /// `palette.activate` replies owed by engine ops still in flight,
    /// keyed by op id. Separate from `palette_present_reply` in every
    /// way: that one belongs to an open `palette.present` and is answered
    /// by the user's pick or dismiss, these belong to invocations whose
    /// row already ran and are answered by their own completion.
    palette_activate_replies: HashMap<u64, PaletteActivateReply>,
    clipboard: ClipboardQueue,
    /// A clone of `runtime`'s handle, so a mutation can be spawned onto
    /// the engine runtime from a `&self` method and awaited by an Iced
    /// task. Cheap and `Send`; dropping one is inert, so it takes no part
    /// in the ordering below.
    runtime_handle: tokio::runtime::Handle,
    // Field order is intentional: terminal sessions and the engine feed
    // (whose receiver carries the wake every sender notifies on) are
    // dropped before the runtime — a dropped receiver is how the adapter
    // tasks learn to stop. The lock is held until every runtime task has
    // been cancelled and joined by Runtime::drop.
    feed_rx: EngineFeedReceiver,
    feed_tx: EngineFeedSender,
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
        let supervisor = Arc::new(PtySupervisor::new());
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            profile.socket_path.clone(),
        );

        hydrate_workspace(&runtime, &client)?;

        let (feed_tx, feed_rx) = engine_feed::channel();
        // One feed, one arrival order across sources — see engine_feed.
        runtime.spawn(engine_feed::pump_workspace_events(
            Arc::clone(&workspace),
            feed_tx.clone(),
        ));
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.spawn(engine_feed::pump_ui_requests(ui_rx, feed_tx.clone()));
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
            window_id: None,
            pending_window_resize: None,
            screenshots: ScreenshotQueue::default(),
            window_size: Size::new(1100.0, 720.0),
            sidebar_drag_width: None,
            modifiers: keyboard::Modifiers::default(),
            test_mode: std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1"),
            status: StatusBanner::default(),
            rename_editor: None,
            rename_input_id: Id::unique(),
            rename_focus_requested: false,
            rename_completion_key: None,
            rename_op: None,
            next_engine_op: 1,
            tab_drag_preview: None,
            tab_strip_generation: 1,
            project_drag_preview: None,
            project_strip_generation: 1,
            confirm_delete: None,
            pending_attachments: servicing::PendingAttachments::default(),
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
            provider_request: 0,
            provider_frames: HashMap::new(),
            palette_present_reply: None,
            palette_activate_replies: HashMap::new(),
            clipboard: ClipboardQueue::default(),
            runtime_handle: runtime.handle().clone(),
            feed_rx,
            feed_tx,
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

    /// The wake this app's feed notifies on, for the subscription that
    /// drives [`Self::service_engine_ready`].
    pub fn wake_handle(&self) -> Arc<tokio::sync::Notify> {
        self.feed_rx.wake_handle()
    }

    /// The sole drain driver: everything asynchronous reaches the app
    /// through the feed, and every feed send wakes this. The
    /// [`Self::service_engine`] drain reconciles, refreshes and closes
    /// exited tabs off the batch it observed, and this ends it by starting
    /// a queued screenshot, so an IPC screenshot request in this very
    /// batch does not wait a frame.
    ///
    /// An empty drain reconciles and refreshes nothing, which is what
    /// makes the spurious wakes the permit model guarantees free — and a
    /// batch of nothing but PTY bytes now genuinely skips the workspace
    /// reconcile (see `EngineBatch::should_reconcile`), because no
    /// periodic pass runs one behind its back any more.
    pub fn service_engine_ready(&mut self) -> UiTask {
        self.service_engine()
            .then(self.screenshots.start_next(self.window_id))
    }

    /// Dispatch an engine mutation without blocking the UI thread.
    ///
    /// The op itself runs on the engine runtime — `handle.spawn` keeps
    /// engine work (and the `tokio::spawn`s it may do) on the runtime that
    /// owns it — while the Iced task only awaits the join. A join failure
    /// (the op panicked, or the runtime is shutting down) surfaces as that
    /// op's own error, so no dispatch can end in a completion that never
    /// arrives.
    fn engine_op<T: Send + 'static>(
        &self,
        op: impl Future<Output = Result<T, String>> + Send + 'static,
        complete: impl FnOnce(Result<T, String>) -> EngineOpResult + Send + 'static,
    ) -> UiTask {
        UiTask::EngineOp(spawn_engine_op(self.runtime_handle.clone(), op, complete))
    }

    /// The id an about-to-be-dispatched op is guarded by.
    fn take_engine_op_id(&mut self) -> u64 {
        let op = self.next_engine_op;
        self.next_engine_op = self.next_engine_op.wrapping_add(1);
        op
    }

    /// An engine mutation reported back — `Message::EngineOp`.
    ///
    /// Reconcile runs on every arm, success or failure — including the
    /// arms that drop a stale completion. A batch of feed events is not a
    /// substitute: the tolerated already-gone outcomes broadcast nothing
    /// at all, and a failed op leaves UI state that optimistically
    /// anticipated it needing the authoritative snapshot back.
    pub fn engine_op_completed(&mut self, result: EngineOpResult) {
        // The reply an async palette row's client is blocked on, held
        // until after the reconcile below: a client that reads state the
        // moment it is answered must not race the UI's own fold-in of the
        // action it just heard about.
        let mut deferred_activation = None;
        match result {
            simple @ (EngineOpResult::TabClosed { .. }
            | EngineOpResult::ProjectDeleted { .. }
            | EngineOpResult::TabOpened { .. }
            | EngineOpResult::ProjectCreated { .. }) => {
                let palette_op = simple.palette_op();
                let error = engine_op_status(simple);
                // The banner's verdict and the IPC reply's are the same
                // verdict: an outcome silent enough to raise no banner is
                // an outcome the client hears as success.
                deferred_activation = palette_op.map(|op| (op, error.clone()));
                if let Some(error) = error {
                    self.set_status(error);
                }
            }
            EngineOpResult::Renamed { op, target, result } => {
                self.rename_completed(op, target, result)
            }
            EngineOpResult::TabsReordered {
                op,
                project_id,
                ordered_ids,
                result,
            } => self.tab_reorder_completed(op, project_id, &ordered_ids, result),
            EngineOpResult::ProjectsReordered {
                op,
                ordered_ids,
                result,
            } => self.project_reorder_completed(op, &ordered_ids, result),
        }
        self.reconcile();
        if let Some((op, error)) = deferred_activation {
            settle_palette_activation(&mut self.palette_activate_replies, op, error);
        }
    }

    pub fn file_dropped(&mut self, window_id: window::Id, path: PathBuf) -> UiTask {
        if self.window_id != Some(window_id) {
            tracing::debug!("ignored native file drop for an unowned window");
            return UiTask::None;
        }
        let now = Instant::now();
        let new_origin = native_file_drop_origin(self.window_id, window_id, self.keyboard_route());
        let (ready, accepted) = self.file_drops.push_at(new_origin, path, now);
        if let Some(batch) = ready {
            self.deliver_file_drop(batch);
        }
        if !accepted {
            tracing::debug!("ignored native file drop without an active terminal input route");
        }
        // Every accepted path schedules its own one-shot, including the
        // ones that only extended the window. The earlier shots then fire
        // against a deadline that has moved and find nothing ready, which
        // is why they need no cancellation.
        match self.file_drops.pending_deadline() {
            Some(deadline) => UiTask::FileDropDeadline(deadline.saturating_duration_since(now)),
            None => UiTask::None,
        }
    }

    /// A file-drop debounce window elapsed. Stale shots — one whose
    /// deadline a later path extended — find nothing ready and do nothing.
    pub fn file_drop_deadline(&mut self) {
        if let Some(batch) = self.file_drops.take_ready_at(Instant::now()) {
            self.deliver_file_drop(batch);
        }
    }

    /// The status banner is up, so its expiry is due — `Message::StatusTick`.
    pub fn expire_status(&mut self) {
        self.status.expire_at(Instant::now());
    }

    pub fn status_active(&self) -> bool {
        self.status.is_active()
    }

    pub fn palette_retry_pending(&self) -> bool {
        self.palette_visibility_request.is_pending()
    }

    /// A tab whose budget ran out stays tracked but stops arming this.
    pub fn attach_retry_pending(&self) -> bool {
        self.pending_attachments.has_retryable()
    }

    fn set_status(&mut self, message: impl Into<String>) {
        // A toast restructures the root widget tree, which drops the grip's
        // widget state — a live drag would never publish its end.
        self.commit_sidebar_drag();
        self.status.set_at(message, Instant::now());
    }

    fn live_sidebar_width(&self) -> f32 {
        self.sidebar_drag_width
            .unwrap_or_else(|| self.workspace.sidebar_width() as f32)
    }

    fn effective_sidebar_width(&self) -> f32 {
        effective_sidebar_width(
            self.workspace.sidebar_collapsed(),
            self.live_sidebar_width(),
        )
    }

    pub fn resize(&mut self, size: Size) {
        let changed = self.window_size != size;
        self.window_size = size;
        let (cols, rows) =
            terminal_grid(size, self.effective_sidebar_width(), self.terminal_metrics);
        for (tab_id, tab) in &mut self.tabs {
            match tab.apply_geometry(cols, rows, self.terminal_metrics, self.metric_generation) {
                Ok(Some(change)) => {
                    tab.commit_geometry(change);
                    // A re-grid rewrites the viewport and drops hover, so
                    // the snapshot the widget draws describes the old
                    // dimensions until it is rebuilt. Window resizes,
                    // sidebar width drags and collapse all land here.
                    refresh_or_warn(*tab_id, tab, "window re-grid");
                }
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
        if matches!(self.keyboard_route(), KeyboardRoute::Confirm) {
            let mut task = UiTask::None;
            match confirm_dialog_action(&event) {
                Some(ConfirmDialogAction::Delete) => {
                    self.rename_completion_key = Some(RenameCompletionKey::Enter);
                    task = self.execute_confirmed_delete();
                }
                Some(ConfirmDialogAction::Cancel) => {
                    self.rename_completion_key = Some(RenameCompletionKey::Escape);
                    self.cancel_confirm_delete();
                }
                None => {}
            }
            // The modal owns the keyboard: every other event is swallowed so
            // no accelerator or terminal encoder can observe it. The only
            // thing that leaves here is the confirmed deletion's own task.
            return task;
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
                let mut task = UiTask::None;
                match key.as_ref() {
                    Key::Named(Named::Escape) => self.palette_back_or_dismiss(),
                    Key::Named(Named::ArrowUp) => self.move_palette_selection(-1),
                    Key::Named(Named::ArrowDown) => self.move_palette_selection(1),
                    // Confirming a rename row closes the palette and opens
                    // the inline editor, which carries its own focus tail.
                    Key::Named(Named::Enter) => task = self.palette_confirm(),
                    _ => {}
                }
                // The text input widget consumes printable events. Never let
                // a palette keystroke leak through to the active PTY.
                return self.take_palette_focus_task().then(task);
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
        // A bare page key scrolls this tab's own scrollback whenever the shared
        // policy keeps it local — no snap, no encode, nothing on the PTY. The
        // bypass is the policy's decision, never the key's: `Forward` (mouse
        // tracking, alternate screen) falls through to the normal encode below.
        if let Some(direction) = input::bare_page_direction(&event, self.modifiers) {
            match tab.handle_page(direction) {
                Ok(PageRoute::LocalViewport { .. }) => return UiTask::None,
                Ok(PageRoute::Forward) => {}
                Err(error) => {
                    // Only the repaint after a completed local move can fail,
                    // so the key is still consumed; the next refresh recovers.
                    tracing::warn!(?error, active_tab, "terminal page scroll failed");
                    return UiTask::None;
                }
            }
        }
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
            KeyboardRoute::Confirm => {
                self.cancel_confirm_delete();
                UiTask::None
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
            KeybindAction::NewTab => Ok(self.new_tab_dispatch().task),
            KeybindAction::CloseTab => {
                let tab_id = self.workspace.active().1;
                if tab_id == 0 {
                    return Ok(UiTask::None);
                }
                Ok(self.close_tab_dispatch(tab_id).task)
            }
            KeybindAction::NewProject => Ok(self.new_project_dispatch().task),
            KeybindAction::RenameProject => {
                self.begin_rename_target(RenameTarget::Project(self.workspace.active().0))?;
                Ok(self.take_rename_focus_task())
            }
            KeybindAction::RenameTab => {
                self.begin_rename_target(RenameTarget::Tab(self.workspace.active().1))?;
                Ok(self.take_rename_focus_task())
            }
            KeybindAction::CloseProject => {
                // The sentinel id 0 is never in the snapshot, so
                // `confirm_close_project` settles it as a silent no-op.
                self.confirm_close_project(self.workspace.active().0)?;
                Ok(UiTask::None)
            }
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

    /// A modal owns the pointer while it is up, so neither strip arms a
    /// gesture behind it.
    fn strip_gestures_enabled(&self) -> bool {
        self.rename_editor.is_none() && self.confirm_delete.is_none()
    }

    fn keyboard_route(&self) -> KeyboardRoute {
        let active_tab = self.workspace.active().1;
        resolve_keyboard_route(
            self.confirm_delete.is_some(),
            self.rename_editor.is_some(),
            self.palette.is_some(),
            active_tab,
            self.tabs.contains_key(&active_tab),
        )
    }

    pub fn set_window_focus(&mut self, focused: bool) {
        if !focused {
            self.rename_completion_key = None;
            self.cancel_drags();
            self.cancel_confirm_delete();
        }
        self.workspace.set_window_focused(focused);
        if let Some(tab) = self.tabs.get(&self.workspace.active().1) {
            tab.set_window_focus(focused);
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let content = self.view_body();
        let Some(confirm) = &self.confirm_delete else {
            return content;
        };
        let panel = container(
            column![
                text("Delete project?").size(15).font(Font {
                    weight: font::Weight::Bold,
                    ..Font::default()
                }),
                text(format!(
                    "“{}” and all of its tabs will be deleted. This cannot be undone.",
                    confirm.name
                ))
                .size(12)
                .color(chrome::MUTED_TEXT),
                row![
                    iced::widget::Space::new().width(Fill),
                    button(text("Cancel").size(12))
                        .padding([4, 12])
                        .style(chrome::transparent_button)
                        .on_press(Message::ConfirmDeleteCancel),
                    button(text("Delete").size(12))
                        .padding([4, 12])
                        .style(chrome::danger_button)
                        .on_press(Message::ConfirmDeleteConfirm)
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            ]
            .spacing(12),
        )
        .width(Fill)
        .max_width(CONFIRM_PANEL_WIDTH)
        .height(Shrink)
        .padding(16)
        .style(chrome::palette_panel);
        let overlay = container(mouse_area(panel).on_press(Message::ConfirmDeleteCardPressed))
            .padding(16)
            .center(Fill);
        let catcher = mouse_area(iced::widget::Space::new().width(Fill).height(Fill))
            .on_press(Message::ConfirmDeleteCancel);
        stack![content, catcher, overlay]
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn view_body(&self) -> Element<'_, Message> {
        let (active_project, active_tab) = self.workspace.active();
        let authoritative_project_ids = self.sidebar_project_ids();
        let visual_project_ids = self
            .project_drag_preview
            .as_ref()
            .filter(|preview| {
                preview.orders_the_strip(self.project_strip_generation)
                    && preview.original_ids == authoritative_project_ids
                    && same_stable_ids(&preview.ordered_ids, &authoritative_project_ids)
            })
            .map(|preview| preview.ordered_ids.clone())
            .unwrap_or(authoritative_project_ids);
        // Sidebar rows are large, so the drag styling waits for the threshold
        // rather than flashing on every ordinary click.
        let dragged_project = self
            .project_drag_preview
            .as_ref()
            .filter(|preview| preview.dragging)
            .map(|preview| preview.context.source_id);
        let hide_agent_rows = agent_rows_hidden(self.project_drag_preview.as_ref());
        let mut sidebar_body = column![].spacing(2).padding([4, 0]);
        for project in visual_project_ids.iter().filter_map(|project_id| {
            self.projects
                .iter()
                .find(|project| project.id == *project_id)
        }) {
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
                .style(chrome::project_pill(
                    project.id == active_project,
                    dragged_project == Some(project.id),
                ));
            let project_row = container(
                row![
                    stripe,
                    iced::widget::Space::new().width(chrome::PROJECT_STRIPE_GAP),
                    project_pill,
                    iced::widget::Space::new().width(chrome::PROJECT_RIGHT_INSET)
                ]
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .height(chrome::ROW_HEIGHT);
            let mut project_group = column![project_row].spacing(2);
            if self.config.show_sidebar_agents && !hide_agent_rows {
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
        let sidebar_header = container(text("PROJECTS").size(11).color(chrome::MUTED_TEXT))
            .height(chrome::BAND_HEIGHT)
            .width(Fill)
            .padding([10, 12])
            .style(chrome::surface);
        let sidebar_footer = container(
            button(text("+ New Project").size(11))
                .height(chrome::PILL_HEIGHT)
                .padding([4, 12])
                .style(chrome::footer_chip_button)
                .on_press(Message::NewProject),
        )
        .height(chrome::BAND_HEIGHT)
        .width(Fill)
        .center_x(Fill)
        .padding([5, 8])
        .style(chrome::surface);
        // The strip delegates layout to its content, so its layout node is the
        // column's: one child per project group, which is what the gesture's
        // hit-testing and target index walk.
        let project_strip = ReorderStrip::projects(
            sidebar_body,
            visual_project_ids,
            self.project_strip_generation,
            self.strip_gestures_enabled(),
        );
        let sidebar = container(column![
            sidebar_header,
            scrollable(project_strip).height(Fill),
            sidebar_footer
        ])
        .width(self.live_sidebar_width())
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
                            .is_some_and(|preview| preview.drags(tab.id)),
                    )),
            );
        }
        let tab_strip = ReorderStrip::tabs(
            tab_pills,
            active_project,
            visual_tab_ids,
            self.tab_strip_generation,
            self.strip_gestures_enabled(),
        );
        // A zero-width scrollbar: any visible indicator overlays the 24px
        // pills themselves and reads as a band across the tab row (#281) —
        // the stock 10px filled rail, and even a 2px hover sliver, both did.
        // Wheel/trackpad scrolling is independent of the scrollbar's size.
        let tab_scroller = scrollable(tab_strip)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::hidden(),
            ))
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
            SidebarResizeGrip::new(
                row![sidebar, main].width(Fill).height(Fill),
                self.live_sidebar_width(),
            )
            .into()
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
            .style(chrome::overlay_scrollable)
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
        // The project strip publishes this on press, immediately before the
        // gesture's start event; cancelling the project drag here would bump
        // the generation the pending start still carries and no drag could
        // ever arm. The project strip cancels the tab drag itself when it
        // arms, exactly as the tab strip does in reverse.
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
        self.cancel_drags();
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
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.set_sidebar_collapsed(!self.workspace.sidebar_collapsed());
    }

    pub fn sidebar_resize_dragged(&mut self, width: f32) {
        if !drag_width_is_actionable(
            self.workspace.sidebar_collapsed(),
            self.live_sidebar_width(),
            width,
        ) {
            return;
        }
        self.sidebar_drag_width = Some(width);
        self.resize(self.window_size);
    }

    pub fn sidebar_resize_ended(&mut self) {
        self.commit_sidebar_drag();
    }

    fn commit_sidebar_drag(&mut self) {
        if let Some(width) = self.sidebar_drag_width.take() {
            self.workspace.set_sidebar_width(f64::from(width));
        }
    }

    fn set_sidebar_collapsed(&mut self, collapsed: bool) {
        // Collapsing drops the grip widget from the tree, so a drag that is
        // still live never publishes its end. Commit first so the width the
        // session shows is the width a relaunch restores. Expanding keeps the
        // grip, so a drag in flight there is still the widget's to finish.
        if collapsed {
            self.commit_sidebar_drag();
        }
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

    pub fn new_tab(&mut self) -> UiTask {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.new_tab_dispatch().task
    }

    /// The new-tab route every surface shares. Nothing follows the
    /// dispatch: `Workspace::open_tab` steals the active selection in the
    /// same commit that creates the tab, so focus is the engine's answer
    /// and the completion's reconcile is the whole tail. Nothing here
    /// reads `self.tabs` for the new id, so there is no intent to pend.
    fn new_tab_dispatch(&mut self) -> EngineDispatch {
        let (project_id, _) = self.workspace.active();
        if project_id == 0 {
            return EngineDispatch::default();
        }
        let cwd = self.launch_cwd(project_id);
        self.open_tab_dispatch(project_id, cwd, String::new(), Vec::new())
    }

    fn open_tab_dispatch(
        &mut self,
        project_id: i64,
        cwd: String,
        title: String,
        argv: Vec<String>,
    ) -> EngineDispatch {
        let op = self.take_engine_op_id();
        let client = self.client.clone();
        EngineDispatch {
            task: self.engine_op(
                async move { open_tab_flow(&client, project_id, cwd, title, argv).await },
                move |result| EngineOpResult::TabOpened {
                    op,
                    project_id,
                    result,
                },
            ),
            op: Some(op),
        }
    }

    pub fn new_project(&mut self) -> UiTask {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.new_project_dispatch().task
    }

    /// The sidebar is expanded here, at the dispatch, rather than when
    /// the project lands: revealing where the new project will appear is
    /// the user's own gesture answered, not the engine's report.
    fn new_project_dispatch(&mut self) -> EngineDispatch {
        self.set_sidebar_collapsed(false);
        let op = self.take_engine_op_id();
        let client = self.client.clone();
        EngineDispatch {
            task: self.engine_op(
                async move { create_project_flow(&client).await },
                move |result| EngineOpResult::ProjectCreated { op, result },
            ),
            op: Some(op),
        }
    }

    fn confirm_close_project(&mut self, project_id: i64) -> Result<(), String> {
        let Some(target) = confirm_delete_target(&self.projects, project_id) else {
            return Ok(());
        };
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.dismiss_palette_with_focus_recovery();
        // The modal drops pointer events, so a held terminal button would
        // never see its release: settle every tab's pointer state (synthetic
        // release into tracking PTYs) before the modal owns input.
        for (tab_id, tab) in &mut self.tabs {
            match tab.prepare_pointer_cancel() {
                Ok(release) => {
                    tab.commit_pointer_cancel(release);
                    // The cancel drops hover, so the link underline and
                    // pointer shape the snapshot carries are decorations
                    // for a gesture that no longer exists.
                    refresh_or_warn(*tab_id, tab, "pointer cancel before delete confirm");
                }
                Err(error) => {
                    tracing::warn!(?error, tab_id, "pointer cancel before delete confirm")
                }
            }
        }
        self.confirm_delete = Some(target);
        Ok(())
    }

    fn cancel_confirm_delete(&mut self) {
        self.confirm_delete = None;
    }

    /// The overlay is dismissed here, at the confirm, exactly as it was
    /// when the deletion blocked the UI thread — the user's answer is
    /// taken before the engine hears about it, and a failure surfaces as a
    /// status banner rather than by leaving the dialog up.
    fn execute_confirmed_delete(&mut self) -> UiTask {
        let Some(confirm) = self.confirm_delete.take() else {
            return UiTask::None;
        };
        let project_id = confirm.project_id;
        let client = self.client.clone();
        self.engine_op(
            async move { delete_project_flow(&client, project_id).await },
            move |result| EngineOpResult::ProjectDeleted { project_id, result },
        )
    }

    pub fn close_tab(&mut self, tab_id: i64) -> UiTask {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.close_tab_dispatch(tab_id).task
    }

    fn close_tab_dispatch(&mut self, tab_id: i64) -> EngineDispatch {
        let op = self.take_engine_op_id();
        let client = self.client.clone();
        EngineDispatch {
            task: self.engine_op(
                async move { close_tab_by_id(&client, tab_id).await },
                move |result| EngineOpResult::TabClosed { op, tab_id, result },
            ),
            op: Some(op),
        }
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

    /// Every focus change in the UI — strip clicks, sidebar rows, the
    /// cycle/switch keybinds, jump-to-unread, the agent and notification
    /// palettes — funnels through here, so this is the one place that owes
    /// them a reconcile.
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
        // Both mutations above broadcast, so a reconcile would eventually
        // arrive on the feed — but only after the bridge task runs, which
        // is not ordered against the next IPC request the same feed
        // carries. Reconciling here keeps "the UI mutated it, the UI shows
        // it" synchronous, exactly as the client-op sites do.
        self.reconcile();
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
            Self::BeginRenameProject(project_id) => return app.begin_rename_project(project_id),
            Self::AgentSelected(tab_id) => app.select_agent(tab_id),
            Self::TabSelected(tab_id) => app.select_tab(tab_id),
            Self::BeginRenameTab(tab_id) => return app.begin_rename_tab(tab_id),
            Self::CloseTab(tab_id) => return app.close_tab(tab_id),
            Self::NewTab => return app.new_tab(),
            Self::NewProject => return app.new_project(),
            Self::ToggleSidebar => app.toggle_sidebar(),
            Self::ConfirmDeleteCancel => app.cancel_confirm_delete(),
            Self::ConfirmDeleteConfirm => return app.execute_confirmed_delete(),
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
        assert_eq!(terminal_grid(size, 220.0, metrics), (2, 2));
    }

    #[test]
    fn collapsed_sidebar_has_no_layout_width() {
        assert_eq!(effective_sidebar_width(false, 220.0), 220.0);
        assert_eq!(effective_sidebar_width(false, 340.0), 340.0);
        assert_eq!(effective_sidebar_width(true, 340.0), 0.0);
    }

    #[test]
    fn a_drag_width_that_lands_after_a_collapse_is_ignored() {
        // The grip is gone once collapsed, so the queued width is stale: it
        // must not overlay the engine value the sidebar shows on expand.
        assert!(!drag_width_is_actionable(true, 220.0, 320.0));
        assert_eq!(effective_sidebar_width(false, 220.0), 220.0);

        assert!(drag_width_is_actionable(false, 220.0, 320.0));
        assert!(!drag_width_is_actionable(false, 320.0, 320.0));
    }

    #[test]
    fn a_wider_sidebar_leaves_the_terminal_fewer_columns() {
        let size = Size::new(1100.0, 720.0);
        let metrics = TerminalMetrics::measure(13.0).expect("test metrics");
        let (default_cols, default_rows) = terminal_grid(size, 220.0, metrics);
        let (wide_cols, wide_rows) = terminal_grid(size, 300.0, metrics);
        assert!(
            wide_cols < default_cols,
            "300px sidebar must yield fewer columns than 220px: {wide_cols} vs {default_cols}"
        );
        assert_eq!(wide_rows, default_rows, "sidebar width must not touch rows");
    }

    #[test]
    fn status_banner_replaces_clears_and_expires_deterministically() {
        let now = Instant::now();
        let mut status = StatusBanner::default();
        assert!(
            !status.is_active(),
            "an app with no banner arms no expiry timer"
        );
        status.set_at("first", now);
        assert_eq!(status.message(), Some("first"));
        assert!(status.is_active());
        status.set_at("replacement", now + Duration::from_secs(1));
        assert_eq!(status.message(), Some("replacement"));
        status.expire_at(now + Duration::from_secs(1));
        assert!(
            status.is_active(),
            "the timer stays armed until the banner's own deadline"
        );
        status.expire_at(now + Duration::from_secs(1) + STATUS_BANNER_DURATION);
        assert_eq!(status.message(), None);
        assert!(!status.is_active(), "expiry disarms the timer");

        status.set_at("clear me", now);
        status.clear();
        assert_eq!(status.message(), None);
        assert!(!status.is_active());
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

    /// The tolerated outcomes are the whole reason completions reconcile:
    /// both of them are answered by the engine *before* it commits
    /// anything, so they broadcast no workspace event and nothing else
    /// would ever tell the UI to look again. They must therefore stay
    /// silent (the user got the state they asked for) while
    /// `engine_op_completed` reconciles unconditionally around this
    /// verdict.
    #[test]
    fn a_tolerated_already_gone_completion_raises_no_banner() {
        for result in [
            EngineOpResult::TabClosed {
                op: 1,
                tab_id: 7,
                result: Ok(CloseTabOutcome::AlreadyGone),
            },
            EngineOpResult::TabClosed {
                op: 1,
                tab_id: 7,
                result: Ok(CloseTabOutcome::Closed),
            },
            EngineOpResult::ProjectDeleted {
                project_id: 3,
                result: Ok(DeleteProjectOutcome::AlreadyGone),
            },
            EngineOpResult::ProjectDeleted {
                project_id: 3,
                result: Ok(DeleteProjectOutcome::Deleted),
            },
        ] {
            assert_eq!(
                engine_op_status(result.clone()),
                None,
                "{result:?} is a success, banner-wise"
            );
        }
    }

    #[test]
    fn a_failed_completion_surfaces_the_engine_error_as_the_banner() {
        assert_eq!(
            engine_op_status(EngineOpResult::TabClosed {
                op: 1,
                tab_id: 7,
                result: Err("close exploded".into()),
            }),
            Some("close exploded".to_string())
        );
        assert_eq!(
            engine_op_status(EngineOpResult::ProjectDeleted {
                project_id: 3,
                result: Err("delete exploded".into()),
            }),
            Some("delete exploded".to_string())
        );
    }

    /// Every dispatch owes the UI exactly one completion. A panicking op
    /// would otherwise leave the mutation with no reconcile and no banner
    /// — the stall this whole shape exists to make impossible — so the
    /// join failure is folded into the op's own error. (The panic message
    /// tokio prints here is expected test output.)
    #[test]
    fn an_engine_op_completes_through_its_own_result_even_when_the_task_panics() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let completed = runtime.block_on(spawn_engine_op(
            runtime.handle().clone(),
            async { Ok(CloseTabOutcome::Closed) },
            |result| EngineOpResult::TabClosed {
                op: 1,
                tab_id: 7,
                result,
            },
        ));
        assert!(matches!(
            completed,
            EngineOpResult::TabClosed {
                op: 1,
                tab_id: 7,
                result: Ok(CloseTabOutcome::Closed)
            }
        ));

        let panicked = runtime.block_on(spawn_engine_op(
            runtime.handle().clone(),
            async { panic!("engine op panicked") },
            |result: Result<DeleteProjectOutcome, String>| EngineOpResult::ProjectDeleted {
                project_id: 3,
                result,
            },
        ));
        let EngineOpResult::ProjectDeleted { project_id, result } = panicked else {
            panic!("a delete's join failure must stay a delete completion")
        };
        assert_eq!(project_id, 3);
        assert!(result.is_err(), "a lost task is that op's own error");
        assert!(engine_op_status(EngineOpResult::ProjectDeleted { project_id, result }).is_some());
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
            runtime
                .block_on(close_tab_by_id(&client, doomed.id))
                .unwrap(),
            CloseTabOutcome::Closed
        );
        assert_eq!(workspace.active(), (project.id, sibling.id));

        // A queued click message retains the ID rendered into it. If another
        // actor already removed that tab, replaying the stale message is an
        // expected no-op and cannot close the newly-active sibling.
        assert_eq!(
            runtime
                .block_on(close_tab_by_id(&client, doomed.id))
                .unwrap(),
            CloseTabOutcome::AlreadyGone
        );
        assert!(workspace.tab(sibling.id).is_ok());

        let last_project = workspace.create_project("last", "/tmp").unwrap();
        let last = workspace.open_tab(last_project.id, "/tmp", "last").unwrap();
        workspace.focus_tab(last.id).unwrap();
        assert_eq!(
            runtime.block_on(close_tab_by_id(&client, last.id)).unwrap(),
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

        let (project_id, tab_id) = runtime.block_on(create_project_flow(&client)).unwrap();

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

        // Close the spawned PTY before the runtime drops: Runtime::drop
        // joins its blocking tasks, and a live shell parks the reader loop
        // in a blocking read forever (deterministic hang on loaded Linux
        // runners; macOS only won the race).
        runtime.block_on(client.delete_project(project_id)).unwrap();
        assert!(!client.supervisor.has(tab_id));
    }

    /// The compound op's two calls are sequential inside one future, so a
    /// failure at the second one happens with the first already
    /// committed. It must surface as the op's error rather than as a
    /// silent half-create — the completion's reconcile then shows
    /// whatever the engine's own rollback left (here: none of it, because
    /// rolling the seed tab back closes the project it was the last tab
    /// of).
    #[test]
    fn create_project_reports_a_failure_that_lands_after_the_project_committed() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let workspace = Arc::new(Workspace::new());
        let supervisor = Arc::new(PtySupervisor::new());
        // Ids are one sequential counter, so the flow's project takes the
        // next one and its seed tab the one after. Squatting on that id
        // makes the seed tab's PTY spawn fail for real (`DuplicateTab`)
        // with the project already in the workspace.
        let probe = workspace.create_project("probe", "/tmp").unwrap();
        let doomed_tab_id = probe.id + 2;
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            "/tmp/roost-iced-create-project-midfail-test.sock".into(),
        );
        runtime
            .block_on(async {
                supervisor.spawn(
                    doomed_tab_id,
                    "/tmp",
                    &["/bin/sh".to_string(), "-c".into(), "cat".into()],
                    DEFAULT_COLS,
                    DEFAULT_ROWS,
                    std::path::Path::new("/tmp/roost-iced-create-project-midfail-test.sock"),
                )
            })
            .expect("occupy the id the seed tab will be allocated");

        let error = runtime
            .block_on(create_project_flow(&client))
            .expect_err("the seed tab cannot spawn");

        assert!(
            engine_op_status(EngineOpResult::ProjectCreated {
                op: 1,
                result: Err(error),
            })
            .is_some(),
            "a mid-flow failure owes the user a banner"
        );
        assert_eq!(
            workspace
                .snapshot()
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>(),
            vec![probe.id],
            "the engine rolled the seed tab back, and with it the project it was the last tab of"
        );

        supervisor.close(doomed_tab_id);
    }

    #[test]
    fn an_opened_tab_is_silent_and_a_failed_open_is_the_banner() {
        assert_eq!(
            engine_op_status(EngineOpResult::TabOpened {
                op: 1,
                project_id: 3,
                result: Ok(9),
            }),
            None
        );
        assert_eq!(
            engine_op_status(EngineOpResult::TabOpened {
                op: 1,
                project_id: 3,
                result: Err("spawn shell failed".into()),
            }),
            Some("spawn shell failed".to_string())
        );
        assert_eq!(
            engine_op_status(EngineOpResult::ProjectCreated {
                op: 2,
                result: Ok((3, 9)),
            }),
            None
        );
        assert_eq!(
            engine_op_status(EngineOpResult::ProjectCreated {
                op: 2,
                result: Err("create exploded".into()),
            }),
            Some("create exploded".to_string())
        );
    }

    /// Only the rows that became asynchronous can have a client parked on
    /// them. Every other completion must answer `None` rather than probe
    /// the stash — a delete reaches the palette through the confirm
    /// overlay, which replies the moment it opens, and renames/reorders
    /// have no palette row at all.
    #[test]
    fn only_the_async_palette_rows_completions_can_owe_a_reply() {
        assert_eq!(
            EngineOpResult::TabClosed {
                op: 4,
                tab_id: 7,
                result: Ok(CloseTabOutcome::Closed),
            }
            .palette_op(),
            Some(4)
        );
        assert_eq!(
            EngineOpResult::TabOpened {
                op: 5,
                project_id: 3,
                result: Ok(9),
            }
            .palette_op(),
            Some(5)
        );
        assert_eq!(
            EngineOpResult::ProjectCreated {
                op: 6,
                result: Ok((3, 9)),
            }
            .palette_op(),
            Some(6)
        );
        assert_eq!(
            EngineOpResult::ProjectDeleted {
                project_id: 3,
                result: Ok(DeleteProjectOutcome::Deleted),
            }
            .palette_op(),
            None
        );
        assert_eq!(
            EngineOpResult::Renamed {
                op: 7,
                target: RenameTarget::Tab(9),
                result: Ok(()),
            }
            .palette_op(),
            None
        );
        assert_eq!(
            EngineOpResult::TabsReordered {
                op: 8,
                project_id: 3,
                ordered_ids: vec![9],
                result: Ok(()),
            }
            .palette_op(),
            None
        );
        assert_eq!(
            EngineOpResult::ProjectsReordered {
                op: 9,
                ordered_ids: vec![3],
                result: Ok(()),
            }
            .palette_op(),
            None
        );
    }

    /// `palette.activate` replies with the state its action produced. For
    /// a row that dismissed the palette and dispatched an op, that state
    /// is the closed one — and it is built at completion time from the
    /// contract, not read off whatever palette exists by then.
    #[test]
    fn a_deferred_activation_answers_with_the_closed_state_its_row_produced() {
        let mut pending = HashMap::new();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        pending.insert(11, tx);

        settle_palette_activation(&mut pending, 11, None);

        assert_eq!(rx.try_recv(), Ok(Ok(PaletteStateResult::default())));
        assert!(pending.is_empty(), "a settled reply is not held twice");
    }

    /// The one palette request allowed to fail keeps failing: an async
    /// row's engine error reaches the blocked client as the operation
    /// error, exactly as it did when the row blocked the UI thread.
    #[test]
    fn a_deferred_activation_answers_a_failed_row_with_its_operation_error() {
        let mut pending = HashMap::new();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        pending.insert(11, tx);

        settle_palette_activation(&mut pending, 11, Some("close exploded".into()));

        assert_eq!(rx.try_recv(), Ok(Err("close exploded".to_string())));
    }

    /// Two clients, two invocations, two ids: each hears its own row's
    /// outcome, and neither completion can answer the other's client.
    #[test]
    fn concurrent_activations_each_answer_their_own_client() {
        let mut pending = HashMap::new();
        let (first_tx, mut first_rx) = tokio::sync::oneshot::channel();
        let (second_tx, mut second_rx) = tokio::sync::oneshot::channel();
        pending.insert(20, first_tx);
        pending.insert(21, second_tx);

        settle_palette_activation(&mut pending, 21, Some("open exploded".into()));

        assert_eq!(second_rx.try_recv(), Ok(Err("open exploded".to_string())));
        assert_eq!(
            first_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "the other invocation is still waiting on its own op"
        );

        settle_palette_activation(&mut pending, 20, None);
        assert_eq!(first_rx.try_recv(), Ok(Ok(PaletteStateResult::default())));
    }

    /// The stash is keyed by op id and nothing else, so the palette's own
    /// life cannot invalidate a reply: the client blocked on this
    /// invocation is answered even though the palette it activated is
    /// long gone and a different one is open in its place.
    #[test]
    fn a_deferred_reply_is_sent_even_after_the_palette_reopened() {
        let mut pending = HashMap::new();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        pending.insert(30, tx);

        // What `palette_state_result()` would answer by completion time:
        // somebody else's palette, which this reply must never carry.
        let reopened = PaletteStateResult {
            open: true,
            frame: Some("launcher".into()),
            ..PaletteStateResult::default()
        };

        settle_palette_activation(&mut pending, 30, None);

        let delivered = rx.try_recv().expect("the client is still owed an answer");
        assert_eq!(delivered, Ok(PaletteStateResult::default()));
        assert_ne!(delivered, Ok(reopened));
    }

    /// `palette.present`'s reply is a different promise on a different
    /// field, fulfilled by the user's pick or dismiss. Settling an
    /// activation must not consume it — a present left waiting is a
    /// client hung until the app exits.
    #[test]
    fn settling_an_activation_leaves_the_present_reply_untouched() {
        let mut pending = HashMap::new();
        let (activate_tx, mut activate_rx) = tokio::sync::oneshot::channel();
        pending.insert(40, activate_tx);
        let (present_tx, mut present_rx) =
            tokio::sync::oneshot::channel::<Result<PalettePresentResult, String>>();
        let present_reply = Some(present_tx);

        settle_palette_activation(&mut pending, 40, None);

        assert!(activate_rx.try_recv().is_ok());
        assert_eq!(
            present_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "the present is still waiting for the user"
        );
        assert!(
            present_reply.is_some(),
            "and its sender is still held for them"
        );
    }

    /// A completion whose invocation was never an IPC one — the keybind,
    /// the button, the pointer — finds nothing stashed and settles
    /// nothing. The op id still exists; only the reply is optional.
    #[test]
    fn a_non_ipc_activation_settles_no_reply() {
        let mut pending: HashMap<u64, PaletteActivateReply> = HashMap::new();
        settle_palette_activation(&mut pending, 50, None);
        assert!(pending.is_empty());
    }

    #[test]
    fn keyboard_route_requires_a_live_terminal_and_gives_editor_precedence() {
        assert_eq!(
            resolve_keyboard_route(false, false, false, 7, false),
            KeyboardRoute::None
        );
        assert_eq!(
            resolve_keyboard_route(false, false, false, 7, true),
            KeyboardRoute::Terminal(7)
        );
        assert_eq!(
            resolve_keyboard_route(false, false, true, 7, true),
            KeyboardRoute::Palette
        );
        assert_eq!(
            resolve_keyboard_route(false, true, true, 7, true),
            KeyboardRoute::Editor
        );
        // An open confirm outranks every other surface, so no keystroke can
        // reach an accelerator or the active PTY while it is up.
        assert_eq!(
            resolve_keyboard_route(true, true, true, 7, true),
            KeyboardRoute::Confirm
        );
        assert_eq!(
            resolve_keyboard_route(true, false, false, 7, true),
            KeyboardRoute::Confirm
        );
    }

    #[test]
    fn confirm_delete_targets_only_projects_present_in_the_snapshot() {
        let workspace = Workspace::new();
        let project = workspace.create_project("doomed", "/tmp").unwrap();
        let snapshot = workspace.snapshot();

        assert_eq!(
            confirm_delete_target(&snapshot, project.id),
            Some(ConfirmDeleteProject {
                project_id: project.id,
                name: "doomed".into()
            })
        );
        assert_eq!(confirm_delete_target(&snapshot, 0), None);
        assert_eq!(confirm_delete_target(&snapshot, project.id + 1), None);
    }

    #[test]
    fn a_confirm_whose_project_vanished_externally_is_auto_dismissed() {
        let workspace = Workspace::new();
        let project = workspace.create_project("doomed", "/tmp").unwrap();
        let snapshot = workspace.snapshot();
        let mut confirm = confirm_delete_target(&snapshot, project.id);

        reconcile_confirm_delete(&mut confirm, &snapshot);
        assert!(confirm.is_some(), "a live project keeps its confirm open");

        workspace.rename_project(project.id, "relabeled").unwrap();
        reconcile_confirm_delete(&mut confirm, &workspace.snapshot());
        assert_eq!(
            confirm.as_ref().map(|confirm| confirm.name.as_str()),
            Some("relabeled"),
            "an external rename must relabel the open confirm"
        );

        workspace.delete_project(project.id).unwrap();
        reconcile_confirm_delete(&mut confirm, &workspace.snapshot());
        assert_eq!(confirm, None);
    }

    #[test]
    fn confirm_dialog_answers_only_a_fresh_enter_or_escape_press() {
        use iced::keyboard::key::{Code, Physical};
        use iced::keyboard::Location;

        let press = |key: Key, code, repeat| keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::default(),
            text: None,
            repeat,
        };
        let release = |key: Key, code| keyboard::Event::KeyReleased {
            modified_key: key.clone(),
            key,
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::default(),
        };

        assert_eq!(
            confirm_dialog_action(&press(Key::Named(Named::Enter), Code::Enter, false)),
            Some(ConfirmDialogAction::Delete)
        );
        assert_eq!(
            confirm_dialog_action(&press(Key::Named(Named::Escape), Code::Escape, false)),
            Some(ConfirmDialogAction::Cancel)
        );
        assert_eq!(
            confirm_dialog_action(&press(Key::Named(Named::Enter), Code::Enter, true)),
            None
        );
        assert_eq!(
            confirm_dialog_action(&release(Key::Named(Named::Enter), Code::Enter)),
            None
        );
        assert_eq!(
            confirm_dialog_action(&press(Key::Character("a".into()), Code::KeyA, false)),
            None
        );

        // Answering arms the same latch the rename editor uses, so the held
        // repeats and the release of that key never reach the PTY.
        let mut pending = Some(RenameCompletionKey::Enter);
        assert!(consume_rename_completion_key(
            &mut pending,
            &press(Key::Named(Named::Enter), Code::Enter, true)
        ));
        assert!(consume_rename_completion_key(
            &mut pending,
            &release(Key::Named(Named::Enter), Code::Enter)
        ));
        assert_eq!(pending, None);

        let mut pending = Some(RenameCompletionKey::Escape);
        assert!(consume_rename_completion_key(
            &mut pending,
            &press(Key::Named(Named::Escape), Code::Escape, true)
        ));
        assert!(consume_rename_completion_key(
            &mut pending,
            &release(Key::Named(Named::Escape), Code::Escape)
        ));
        assert_eq!(pending, None);
    }

    #[test]
    fn confirmed_delete_cascades_tabs_and_ptys_with_the_engine_id_fallback() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let workspace = Arc::new(Workspace::new());
        let supervisor = Arc::new(PtySupervisor::new());
        let client = LocalClient::new(
            Arc::clone(&workspace),
            Arc::clone(&supervisor),
            "/tmp/roost-iced-delete-project-test.sock".into(),
        );
        let open = |project_id| {
            runtime
                .block_on(client.open_tab(
                    project_id,
                    "/tmp",
                    "",
                    &[],
                    u32::from(DEFAULT_COLS),
                    u32::from(DEFAULT_ROWS),
                ))
                .unwrap()
        };
        let keeper = workspace.create_project("keeper", "/tmp").unwrap();
        let keeper_tab = open(keeper.id);
        let doomed = workspace.create_project("doomed", "/tmp").unwrap();
        let doomed_first = open(doomed.id);
        let doomed_second = open(doomed.id);
        let last_row = workspace.create_project("last row", "/tmp").unwrap();
        let last_row_tab = open(last_row.id);
        // The engine falls back to the lowest remaining project id, not the
        // first sidebar row — pin that after a reorder puts them at odds.
        workspace
            .reorder_projects(&[last_row.id, doomed.id, keeper.id])
            .unwrap();
        workspace.focus_tab(doomed_first.id).unwrap();

        assert_eq!(
            runtime
                .block_on(delete_project_flow(&client, doomed.id))
                .unwrap(),
            DeleteProjectOutcome::Deleted
        );
        assert!(workspace
            .snapshot()
            .iter()
            .all(|project| project.id != doomed.id));
        assert!(workspace.tab(doomed_first.id).is_err());
        assert!(workspace.tab(doomed_second.id).is_err());
        assert!(!supervisor.has(doomed_first.id));
        assert!(!supervisor.has(doomed_second.id));
        assert!(supervisor.has(keeper_tab.id));
        assert_eq!(workspace.active(), (keeper.id, keeper_tab.id));

        // A stale confirm settles as a silent dismiss, never as an error.
        assert_eq!(
            runtime
                .block_on(delete_project_flow(&client, doomed.id))
                .unwrap(),
            DeleteProjectOutcome::AlreadyGone
        );

        assert_eq!(
            runtime
                .block_on(delete_project_flow(&client, keeper.id))
                .unwrap(),
            DeleteProjectOutcome::Deleted
        );
        assert_eq!(
            runtime
                .block_on(delete_project_flow(&client, last_row.id))
                .unwrap(),
            DeleteProjectOutcome::Deleted
        );
        assert!(workspace.snapshot().is_empty());
        assert_eq!(workspace.active(), (0, 0));
        assert!(!supervisor.has(keeper_tab.id));
        assert!(!supervisor.has(last_row_tab.id));
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
