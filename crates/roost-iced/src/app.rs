use std::borrow::Cow;
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
    button, column, container, image, mouse_area, row, scrollable, stack, text, text_input, Space,
};
use iced::{font, window, Alignment, Color, Element, Fill, Font, Shrink, Size};
use roost_engine::git_metrics;
use roost_engine::ipc::{
    ClipboardOp, DumpData, ExpandSelectionData, IpcHandler, ResolvedCellData, ResolvedCellsData,
    SelectionData, UiRequest,
};
use roost_engine::osc::{ClipboardTarget, OscAction, OscColorSnapshot};
use roost_engine::pointer::{DragCellGate, MotionEmitter, PointerAction, PointerButton};
use roost_engine::process::{self, ProcessRequest};
use roost_engine::session::{InputCapture, TabOutput, TabSession};
use roost_engine::single_instance::InstanceLocks;
use roost_engine::{
    LocalClient, PtySupervisor, RestoreTab, Workspace, WorkspaceError, WorkspaceEvent,
};
use roost_ipc::agent;
use roost_ipc::messages::{
    AppMenuDumpResult, AppNotificationStatusResult, AppRenderStatsResult, AppUpdateStatusResult,
    PaletteItemView, PalettePresentResult, PaletteStateResult, Project, SidebarDumpAgentRow,
    SidebarDumpProject, SidebarDumpResult, WindowMetricsResult,
};
use roost_ipc::paths::{BundleProfile, BundleProfileKind};
use roost_ipc::IpcServer;
use roost_ui_model::theme::Theme;
use roost_ui_model::typography::{self, FamilyApply, TerminalTypography};
use roost_ui_model::{
    agent_palette,
    config::{self, RoostConfig},
    custom_command,
    keybind::{self, Accel, AccelMods, KeybindAction},
    keys::{ProjectKey, TabKey},
    notification_inbox, palette, provider,
    rollup::project_rollup,
    window_title,
};
use roost_url::HoverUrl;
use roost_vt::{
    key_action, mouse_action, mouse_button, ColorRgb, KeyEncoder, KeyEvent, MouseEncoder,
    MouseEvent, PageDirection, PageRoute, RenderState, ScrollDirection, ScrollRoute, Terminal,
    TerminalOptions, TerminalScroll, TerminalSelection,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::engine_feed::{self, EngineBatch, EngineFeed, EngineFeedReceiver, EngineFeedSender};
use crate::font_registry::{system_font_registry, FontRegistry};
use crate::notifications::DesktopNotifications;
use crate::palette_scroll::Visibility;
use crate::sidebar_resize::SidebarResizeGrip;
use crate::strip_reorder::{ReorderStrip, StripEvent};
use crate::terminal_widget::{
    DrawCell, ImePreedit, RenderedRow, TerminalMetrics, TerminalPointerEvent, TerminalSnapshot,
    TerminalWheelEvent, TerminalWidget, TERMINAL_PADDING,
};
use crate::Message;
use crate::{chrome, input};

// `mod palette` would collide with the `roost_ui_model::palette` import in
// this module's namespace, so the palette-overlay half of App lives in
// `palettes` (it hosts the command/agent/provider/notification palettes).
pub(crate) mod host_tab;
mod interactions;
mod palettes;
mod servicing;
mod tab_backend;
mod terminal_tab;
// The in-crate `#[ignore]`d perf harness — see `tools/perf/README.md` for
// how to run it. Gated on `cfg(test)` like `terminal_tab`'s test-only
// `attach_test_terminal` fixture it depends on; it carries no production
// code, so the whole module (not just an inner `mod tests`) is test-only.
#[cfg(test)]
mod perf_bench;

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
use self::tab_backend::{TabBackend, TabHandle};
use self::terminal_tab::{
    apply_geometry_batch, clear_preedit_or_warn, pointer_origin_tab, refresh_or_warn,
    terminal_grid, GeometryBatchOperation, NativePointerDispatch, TerminalTab,
};
#[cfg(test)]
use self::terminal_tab::{
    attach_test_terminal, feed_text_until, GeometryBatchFailure, GeometryChange,
    LocalPointerGesture, NativePointerOutcome, TerminalGeometry,
};

const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 32;
const STATUS_BANNER_DURATION: Duration = Duration::from_secs(5);
/// How often the banner's expiry is checked while one is up. Coarse
/// against the five-second life it polices — the banner is allowed to
/// outlive its deadline by up to one of these.
pub(crate) const STATUS_TICK_INTERVAL: Duration = Duration::from_millis(500);
const CONFIRM_PANEL_WIDTH: f32 = 420.0;
/// The inline tab-rename field's width. It stands in for the measured
/// title width while a pill is being renamed, so an editing pill sizes by
/// the same rule as every other one.
const RENAME_FIELD_WIDTH: f32 = 140.0;

/// The tab pill the strip reveal scrolls to. Keyed by the tab alone: the
/// pill for a tab is one container wherever the strip reorders it to, and
/// a reveal issued for a tab that has since closed simply finds nothing.
fn tab_pill_id(tab: TabKey) -> Id {
    Id::from(format!("tab-pill:{tab}"))
}

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
    project: ProjectKey,
    name: String,
    tab_count: usize,
}

fn confirm_delete_target(
    projects: &[Project],
    project: ProjectKey,
) -> Option<ConfirmDeleteProject> {
    // The snapshot is the local workspace's, so only a local key can name
    // a row in it.
    let project_id = project.local_project()?;
    projects
        .iter()
        .find(|candidate| candidate.id == project_id)
        .map(|candidate| ConfirmDeleteProject {
            project,
            name: candidate.name.clone(),
            tab_count: candidate.tabs.len(),
        })
}

fn reconcile_confirm_delete(confirm: &mut Option<ConfirmDeleteProject>, projects: &[Project]) {
    if let Some(open) = confirm.as_ref() {
        // Re-resolve rather than only checking liveness: an external
        // rename while the dialog is open must not leave the user
        // approving a deletion under a stale label. The tab count rides
        // the same re-resolve — the Mac snapshots it when the alert opens
        // (App.swift:3145-3182), but its alert is modal so nothing can
        // close a tab underneath it; ours stays open across IPC traffic,
        // and quoting a count the project no longer has would be a lie.
        *confirm = confirm_delete_target(projects, open.project);
    }
}

/// What a window-focus transition tears down. A table rather than a
/// straight-line body because the interesting part of the policy is what it
/// leaves ALONE, and `App` needs a bound IPC socket plus a real `state.json`
/// to construct — this is the only seam a unit test can pin it through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct FocusTeardown {
    rename_completion_key: bool,
    drags: bool,
    /// Always false. The delete confirmation is an answer the user still
    /// owes; unfocus dropping it (a click into another app, a screenshot
    /// tool stealing focus) read as the app having crashed. The Mac's
    /// `NSAlert` is application-modal and equally unaffected.
    confirm_delete: bool,
    ime_composition: bool,
    /// Refocus only: macOS discards marked text when the window loses
    /// focus, so a commit arriving after refocus is fresh input (emoji
    /// picker), not residue of the composition the unfocus cancel dropped.
    ime_discard: bool,
}

fn focus_teardown(focused: bool) -> FocusTeardown {
    if focused {
        FocusTeardown {
            ime_discard: true,
            ..FocusTeardown::default()
        }
    } else {
        FocusTeardown {
            rename_completion_key: true,
            drags: true,
            confirm_delete: false,
            ime_composition: true,
            ime_discard: false,
        }
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
        tab: TabKey,
        result: Result<CloseTabOutcome, String>,
    },
    ProjectDeleted {
        project: ProjectKey,
        result: Result<DeleteProjectOutcome, String>,
    },
    /// One tab opened: the new-tab routes and the launcher's rows. `op`
    /// keys the deferred palette reply exactly as [`Self::TabClosed`]'s
    /// does.
    ///
    /// The engine mints the new id in its own id-space; the dispatch
    /// qualifies it at the backend it dispatched to, which is the only
    /// thing that knows which one that was.
    TabOpened {
        op: u64,
        project: ProjectKey,
        result: Result<TabKey, String>,
    },
    /// A project and its first tab, from the one compound op that
    /// creates both.
    ProjectCreated {
        op: u64,
        result: Result<(ProjectKey, TabKey), String>,
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
    ///
    /// Bare, deliberately: `ordered_ids` is the order this op HANDED the
    /// engine, echoed back so the preview can tell a superseded reorder
    /// from a current one. It is a wire payload round-tripping, not
    /// routing state — and it is already scoped by `project_id`, which
    /// only the strip that dispatched it can have produced.
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
        EngineOpResult::TabClosed { tab, result, .. } => match result {
            Ok(CloseTabOutcome::Closed) => None,
            Ok(CloseTabOutcome::AlreadyGone) => {
                tracing::debug!(?tab, "close_tab: rendered tab already gone");
                None
            }
            Err(error) => {
                tracing::warn!(%error, ?tab, "close_tab failed");
                Some(error)
            }
        },
        EngineOpResult::ProjectDeleted { project, result } => match result {
            Ok(DeleteProjectOutcome::Deleted) => None,
            Ok(DeleteProjectOutcome::AlreadyGone) => {
                tracing::debug!(?project, "confirmed delete: project already gone");
                None
            }
            Err(error) => {
                tracing::warn!(%error, ?project, "delete project failed");
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
            project, result, ..
        } => match result {
            Ok(tab) => {
                tracing::debug!(?project, ?tab, "opened tab");
                None
            }
            Err(error) => {
                tracing::warn!(%error, ?project, "open tab failed");
                Some(error)
            }
        },
        EngineOpResult::ProjectCreated { result, .. } => match result {
            Ok((project, tab)) => {
                tracing::debug!(?project, ?tab, "created project");
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

/// Closing the sidebar from a drag past the collapse threshold is the one
/// collapse that must *not* commit the live drag: the widths that gesture
/// published all sat at the clamp floor on its way down, so committing would
/// remember 160 where the user had, say, 300. Dropping the drag leaves the
/// persisted width untouched and a reopen restores the pre-drag sidebar.
fn drop_drag_and_collapse(drag_width: &mut Option<f32>, workspace: &Workspace) {
    *drag_width = None;
    workspace.set_sidebar_collapsed(true);
}

/// The core half of every focus change: the two ops the UI owes the
/// workspace, in order. `focus_tab`'s error is also the "is this tab still
/// there?" guard — a route holding only a `&Workspace` (the notification
/// banner's click, which arrives with no `&mut App` in hand) gets both from
/// this one call.
fn focus_tab_in_core(workspace: &Workspace, tab: TabKey) -> Result<(), String> {
    // The local workspace owns only the local id-space; another
    // instance's tab is not this workspace's to focus, and applying its
    // number here would jump to whatever local tab shares it.
    let tab_id = tab.local_tab().ok_or_else(|| {
        format!("tab {tab:?} belongs to another instance; the local workspace cannot focus it")
    })?;
    workspace
        .focus_tab(tab_id)
        .map_err(|error| error.to_string())?;
    workspace
        .set_tab_has_notification(tab_id, false)
        .map_err(|error| error.to_string())
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
    /// The terminal that owns the keyboard, host-qualified so a
    /// composition or a keystroke can never be delivered to another
    /// instance's tab of the same number.
    Terminal(TabKey),
}

/// The tab a preedit update belongs to. Only a terminal that owns the
/// keyboard may hold a composition — every other surface has its own
/// `TextInput`, which requests its own IME.
fn ime_preedit_target(route: KeyboardRoute) -> Option<TabKey> {
    match route {
        KeyboardRoute::Terminal(tab) => Some(tab),
        KeyboardRoute::None
        | KeyboardRoute::Confirm
        | KeyboardRoute::Editor
        | KeyboardRoute::Palette => None,
    }
}

/// The tab a commit belongs to. The tab holding the composition wins over
/// the current route, so a commit that races a tab switch still lands
/// where the user was typing.
fn ime_commit_target(preedit_holder: Option<TabKey>, route: KeyboardRoute) -> Option<TabKey> {
    preedit_holder.or_else(|| ime_preedit_target(route))
}

/// One-shot latch that drops the commit the OS re-offers after a cancel
/// already discarded its marked text — without it that stray commit would
/// fall through to whichever terminal owns the route by then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ImeDiscard(bool);

impl ImeDiscard {
    fn arm(&mut self, discarded_live_composition: bool) {
        self.0 |= discarded_live_composition;
    }

    fn disarm(&mut self) {
        self.0 = false;
    }

    /// Whether this commit belongs to a composition already discarded, and
    /// must therefore be dropped. Consumes the latch either way.
    fn claims_commit(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }
}

/// Cancel every composition except `keep`'s. Always a cancel, never a
/// commit: losing focus or handing the keyboard to another surface must
/// not type text the user never confirmed.
///
/// This drops Roost's side only. The OS keeps its own composition — the
/// terminal is still the keyboard route, so the IME stays enabled and the
/// candidate window can linger until the user acts on it. `discard` is
/// what keeps that residue off the PTY. Abandoning it OS-side would mean
/// pulsing `InputMethod::Disabled` for a frame, which needs verification
/// against a real IME before it goes in.
fn cancel_preedits(
    tabs: &mut HashMap<TabKey, TerminalTab>,
    discard: &mut ImeDiscard,
    keep: Option<TabKey>,
) {
    let mut cancelled = false;
    for (key, tab) in tabs.iter_mut() {
        if Some(*key) == keep {
            continue;
        }
        cancelled |= clear_preedit_or_warn(key.tab, tab);
    }
    discard.arm(cancelled);
}

fn set_preedit_in(
    tabs: &mut HashMap<TabKey, TerminalTab>,
    discard: &mut ImeDiscard,
    route: KeyboardRoute,
    text: String,
    cursor: Option<Range<usize>>,
) {
    let Some(key) = ime_preedit_target(route) else {
        return;
    };
    let Some(tab) = tabs.get_mut(&key) else {
        return;
    };
    let tab_id = key.tab;
    // Only a fresh composition disarms the latch. An empty preedit is the
    // clear winit sends immediately before every commit, so treating that
    // as a new composition would defeat the discard.
    let started = !text.is_empty();
    if let Err(error) = tab.set_preedit(text, cursor) {
        tracing::warn!(?error, tab_id, "terminal preedit update failed");
    }
    if started {
        discard.disarm();
    }
}

fn commit_ime_in(
    tabs: &mut HashMap<TabKey, TerminalTab>,
    discard: &mut ImeDiscard,
    route: KeyboardRoute,
    text: &str,
) {
    if discard.claims_commit() {
        return;
    }
    // The holder's WHOLE key travels to the lookup: reducing it to a
    // number here and re-qualifying it below would hand the commit to
    // whichever instance the route happens to name.
    let holder = tabs
        .iter()
        .find(|(_, tab)| tab.preedit.is_some())
        .map(|(key, _)| *key);
    let Some(key) = ime_commit_target(holder, route) else {
        return;
    };
    let Some(tab) = tabs.get_mut(&key) else {
        return;
    };
    if let Err(error) = tab.commit_ime(text) {
        tracing::warn!(?error, tab_id = key.tab, "terminal IME commit failed");
    }
}

/// Whether the terminal for `active_tab` should ask the platform for an
/// input method: it owns the keyboard and the window has focus.
fn terminal_ime_active(route: KeyboardRoute, active_tab: TabKey, window_focused: bool) -> bool {
    window_focused && ime_preedit_target(route) == Some(active_tab)
}

/// Whether the active terminal should render a solid (as opposed to
/// hollow) cursor: the window has focus AND the keyboard route is a
/// terminal — palette/rename/confirm owning the route draws the cursor
/// hollow, mac parity. Independent of the route-only `app.active_terminal_
/// focused` IPC op (`test_focus.py`), which stays unchanged.
fn terminal_cursor_focused(route: KeyboardRoute, window_focused: bool) -> bool {
    window_focused && matches!(route, KeyboardRoute::Terminal(_))
}

fn resolve_keyboard_route(
    confirm_open: bool,
    editor_open: bool,
    palette_open: bool,
    active_tab: TabKey,
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
    /// A paste found no text on the system clipboard — go look for an
    /// image. The read + PNG encode block, so this runs off the UI
    /// thread and reports back as `Message::PasteImageMaterialized`.
    PasteImageProbe {
        tab: TabKey,
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
    /// Scroll the tab strip so the newly activated pill is fully on
    /// screen — the same scroll-into-view operation the palette uses,
    /// walked along the strip's horizontal axis.
    RevealTab {
        scroll_id: Id,
        pill_id: Id,
    },
    /// The workspace has no projects left: end the run loop so `App` is
    /// dropped and its `Drop` flushes state (mac parity — the Swift app
    /// closes its window on the last project's deletion). Deliberately
    /// NOT `iced::exit()` at the point of request: `main` turns this into
    /// one more message round-trip so the deleting IPC client's reply is
    /// written before the loop tears the socket down.
    Exit,
}

/// Where the app is in the exit-on-empty sequence. A plain flag would
/// re-arm on every subsequent `reconcile()` (the workspace stays empty
/// until the process is gone), so the request latches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ExitState {
    #[default]
    Running,
    Requested,
    Dispatched,
}

impl ExitState {
    /// The snapshot side. Returns whether THIS observation raised the
    /// request, so the log line lands on the edge rather than on every
    /// reconcile that follows it.
    fn observe(&mut self, projects_empty: bool) -> bool {
        projects_empty && self.request()
    }

    /// Latch the request — from the empty-workspace observation above, or
    /// directly from the menu's Quit, which has no empty workspace to
    /// observe. Returns whether THIS call raised it, so a second request
    /// while the first is in flight cannot queue a second exit.
    fn request(&mut self) -> bool {
        if *self != Self::Running {
            return false;
        }
        *self = Self::Requested;
        true
    }

    /// The drain side: a raised request is handed out exactly once, so a
    /// second reconcile in the same teardown cannot queue a second exit.
    fn take(&mut self) -> bool {
        if *self != Self::Requested {
            return false;
        }
        *self = Self::Dispatched;
        true
    }
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

/// App-identity name the window title falls back to when no project supplies
/// one.
///
/// Keyed off the resolved profile, not the OS: the macOS Iced bundle and the
/// Linux *dev* iced profile both announce `Roost-Iced` (matching the bundle's
/// `CFBundleName`), while the packaged Linux build resolves `Linux` and keeps
/// the production `Roost` users already see.
/// Pure half of [`App::window_title`]: `project` is the active project's
/// `(name, effective cwd)`, `None` when no project is active. Split out so
/// tests can pin that BOTH branches thread the profile-chosen fallback.
fn compose_window_title(fallback: &str, project: Option<(&str, &str)>, home: &str) -> String {
    match project {
        None => fallback.to_string(),
        Some((name, cwd)) => window_title::window_title_with_fallback(fallback, name, cwd, home),
    }
}

fn title_fallback(kind: BundleProfileKind) -> &'static str {
    match kind {
        BundleProfileKind::Iced => "Roost-Iced",
        BundleProfileKind::Session => "Roost-Session",
        BundleProfileKind::Mac | BundleProfileKind::Linux => window_title::DEFAULT_WINDOW_TITLE,
    }
}

/// One tab pill's elided title, with the inputs it was elided under.
#[derive(Debug, Clone, PartialEq)]
struct PillLabel {
    /// The DISPLAY title — post-`"shell"` substitution, exactly what
    /// `view()` hands the eliding measurer.
    title: String,
    active: bool,
    has_notification: bool,
    label: String,
    width: f32,
}

impl PillLabel {
    /// Whether this entry was elided under exactly `key`. `id` is the map
    /// slot, so only the elision inputs take part.
    fn matches(&self, key: PillKey<'_>) -> bool {
        self.title == key.title
            && self.active == key.active
            && self.has_notification == key.has_notification
    }
}

/// The pill's key as `view()` derives it, for one tab. Carrying it as one
/// value is what keeps the lookup and the populate from disagreeing on the
/// two same-typed flags.
#[derive(Debug, Clone, Copy)]
struct PillKey<'a> {
    tab: TabKey,
    title: &'a str,
    active: bool,
    has_notification: bool,
}

/// What a pill spends beyond its title: the close affordance on the active
/// pill, the badge on a notified inactive one. Both are laid out trailing,
/// so both come out of the title's width budget.
fn pill_trailing_width(active: bool, has_notification: bool) -> f32 {
    if active {
        chrome::PILL_HEIGHT
    } else if has_notification {
        chrome::NOTIFICATION_DOT_SIZE
    } else {
        0.0
    }
}

/// The one place the pill title's font + width budget are derived, so
/// `view()` and the memo populate cannot drift into measuring differently.
fn pill_title_metrics(active: bool, has_notification: bool) -> (Font, f32) {
    let font = chrome::chrome_font(if active {
        font::Weight::Medium
    } else {
        font::Weight::Normal
    });
    let budget = chrome::TAB_PILL_MAX_WIDTH
        - chrome::TAB_PILL_CHROME_WIDTH
        - pill_trailing_width(active, has_notification);
    (font, budget)
}

fn elide_pill_title(key: PillKey<'_>) -> (Cow<'_, str>, f32) {
    let (font, budget) = pill_title_metrics(key.active, key.has_notification);
    chrome::elide_to_width(key.title, font, chrome::TAB_TITLE_SIZE, budget)
}

/// Recompute `labels` for exactly `keys`, skipping every tab whose key is
/// unchanged and dropping every tab that is no longer in the strip.
fn refresh_pill_label_map(labels: &mut HashMap<TabKey, PillLabel>, keys: &[PillKey<'_>]) {
    for &key in keys {
        if labels
            .get(&key.tab)
            .is_some_and(|stored| stored.matches(key))
        {
            continue;
        }
        let (label, width) = elide_pill_title(key);
        labels.insert(
            key.tab,
            PillLabel {
                title: key.title.to_owned(),
                active: key.active,
                has_notification: key.has_notification,
                label: label.into_owned(),
                width,
            },
        );
    }
    labels.retain(|tab, _| keys.iter().any(|key| key.tab == *tab));
}

/// The display string a pill shows for `title` — an untitled tab reads
/// `"shell"`, and that substituted string is what gets elided, cached and
/// keyed on.
fn pill_display_title(title: &str) -> &str {
    if title.is_empty() {
        "shell"
    } else {
        title
    }
}

pub struct App {
    workspace: Arc<Workspace>,
    backend: TabBackend,
    client: LocalClient,
    tabs: HashMap<TabKey, TerminalTab>,
    projects: Vec<Project>,
    sidebar_agents: HashMap<ProjectKey, Vec<agent_palette::SidebarAgentRow>>,
    notification_inbox: notification_inbox::NotificationInbox,
    window_id: Option<window::Id>,
    pending_window_resize: Option<Size>,
    screenshots: ScreenshotQueue,
    window_size: Size,
    /// Set only while the seam is being dragged; the engine holds the
    /// committed width, and persisting per pointer-move would rewrite
    /// `state.json` on every frame.
    sidebar_drag_width: Option<f32>,
    window_focused: bool,
    /// App-identity name the window title falls back to, fixed at bootstrap
    /// from the resolved profile — see [`title_fallback`].
    title_fallback: &'static str,
    ime_discard: ImeDiscard,
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
    /// Elided tab-pill titles, memoized across `view()` rebuilds: eliding
    /// binary-searches over full `Paragraph` shaping, which made the pill
    /// loop 79% of per-keystroke main-thread work (plan 029 F1).
    ///
    /// The memo is sound only while EVERY elision input is in the key. It
    /// is today — the display title, `active` and `has_notification` are
    /// the only variables; the font, text size and width budget are
    /// constants (`pill_title_metrics`). If the budget ever becomes
    /// window-relative, or the chrome font configurable, the key must grow
    /// to match or pills will render at a stale width.
    pill_labels: HashMap<TabKey, PillLabel>,
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
    tab_strip_scroll_id: Id,
    /// The active tab the strip has already been scrolled to. Compared
    /// against the workspace's active tab in `reconcile()`, which is what
    /// makes the reveal fire for every activation route — including raw
    /// IPC, which never touches a UI focus helper.
    revealed_tab: Option<TabKey>,
    tab_reveal_request: Option<TabKey>,
    exit_state: ExitState,
    /// The gating value last pushed to the native menu bar, so the seam is
    /// touched only when the keyboard route actually moved.
    #[cfg(target_os = "macos")]
    menu_gating: crate::macos::menu::MenuGating,
    /// The Window menu's dynamic rows as last built, so a reconcile that
    /// moved no project or tab never touches AppKit.
    #[cfg(target_os = "macos")]
    menu_window_rows: crate::macos::menu::WindowRows,
    /// `SPUUpdater.canCheckForUpdates` as last pushed onto the "Check
    /// for Updates…" item. `None` before the first push, so boot writes
    /// the item's state even when it is already correct.
    #[cfg(target_os = "macos")]
    menu_can_check_updates: Option<bool>,
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
    desktop_notifications: DesktopNotifications,
    /// Connected host sessions and their workspace mirrors (plan 037).
    /// Empty until a host is connected, and inert while it is: with zero
    /// hosts nothing is spawned and no feed item can arrive.
    ///
    /// Sits above `feed_rx`/`runtime` for the drop-order contract below —
    /// each connection owns a runtime task, and its `Drop` signals that
    /// task to wind down, which it can only act on while the runtime it
    /// runs on is still there.
    hosts: crate::host_conn::HostConnSet,
    /// The focused host tab's attach machinery. Attach-on-focus keeps
    /// exactly one entry alive at a time today (plan 037 §3.4) — the map
    /// is keyed like `tabs` so C6/C7 can widen that policy without
    /// reshaping the state. Dropped before the runtime for the same
    /// reason as `hosts`: each attach owns runtime tasks.
    host_attach: HashMap<TabKey, host_tab::HostAttach>,
    /// Resume points of previously attached host tabs: refocus resumes
    /// from here (`mode: "resume"` when the ring covers it). Keyed by
    /// the connection incarnation, so a reconnect naturally orphans the
    /// stale entries and the purge drops them.
    host_resume: HashMap<TabKey, host_tab::ResumePoint>,
    /// A clone of `runtime`'s handle, so a mutation can be spawned onto
    /// the engine runtime from a `&self` method and awaited by an Iced
    /// task. Cheap and `Send`; dropping one is inert, so it takes no part
    /// in the ordering below.
    runtime_handle: tokio::runtime::Handle,
    // Field order is intentional: terminal sessions and the engine feed
    // (whose receiver carries the wake every sender notifies on) are
    // dropped before the runtime — a dropped receiver is how the adapter
    // tasks learn to stop. The locks are held until every runtime task
    // has been cancelled and joined by Runtime::drop, so they stay last.
    // `InstanceLocks` owns the release *order* between the two locks
    // (state before socket, the reverse of acquisition) in its own field
    // order — one field here, so no ordering trap at this seam.
    feed_rx: EngineFeedReceiver,
    feed_tx: EngineFeedSender,
    runtime: tokio::runtime::Runtime,
    _locks: InstanceLocks,
}

impl App {
    pub fn bootstrap(profile: &BundleProfile, locks: InstanceLocks) -> Result<Self> {
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
        // Read before the struct literal moves `active_theme_name`: the
        // host connection set seeds every session's palette from it.
        let host_theme = Theme::load_bundled(&active_theme_name);
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

        let test_mode = std::env::var("ROOST_TEST_MODE").as_deref() == Ok("1");
        let mut app = Self {
            workspace,
            // The engine keeps its direct supervisor reference (the
            // clones handed to `LocalClient` and `IpcHandler` above);
            // only UI-side terminal ops route through the backend.
            backend: TabBackend::in_process(supervisor, test_mode),
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
            window_focused: true,
            title_fallback: title_fallback(profile.kind),
            ime_discard: ImeDiscard::default(),
            modifiers: keyboard::Modifiers::default(),
            test_mode,
            status: StatusBanner::default(),
            rename_editor: None,
            rename_input_id: Id::unique(),
            rename_focus_requested: false,
            rename_completion_key: None,
            rename_op: None,
            next_engine_op: 1,
            pill_labels: HashMap::new(),
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
            tab_strip_scroll_id: Id::unique(),
            revealed_tab: None,
            tab_reveal_request: None,
            exit_state: ExitState::default(),
            #[cfg(target_os = "macos")]
            menu_gating: crate::macos::menu::MenuGating::default(),
            #[cfg(target_os = "macos")]
            menu_window_rows: crate::macos::menu::WindowRows::default(),
            #[cfg(target_os = "macos")]
            menu_can_check_updates: None,
            palette_visibility_retries: 0,
            git_probe: Arc::new(git_metrics::GitProbe::new()),
            metrics_cache: git_metrics::MetricsCache::default(),
            provider_request: 0,
            provider_frames: HashMap::new(),
            palette_present_reply: None,
            palette_activate_replies: HashMap::new(),
            clipboard: ClipboardQueue::default(),
            desktop_notifications: DesktopNotifications::new(
                runtime.handle(),
                feed_tx.clone(),
                profile.app_id.to_owned(),
            ),
            hosts: crate::host_conn::HostConnSet::new(
                runtime.handle().clone(),
                feed_tx.clone(),
                &host_theme,
            ),
            host_attach: HashMap::new(),
            host_resume: HashMap::new(),
            runtime_handle: runtime.handle().clone(),
            feed_rx,
            feed_tx,
            runtime,
            _locks: locks,
        };
        app.reconcile();
        app.resize(app.window_size);
        app.reconnect_saved_hosts();
        tracing::info!(socket = %profile.socket_path.display(), "Iced walking skeleton ready");
        Ok(app)
    }

    /// Launch-time auto-reconnect for saved host sessions: **connect if
    /// present**, and nothing more (plan 037 §3.2).
    ///
    /// Three rules, all deliberate. No daemon is ever spawned here — an
    /// absent socket leaves the host disconnected with a ↻, because a
    /// silent start on every launch is not something an app should do.
    /// Only a localhost host is dialed — a remote host is
    /// manual-reconnect only (D8), and an `ssh -L` forward that is not up
    /// would otherwise make every launch wait on a dial. And on macOS
    /// nothing happens at all: no `roost-session` is packaged there, so
    /// the localhost surface is hidden (§3.1's Mac gate).
    ///
    /// With no saved hosts this is a no-op over an empty list, which is
    /// the zero-change baseline.
    fn reconnect_saved_hosts(&mut self) {
        if cfg!(target_os = "macos") {
            return;
        }
        for host in self.workspace.hosts() {
            let (socket, localhost) = match crate::host_conn::resolve_target(&host.target) {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::warn!(host = %host.id, ?error, "cannot resolve a saved host's target");
                    continue;
                }
            };
            if !localhost {
                continue;
            }
            self.hosts.connect(
                &host.id,
                &host.label,
                socket,
                localhost,
                crate::host_conn::ConnectMode::IfPresent,
            );
        }
        if !self.hosts.is_empty() {
            tracing::info!("probing saved host sessions");
        }
    }

    pub fn window_opened(&mut self, id: window::Id) -> UiTask {
        // The initial Dock-badge sync: `App::new`'s reconcile runs before
        // iced has a window, i.e. before there is an app to badge. From
        // here on the reconcile owns it, and a repeat from a later
        // `WindowFocus` is an idempotent rewrite of the same label.
        self.sync_dock_badge();
        let opened = prepare_window_opened(
            &mut self.window_id,
            &mut self.pending_window_resize,
            &mut self.screenshots,
            id,
        );
        // `App::new`'s reconcile ran before `iced::application(..).font(..)`
        // registered Inter, so it deliberately skipped the pill memo rather
        // than cache wrong-font measurements. The fonts are registered by
        // the time a window opens; this is the populate that reconcile
        // skipped.
        self.refresh_pill_labels();
        self.install_main_menu();
        // After the menu install, so the "Check for Updates…" item
        // already exists for the readiness push below (plan 028 § 3.8).
        self.init_sparkle();
        self.sync_update_menu_item();
        // A freshly installed menu has no Window rows yet, and the next
        // reconcile may be a while off (nothing forces one on the turn a
        // window opens).
        self.sync_window_menu();
        // A notification fired before this runs finds the backend still
        // disabled and shows nothing — by design, and vanishingly rare:
        // policy B only fires for an unfocused window.
        self.init_notifications();
        opened.task
    }

    /// Bring [`App::pill_labels`] up to date with the active project's
    /// strip. Only the active project's tabs are drawn as pills, so only
    /// they are memoized.
    fn refresh_pill_labels(&mut self) {
        let (active_project, active_tab) = self.workspace.active();
        let host = self.backend.host();
        let keys: Vec<PillKey<'_>> = self
            .projects
            .iter()
            .find(|project| project.id == active_project)
            .map(|project| {
                project
                    .tabs
                    .iter()
                    .map(|tab| PillKey {
                        tab: TabKey::new(host, tab.id),
                        title: pill_display_title(&tab.title),
                        active: tab.id == active_tab,
                        has_notification: tab.has_notification,
                    })
                    .collect()
            })
            .unwrap_or_default();
        refresh_pill_label_map(&mut self.pill_labels, &keys);
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

    /// Hand out the pending strip reveal, if any. Held back until the
    /// window exists so a boot-time activation (state.json restoring a
    /// tab that is not the first) still reveals once there is a layout to
    /// measure, instead of being dropped against an empty widget tree.
    pub fn take_tab_reveal_task(&mut self) -> UiTask {
        if self.window_id.is_none() {
            return UiTask::None;
        }
        let Some(tab_id) = self.tab_reveal_request.take() else {
            return UiTask::None;
        };
        UiTask::RevealTab {
            scroll_id: self.tab_strip_scroll_id.clone(),
            pill_id: tab_pill_id(tab_id),
        }
    }

    /// Hand out the exit request `reconcile()` latched, once.
    pub fn take_exit_task(&mut self) -> UiTask {
        if self.exit_state.take() {
            UiTask::Exit
        } else {
            UiTask::None
        }
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
        // The attached host tab's server must follow the grid too — the
        // machine decides when (immediately while live; withheld during
        // hydration, where a mid-snapshot resize forfeits history).
        let host_geometry = self.host_geometry(cols, rows);
        for attach in self.host_attach.values_mut() {
            attach.note_resize(host_geometry);
        }
        for (key, tab) in &mut self.tabs {
            match tab.apply_geometry(cols, rows, self.terminal_metrics, self.metric_generation) {
                Ok(Some(change)) => {
                    tab.commit_geometry(change);
                    // A re-grid rewrites the viewport and drops hover, so
                    // the snapshot the widget draws describes the old
                    // dimensions until it is rebuilt. Window resizes,
                    // sidebar width drags and collapse all land here.
                    refresh_or_warn(key.tab, tab, "window re-grid");
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        tab_id = key.tab,
                        cols,
                        rows,
                        "terminal resize failed"
                    )
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
            if let Some(tab) = self.tabs.get_mut(&self.backend.tab_key(tab_id)) {
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

        // winit withholds the presses an IME consumed, so the only ones
        // arriving mid-composition are the keys the IME declined and
        // forwarded (Enter, Escape, arrows). Those belong to the
        // composition, never to a Roost binding — but a modifier-state
        // event still has to reach its handling above and below. The
        // accepted cost is that every binding, Copy/Paste included, waits
        // for the composition to end rather than cutting it short.
        let composing = self.terminal_composing();
        if !(composing && input::non_modifier_press(&event)) {
            if let Some(action) = input::accelerator(&event)
                .and_then(|accelerator| self.keybindings.get(&accelerator).copied())
            {
                let repeat = matches!(&event, keyboard::Event::KeyPressed { repeat: true, .. });
                return self.dispatch_keybind_action(action, repeat);
            }
        }

        let KeyboardRoute::Terminal(active_key) = self.keyboard_route() else {
            return UiTask::None;
        };
        let active_tab = active_key.tab;
        let Some(tab) = self.tabs.get_mut(&active_key) else {
            return UiTask::None;
        };
        // A bare page key scrolls this tab's own scrollback whenever the shared
        // policy keeps it local — no snap, no encode, nothing on the PTY. The
        // bypass is the policy's decision, never the key's: `Forward` (mouse
        // tracking, alternate screen) falls through to the normal encode below.
        let page_direction = if composing {
            None
        } else {
            input::bare_page_direction(&event, self.modifiers)
        };
        if let Some(direction) = page_direction {
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
        if input::non_modifier_press(&event) {
            if let Err(error) = tab.snap_to_bottom_for_input() {
                tracing::warn!(?error, active_tab, "terminal snap-to-bottom failed");
            }
        }
        let bytes = input::encode_press(&mut tab.encoder, &tab.terminal, event, composing);
        tab.session.send_input(bytes);
        UiTask::None
    }

    /// Whether the terminal that owns the keyboard is mid-composition.
    fn terminal_composing(&self) -> bool {
        ime_preedit_target(self.keyboard_route())
            .and_then(|key| self.tabs.get(&key))
            .is_some_and(|tab| tab.preedit.is_some())
    }

    fn cancel_ime_composition(&mut self) {
        cancel_preedits(&mut self.tabs, &mut self.ime_discard, None);
    }

    /// The IME session ended or restarted: the composition goes, and so
    /// does any pending discard — across a session boundary the OS will
    /// not re-offer the marked text a cancel already dropped.
    pub fn ime_session_boundary(&mut self) {
        self.cancel_ime_composition();
        self.ime_discard.disarm();
    }

    pub fn ime_preedit(&mut self, text: String, cursor: Option<Range<usize>>) {
        let route = self.keyboard_route();
        set_preedit_in(&mut self.tabs, &mut self.ime_discard, route, text, cursor);
    }

    pub fn ime_commit(&mut self, text: &str) {
        let route = self.keyboard_route();
        commit_ime_in(&mut self.tabs, &mut self.ime_discard, route, text);
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

    /// What the menu bar's enabled-state should currently be — the whole
    /// keyboard-route → menu mapping in one place, read both by the seam
    /// push and by the dispatch-side gate below.
    #[cfg(target_os = "macos")]
    fn menu_gating(&self) -> crate::macos::menu::MenuGating {
        crate::macos::menu::MenuGating {
            palette_open: self.palette.is_some(),
            text_capture: self.rename_editor.is_some()
                || self.confirm_delete.is_some()
                || self.terminal_composing(),
        }
    }

    /// Route a native menu activation into the paths a keystroke takes.
    ///
    /// `repeat` is always `false`: a held chord is now AppKit's menu
    /// repeat rather than iced's key repeat, so the per-action repeat
    /// suppression `dispatch_keybind_action` applies has nothing to see
    /// (plan 028 § 3.5, an accepted behavior change).
    ///
    /// The gate here mirrors [`Self::menu_gating`] one layer down, because
    /// the item's enabled-state is not authoritative by the time the event
    /// is dispatched: the activation rides the feed and lands a turn
    /// later, and `performActionForItemAtIndex:` — the route the test op
    /// takes — does not consult enabled-state at all.
    #[cfg(target_os = "macos")]
    pub fn menu_event(&mut self, event: crate::macos::menu::MenuEvent) -> UiTask {
        use crate::macos::menu::{command_enabled, is_palette_toggle, MenuEvent};

        match event {
            MenuEvent::Action(action) => {
                if !command_enabled(self.menu_gating(), is_palette_toggle(action)) {
                    return UiTask::None;
                }
                self.dispatch_keybind_action(action, false)
            }
            // The Window menu's rows take the same paths the sidebar and
            // tab strip take (`Message::ProjectSelected`/`TabSelected`) —
            // including their lack of Swift's `ensureSidebarVisible`,
            // which no iced selection route performs.
            MenuEvent::SelectProject(project_id) => {
                if !command_enabled(self.menu_gating(), false) {
                    return UiTask::None;
                }
                self.select_project(project_id);
                UiTask::None
            }
            MenuEvent::SelectTab(tab_id) => {
                if !command_enabled(self.menu_gating(), false) {
                    return UiTask::None;
                }
                self.select_tab(tab_id);
                UiTask::None
            }
            // Quit is never gated — it is the one command that must work
            // while a modal or a palette owns the keyboard, and the Swift
            // app agrees (its `validateMenuItem` gates only the items
            // targeting the app delegate, not `NSApplication`'s Quit).
            MenuEvent::Quit => {
                self.exit_state.request();
                UiTask::None
            }
            // Ungated for Quit's reason, and additionally guarded by the
            // updater itself: the item is only enabled while
            // `canCheckForUpdates` holds, and Sparkle re-checks that on
            // its own side. Everything past this point — panels, errors,
            // the download flow — is Sparkle's UI, not ours.
            MenuEvent::CheckForUpdates => {
                if let Some(mtm) = servicing::seam_on_main("interactive update check") {
                    crate::macos::sparkle::check_for_updates(mtm);
                    self.sync_update_menu_item();
                }
                UiTask::None
            }
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
                Ok(self.close_tab_dispatch(self.backend.tab_key(tab_id)).task)
            }
            KeybindAction::NewProject => Ok(self.new_project_dispatch().task),
            KeybindAction::RenameProject => {
                self.begin_rename_target(RenameTarget::Project(self.active_project_key()))?;
                Ok(self.take_rename_focus_task())
            }
            KeybindAction::RenameTab => {
                let tab = self.active_tab_key();
                self.begin_rename_target(RenameTarget::Tab(tab))?;
                Ok(self.take_rename_focus_task())
            }
            KeybindAction::CloseProject => {
                // The sentinel id 0 is never in the snapshot, so
                // `confirm_close_project` settles it as a silent no-op.
                self.confirm_close_project(self.active_project_key())?;
                Ok(UiTask::None)
            }
            KeybindAction::JumpToUnread => {
                if let Some(tab) = notification_inbox::next_unread(
                    &self.notification_inbox,
                    self.active_project_key(),
                ) {
                    self.focus_tab_and_clear(tab, true)?;
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

    /// The active project, host-qualified. The workspace's active
    /// selection is a pinned-bare boundary, so this and
    /// [`Self::active_tab_key`] are the joints that qualify it at the
    /// backend that owns it.
    fn active_project_key(&self) -> ProjectKey {
        ProjectKey::new(self.backend.host(), self.workspace.active().0)
    }

    /// [`Self::active_project_key`]'s twin: the one place the workspace's
    /// bare active tab id becomes a key.
    fn active_tab_key(&self) -> TabKey {
        self.backend.tab_key(self.workspace.active().1)
    }

    fn keyboard_route(&self) -> KeyboardRoute {
        let active_tab = self.active_tab_key();
        resolve_keyboard_route(
            self.confirm_delete.is_some(),
            self.rename_editor.is_some(),
            self.palette.is_some(),
            active_tab,
            self.tabs.contains_key(&active_tab),
        )
    }

    pub fn set_window_focus(&mut self, focused: bool) {
        let teardown = focus_teardown(focused);
        if teardown.rename_completion_key {
            self.rename_completion_key = None;
        }
        if teardown.drags {
            self.cancel_drags();
        }
        if teardown.confirm_delete {
            self.cancel_confirm_delete();
        }
        if teardown.ime_composition {
            self.cancel_ime_composition();
        }
        if teardown.ime_discard {
            self.ime_discard.disarm();
        }
        self.window_focused = focused;
        self.workspace.set_window_focused(focused);
        if let Some(tab) = self.tabs.get(&self.active_tab_key()) {
            tab.set_window_focus(focused);
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let started = Instant::now();
        let content = self.view_body();
        let Some(confirm) = &self.confirm_delete else {
            crate::perf::record_view(started.elapsed());
            return content;
        };
        // Copy and structure follow the Mac's `closeActiveProject` NSAlert
        // (App.swift:3145-3182) verbatim, plural "tabs" included.
        let message = column![
            text(format!("Close {}?", confirm.name))
                .size(15)
                .font(chrome::chrome_font(font::Weight::Semibold)),
            text(format!(
                "This will close {} tabs in this project. The action can't be undone.",
                confirm.tab_count
            ))
            .size(12)
            .color(chrome::MUTED_TEXT),
        ]
        .spacing(6);
        let heading: Element<'_, Message> = match chrome::app_icon() {
            Some(icon) => row![
                image(icon)
                    .width(chrome::APP_ICON_SIZE)
                    .height(chrome::APP_ICON_SIZE),
                message
            ]
            .spacing(14)
            .align_y(Alignment::Start)
            .into(),
            None => message.into(),
        };
        let panel = container(
            column![
                heading,
                row![
                    iced::widget::Space::new().width(Fill),
                    button(text("Cancel").size(12))
                        .padding([4, 12])
                        .style(chrome::transparent_button)
                        .on_press(Message::ConfirmDeleteCancel),
                    button(text("Close Project").size(12))
                        .padding([4, 12])
                        .style(chrome::danger_button)
                        .on_press(Message::ConfirmDeleteConfirm)
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            ]
            .spacing(16),
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
        let result = stack![content, catcher, overlay]
            .width(Fill)
            .height(Fill)
            .into();
        crate::perf::record_view(started.elapsed());
        result
    }

    fn view_body(&self) -> Element<'_, Message> {
        let (active_project, active_tab) = self.workspace.active();
        // `self.projects` is this backend's snapshot, so every id read out
        // of it below qualifies at this backend's instance.
        let host = self.backend.host();
        let active_key = TabKey::new(host, active_tab);
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
            let project_key = ProjectKey::new(host, project.id);
            let rollup = project_rollup(
                project
                    .tabs
                    .iter()
                    .map(|tab| agent::effective_lifecycle(&tab.agent_state())),
            );
            let notifying = project.tabs.iter().any(|tab| tab.has_notification);
            let active = project.id == active_project;
            let stripe = container(
                iced::widget::Space::new()
                    .width(chrome::PROJECT_STRIPE_WIDTH)
                    .height(chrome::ROW_HEIGHT - 2.0 * chrome::PROJECT_STRIPE_INSET_Y),
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
                Some(editor) if editor.target == RenameTarget::Project(project_key) => {
                    text_input("Project name", &editor.draft)
                        .id(self.rename_input_id.clone())
                        .on_input(Message::RenameDraftChanged)
                        .on_submit(Message::RenameSubmit)
                        .size(13)
                        // Absolute line height so the editor's total height is
                        // integral: the default relative line height makes it
                        // ~18.9px, and centering that inside the pill puts the
                        // 1px focus border on half-pixels — tiny-skia at scale 1
                        // then blends the border away (fuzzy on screen, and the
                        // real-input harness counts exact border pixels).
                        .line_height(iced::widget::text::LineHeight::Absolute(18.0.into()))
                        .padding([1, 3])
                        .style(chrome::inline_rename_input)
                        .into()
                }
                // Label leading, notification-dot slot trailing: the spacer
                // holds that slot open against the pill's trailing inset
                // (`PROJECT_DOT_INSET`, matched by the pill container's own
                // right padding — chrome::badge lands 8px from the pill's
                // right edge for free). The rename editor keeps the whole
                // pill instead — a fill spacer beside it would halve the
                // field, and no dot renders beside the editor.
                _ => {
                    let label_color = if active {
                        chrome::PROJECT_LABEL_ACTIVE
                    } else {
                        chrome::PROJECT_LABEL_INACTIVE
                    };
                    let mut label_row = row![
                        text(&project.name).size(13).color(label_color),
                        iced::widget::Space::new().width(Fill)
                    ]
                    .align_y(Alignment::Center);
                    if notifying {
                        label_row = label_row.push(
                            container(
                                iced::widget::Space::new()
                                    .width(chrome::NOTIFICATION_DOT_SIZE)
                                    .height(chrome::NOTIFICATION_DOT_SIZE),
                            )
                            .style(chrome::badge),
                        );
                    }
                    label_row.width(Fill).into()
                }
            };
            let project_pill = container(project_label)
                .width(Fill)
                .center_y(chrome::ROW_HEIGHT - 2.0 * chrome::PROJECT_PILL_INSET_Y)
                .padding(iced::Padding {
                    top: 0.0,
                    right: chrome::PROJECT_DOT_INSET,
                    bottom: 0.0,
                    left: chrome::PROJECT_LABEL_INSET,
                })
                .style(chrome::project_pill(
                    active,
                    dragged_project == Some(project.id),
                ));
            // The rail sits at the row's leading edge, inside the pill's own
            // 6px inset — the two never overlap, so a plain row places both
            // without a stack layer between the strip and its rows.
            let project_row = container(
                row![
                    stripe,
                    iced::widget::Space::new()
                        .width(chrome::PROJECT_PILL_INSET_X - chrome::PROJECT_STRIPE_WIDTH),
                    project_pill,
                    iced::widget::Space::new().width(chrome::PROJECT_PILL_INSET_X)
                ]
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .height(chrome::ROW_HEIGHT);
            let mut project_group = column![project_row].spacing(2);
            if self.config.show_sidebar_agents && !hide_agent_rows {
                for agent in self.sidebar_agents.get(&project_key).into_iter().flatten() {
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
                            .style(chrome::agent_button(agent.tab == active_key))
                            .on_press(Message::AgentSelected(agent.tab)),
                    );
                }
            }
            sidebar_body = sidebar_body.push(project_group);
        }
        let sidebar_header = container(
            text("PROJECTS")
                .size(11)
                .color(chrome::MUTED_TEXT)
                .font(chrome::chrome_font(font::Weight::Semibold)),
        )
        .center_y(chrome::BAND_HEIGHT)
        .width(Fill)
        .padding([0, 12])
        .style(chrome::band);
        let sidebar_footer = container(
            button(text("+ New Project").size(13))
                .height(chrome::PILL_HEIGHT)
                .padding([3, 12])
                .style(chrome::footer_chip_button)
                .on_press(Message::NewProject),
        )
        .height(chrome::FOOTER_BAND_HEIGHT)
        .width(Fill)
        .center_x(Fill)
        .padding(iced::Padding {
            top: chrome::FOOTER_PADDING_TOP,
            right: 8.0,
            bottom: chrome::FOOTER_PADDING_BOTTOM,
            left: 8.0,
        })
        .style(chrome::band);
        // The strip delegates layout to its content, so its layout node is the
        // column's: one child per project group, which is what the gesture's
        // hit-testing and target index walk.
        let project_strip = ReorderStrip::projects(
            sidebar_body,
            host,
            visual_project_ids,
            self.project_strip_generation,
            self.strip_gestures_enabled(),
        );
        // The hairline lives inside the sidebar's own width — the outer
        // container paints it and pads the three region fills off it — so the
        // terminal grid keeps every pixel `sidebar_width` leaves it and the
        // resize grip's seam still lands on the sidebar's right edge.
        let sidebar = container(column![
            sidebar_header,
            container(scrollable(project_strip).height(Fill))
                .width(Fill)
                .height(Fill)
                .style(chrome::list),
            sidebar_footer
        ])
        .width(self.live_sidebar_width())
        .height(Fill)
        .padding(iced::Padding::default().right(chrome::DIVIDER_WIDTH))
        .style(chrome::divider);

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
        // `TabDragPreview::drags` waits for the strip's drag threshold:
        // `StripEvent::Started` fires on a bare press, so styling off
        // preview presence alone painted the accent drag border for a
        // frame on every ordinary click. Same rule the sidebar rows use.
        let mut tab_pills = row![].spacing(6);
        for tab in active_project_tabs {
            let tab_key = TabKey::new(host, tab.id);
            let title = pill_display_title(&tab.title);
            let active = tab.id == active_tab;
            let editing = self
                .rename_editor
                .as_ref()
                .filter(|editor| editor.target == RenameTarget::Tab(tab_key));
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
            let key = PillKey {
                tab: tab_key,
                title,
                active,
                has_notification: tab.has_notification,
            };
            let trailing_width = pill_trailing_width(active, tab.has_notification);
            let (title_font, _) = pill_title_metrics(active, tab.has_notification);
            let cached = self
                .pill_labels
                .get(&tab_key)
                .filter(|stored| stored.matches(key));
            let (label, label_width) = if editing.is_some() {
                (Cow::Borrowed(title), RENAME_FIELD_WIDTH)
            } else if let Some(stored) = cached {
                (Cow::Borrowed(stored.label.as_str()), stored.width)
            } else {
                // `view` is `&self`, so a miss cannot be memoized here — it
                // re-elides, and the e2e guard fails loudly if misses ever
                // become steady state (`elide_calls > 0` per keystroke).
                elide_pill_title(key)
            };
            let pill_width = (chrome::TAB_PILL_CHROME_WIDTH + trailing_width + label_width)
                .ceil()
                .clamp(chrome::TAB_PILL_MIN_WIDTH, chrome::TAB_PILL_MAX_WIDTH);
            let select: Element<'_, Message> = if let Some(editor) = editing {
                container(
                    row![
                        dot,
                        text_input("Tab name", &editor.draft)
                            .id(self.rename_input_id.clone())
                            .on_input(Message::RenameDraftChanged)
                            .on_submit(Message::RenameSubmit)
                            .width(RENAME_FIELD_WIDTH)
                            .size(chrome::TAB_TITLE_SIZE)
                            // Same integral-height rationale as the project
                            // rename editor above.
                            .line_height(iced::widget::text::LineHeight::Absolute(18.0.into(),))
                            .padding([1, 3])
                            .style(chrome::inline_rename_input)
                    ]
                    .spacing(chrome::TAB_PILL_LABEL_SPACING)
                    .align_y(Alignment::Center),
                )
                .height(chrome::PILL_HEIGHT)
                .padding([2.0, chrome::TAB_PILL_LABEL_PADDING_X])
                .into()
            } else {
                container(
                    row![
                        dot,
                        text(label)
                            .size(chrome::TAB_TITLE_SIZE)
                            .color(if active {
                                chrome::TEXT
                            } else {
                                chrome::MUTED_TEXT
                            })
                            .font(title_font)
                            // Elision measured with Advanced shaping; the
                            // default Auto drops to Basic for ASCII and
                            // sums unkerned advances a hair wider than
                            // the measured budget.
                            .shaping(iced::widget::text::Shaping::Advanced)
                            // The pill is a fixed width now, so an
                            // unwrapped run is what keeps a title one line
                            // tall when the elision lands a hair long.
                            .wrapping(iced::widget::text::Wrapping::None)
                    ]
                    .spacing(chrome::TAB_PILL_LABEL_SPACING)
                    .align_y(Alignment::Center),
                )
                .height(chrome::PILL_HEIGHT)
                .padding([2.0, chrome::TAB_PILL_LABEL_PADDING_X])
                .into()
            };
            // The spacer pins the badge and the close affordance to the
            // pill's trailing edge once `TAB_PILL_MIN_WIDTH` gives a short
            // title more room than it asked for; at natural width it
            // resolves to nothing.
            let mut pill =
                row![select, iced::widget::Space::new().width(Fill)].align_y(Alignment::Center);
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
                        .style(chrome::close_button)
                        .on_press(Message::CloseTab(tab_key)),
                );
            }
            tab_pills = tab_pills.push(
                container(pill)
                    .id(tab_pill_id(tab_key))
                    .width(pill_width)
                    .height(chrome::PILL_HEIGHT)
                    .padding([0.0, chrome::TAB_PILL_PADDING_X])
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
            host,
            active_project,
            visual_tab_ids,
            self.tab_strip_generation,
            self.strip_gestures_enabled(),
        );
        let add_tab_button = button(text("+").size(16).color(chrome::MUTED_TEXT))
            .width(chrome::PILL_HEIGHT)
            .height(chrome::PILL_HEIGHT)
            .padding(1)
            .style(chrome::transparent_button)
            .on_press(Message::NewTab);
        // The `+` is a sibling of the strip — never inside its content, since
        // the strip walks its own layout children for reorder hit-testing and
        // an extra child there would corrupt the drag target index. As a
        // sibling row inside the scrollable it hugs the last pill and scrolls
        // with overflow (Mac parity: the Mac's trailing ＋ scrolls with the
        // strip too; under overflow it scrolls offscreen — accepted, #281).
        let tab_strip_row = row![tab_strip, add_tab_button]
            .spacing(6)
            .align_y(Alignment::Center);
        // A zero-width scrollbar: any visible indicator overlays the 24px
        // pills themselves and reads as a band across the tab row (#281) —
        // the stock 10px filled rail, and even a 2px hover sliver, both did.
        // Wheel/trackpad scrolling is independent of the scrollbar's size.
        let tab_scroller = scrollable(tab_strip_row)
            .id(self.tab_strip_scroll_id.clone())
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Fill)
            .height(chrome::PILL_HEIGHT);
        let tab_bar = container(tab_scroller)
            .height(chrome::BAND_HEIGHT)
            .width(Fill)
            .padding([chrome::BAND_PILL_PADDING_Y, 8.0])
            .style(chrome::band);

        let terminal: Element<'_, Message> = match self.tabs.get(&active_key) {
            Some(tab) if tab.applied_metrics.is_some() => TerminalWidget {
                tab_id: active_tab,
                snapshot: tab.snapshot.clone(),
                metrics: tab.applied_metrics.unwrap_or(self.terminal_metrics),
                metric_generation: tab.metric_generation,
                ime_active: terminal_ime_active(
                    self.keyboard_route(),
                    active_key,
                    self.window_focused,
                ),
                focused: terminal_cursor_focused(self.keyboard_route(), self.window_focused),
            }
            .into(),
            // No frame to draw yet (the tab is spawning, or attached but
            // still without applied metrics). The mac shows the terminal
            // background until the first frame; text here flashes on every
            // fast spawn. An attached tab answers from its own snapshot, so
            // the theme parse is bounded to the pre-attach window.
            _ => {
                let background = self
                    .tabs
                    .get(&active_key)
                    .map(|tab| tab.snapshot.background)
                    .unwrap_or_else(|| Theme::load_bundled(&self.active_theme_name).background);
                container(Space::new())
                    .width(Fill)
                    .height(Fill)
                    .style(move |_| {
                        container::Style::default()
                            .background(crate::terminal_widget::color(background))
                    })
                    .into()
            }
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
                            .font(chrome::chrome_font(font::Weight::Semibold))
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
                            span = span.font(chrome::chrome_font(font::Weight::Semibold));
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
                .padding([6.0, chrome::PALETTE_ROW_PADDING_X])
                .style(chrome::palette_row(selected, actionable));
            let row = if actionable {
                row.on_press(Message::PaletteActivate(item.id))
            } else {
                row
            };
            // The outer padding — beyond the row's own — pulls the
            // selection highlight in from the panel edge to match the
            // mac's inset look (chrome::PALETTE_ROW_OUTER_INSET's doc
            // comment has the measured mac numbers); the id stays on this
            // outer wrapper so the reveal/clip-snap geometry pass in
            // palette_scroll.rs sees the row's full allocated height.
            items = items.push(
                container(row)
                    .padding([0.0, chrome::PALETTE_ROW_OUTER_INSET])
                    .id(palette_row_id(
                        self.palette_session,
                        self.palette_layout_revision,
                        index,
                    )),
            );
        }
        // Hidden like the tab strip (app.rs:2260-2264, #281): wheel/trackpad
        // scroll stays live, but no rail overlays the rows. W-K.2.
        let list = scrollable(items)
            .id(self.palette_scroll_id.clone())
            .on_scroll(|_| Message::PaletteScrolled)
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::hidden(),
            ))
            .height(Shrink);
        // W-K.3: the mac's own divider (PalettePanel.swift:188-202) doesn't
        // carry over — Charlie called the iced rendering of it out as
        // visual clutter under the filter input; a plain gap replaces it.
        let panel = container(column![input, list].spacing(8))
            .width(Fill)
            .max_width(chrome::PALETTE_WIDTH)
            .height(Shrink)
            .max_height(chrome::PALETTE_MAX_HEIGHT)
            .padding(chrome::PALETTE_PANEL_PADDING)
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

    pub fn select_project(&mut self, project: ProjectKey) {
        // The project strip publishes this on press, immediately before the
        // gesture's start event; cancelling the project drag here would bump
        // the generation the pending start still carries and no drag could
        // ever arm. The project strip cancels the tab drag itself when it
        // arms, exactly as the tab strip does in reverse.
        self.cancel_tab_drag();
        self.cancel_editor_for_interaction();
        let Some(project_id) = project.local_project() else {
            tracing::debug!(?project, "project selection from another instance");
            return;
        };
        if let Some(tab_id) = self.workspace.preferred_tab(project_id) {
            let _ = self.focus_tab_and_clear(self.backend.tab_key(tab_id), false);
        }
    }

    pub fn select_tab(&mut self, tab: TabKey) {
        self.cancel_editor_for_interaction();
        let _ = self.focus_tab_and_clear(tab, false);
    }

    pub fn select_agent(&mut self, tab: TabKey) {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        let _ = self.focus_tab_and_clear(tab, true);
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

    /// The grip dragged the seam past the collapse threshold. Same discipline
    /// as `toggle_sidebar` — the sidebar leaves the tree, so anything anchored
    /// in it is stranded — but the drag itself is dropped, not committed, and
    /// dropped *first*: both `cancel_drags` and `set_sidebar_collapsed` commit
    /// a live drag, and committing here would pin the remembered width at the
    /// clamp floor the gesture last published instead of the width the sidebar
    /// had before it started.
    pub fn sidebar_drag_collapsed(&mut self) {
        drop_drag_and_collapse(&mut self.sidebar_drag_width, &self.workspace);
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.resize(self.window_size);
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
        // The one collapse that does not come through here is the grip's own
        // drag-to-collapse (`sidebar_drag_collapsed`), which drops that drag
        // rather than committing a width the user dragged away from.
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
        let host = self.backend.host();
        let project = ProjectKey::new(host, project_id);
        EngineDispatch {
            task: self.engine_op(
                async move { open_tab_flow(&client, project_id, cwd, title, argv).await },
                move |result| EngineOpResult::TabOpened {
                    op,
                    project,
                    result: result.map(|tab_id| TabKey::new(host, tab_id)),
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
        let host = self.backend.host();
        EngineDispatch {
            task: self.engine_op(
                async move { create_project_flow(&client).await },
                move |result| EngineOpResult::ProjectCreated {
                    op,
                    result: result.map(|(project_id, tab_id)| {
                        (ProjectKey::new(host, project_id), TabKey::new(host, tab_id))
                    }),
                },
            ),
            op: Some(op),
        }
    }

    fn confirm_close_project(&mut self, project: ProjectKey) -> Result<(), String> {
        let Some(target) = confirm_delete_target(&self.projects, project) else {
            return Ok(());
        };
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.dismiss_palette_with_focus_recovery();
        // The modal drops pointer events, so a held terminal button would
        // never see its release: settle every tab's pointer state (synthetic
        // release into tracking PTYs) before the modal owns input.
        for (key, tab) in &mut self.tabs {
            match tab.prepare_pointer_cancel() {
                Ok(release) => {
                    tab.commit_pointer_cancel(release);
                    // The cancel drops hover, so the link underline and
                    // pointer shape the snapshot carries are decorations
                    // for a gesture that no longer exists.
                    refresh_or_warn(key.tab, tab, "pointer cancel before delete confirm");
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        tab_id = key.tab,
                        "pointer cancel before delete confirm"
                    )
                }
            }
        }
        self.confirm_delete = Some(target);
        self.cancel_ime_composition();
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
        let project = confirm.project;
        let Some(project_id) = project.local_project() else {
            tracing::warn!(
                ?project,
                "delete confirmed for a project on another instance"
            );
            return UiTask::None;
        };
        let client = self.client.clone();
        self.engine_op(
            async move { delete_project_flow(&client, project_id).await },
            move |result| EngineOpResult::ProjectDeleted { project, result },
        )
    }

    pub fn close_tab(&mut self, tab: TabKey) -> UiTask {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.close_tab_dispatch(tab).task
    }

    fn close_tab_dispatch(&mut self, tab: TabKey) -> EngineDispatch {
        let Some(tab_id) = tab.local_tab() else {
            tracing::debug!(?tab, "close requested for a tab on another instance");
            return EngineDispatch::default();
        };
        let op = self.take_engine_op_id();
        let client = self.client.clone();
        EngineDispatch {
            task: self.engine_op(
                async move { close_tab_by_id(&client, tab_id).await },
                move |result| EngineOpResult::TabClosed { op, tab, result },
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
        self.focus_tab_and_clear(self.backend.tab_key(tabs[next].id), false)?;
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
        self.focus_tab_and_clear(self.backend.tab_key(tab_id), false)
    }

    fn switch_tab_by_index(&mut self, index: u8) -> Result<(), String> {
        let project_id = self.workspace.active().0;
        let projects = self.workspace.snapshot();
        let Some(tab_id) = active_project_tab_at_index(&projects, project_id, index) else {
            return Ok(());
        };
        self.focus_tab_and_clear(self.backend.tab_key(tab_id), false)
    }

    /// Every focus change in the UI — strip clicks, sidebar rows, the
    /// cycle/switch keybinds, jump-to-unread, the agent and notification
    /// palettes — funnels through here, so this is the one place that owes
    /// them a reconcile.
    fn focus_tab_and_clear(&mut self, tab: TabKey, reveal_sidebar: bool) -> Result<(), String> {
        focus_tab_in_core(&self.workspace, tab)?;
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

    /// The window title, recomposed from live state on every update batch
    /// (iced's `window::State::synchronize` re-calls the title fn and only
    /// touches the OS window when the string changed).
    ///
    /// Mirrors the Mac's priority: the active tab's cwd — which OSC 7 keeps
    /// current through `Workspace::set_tab_cwd` — falling back to the
    /// project's static cwd before a tab has reported one. The native
    /// foreground-process lookup `launch_cwd` uses is deliberately not
    /// consulted here: this runs every batch, and the Mac subtitle tracks
    /// OSC 7 only.
    pub fn window_title(&self, home: &str) -> String {
        let (project_id, tab_id) = self.workspace.active();
        let project = self.projects.iter().find(|p| p.id == project_id).map(|p| {
            let cwd = p
                .tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .map(|tab| tab.cwd.as_str())
                .filter(|cwd| !cwd.is_empty())
                .unwrap_or(p.cwd.as_str());
            (p.name.as_str(), cwd)
        });
        compose_window_title(self.title_fallback, project, home)
    }

    fn launch_cwd(&self, project_id: i64) -> String {
        let active_tab = self.workspace.active().1;
        if let Some(native) = self.backend.foreground_cwd(active_tab) {
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
        // The one observable proof that the run loop dropped `App` rather
        // than the process being killed under it — the exit-on-empty path
        // depends on this running.
        tracing::info!("workspace state flushed on shutdown");
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
            Self::ConfirmDeleteCancel => app.cancel_confirm_delete(),
            Self::ConfirmDeleteConfirm => return app.execute_confirmed_delete(),
            _ => {}
        }
        UiTask::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_workspace_requests_exactly_one_exit() {
        let mut state = ExitState::default();
        assert!(!state.take(), "a running app has no exit to drain");

        assert!(state.observe(true));
        // Every later reconcile sees the same empty workspace (nothing can
        // refill it once the exit is under way) — none of them may re-raise.
        assert!(!state.observe(true));

        assert!(state.take());
        assert!(!state.take(), "the exit is dispatched once");
        assert!(!state.observe(true));
    }

    #[test]
    fn a_populated_workspace_never_requests_an_exit() {
        let mut state = ExitState::default();
        assert!(!state.observe(false));
        assert!(!state.take());
        assert_eq!(state, ExitState::Running);
    }

    #[test]
    fn the_iced_profile_titles_itself_roost_iced() {
        assert_eq!(title_fallback(BundleProfileKind::Iced), "Roost-Iced");
        assert_eq!(title_fallback(BundleProfileKind::Mac), "Roost");
        assert_eq!(
            title_fallback(BundleProfileKind::Linux),
            "Roost",
            "the packaged Linux build resolves Linux and keeps the production name"
        );
        assert_eq!(
            title_fallback(BundleProfileKind::Session),
            "Roost-Session",
            "no window ever runs the headless session profile, but the fallback stays total"
        );
    }

    /// The fallback has to reach BOTH `App::window_title` branches — the
    /// no-project early return and the composed title — through the same
    /// `compose_window_title` the method calls, so neither branch can
    /// quietly revert to a hardcoded "Roost".
    #[test]
    fn the_iced_fallback_composes_into_the_full_title() {
        let fallback = title_fallback(BundleProfileKind::Iced);
        assert_eq!(
            compose_window_title(fallback, None, "/Users/me"),
            "Roost-Iced"
        );
        assert_eq!(
            compose_window_title(fallback, Some(("", "/tmp")), "/Users/me"),
            "Roost-Iced – /tmp"
        );
        assert_eq!(
            compose_window_title(fallback, Some(("strix", "/Users/me/w")), "/Users/me"),
            "strix – ~/w"
        );
    }

    #[test]
    fn terminal_geometry_never_produces_zero_grid() {
        let size = Size::new(1.0, 1.0);
        let metrics = TerminalMetrics::measure(13.0).expect("test metrics");
        assert_eq!(terminal_grid(size, 220.0, metrics), (2, 2));
    }

    fn pill_key(id: i64, title: &str, active: bool) -> PillKey<'_> {
        PillKey {
            tab: TabKey::local(id),
            title,
            active,
            has_notification: false,
        }
    }

    /// A sentinel no elision could ever produce, so its survival proves a
    /// skip and its disappearance proves a recompute.
    fn seed_stale(labels: &mut HashMap<TabKey, PillLabel>, key: PillKey<'_>) {
        labels.insert(
            key.tab,
            PillLabel {
                title: key.title.to_owned(),
                active: key.active,
                has_notification: key.has_notification,
                label: "STALE".to_owned(),
                width: -1.0,
            },
        );
    }

    #[test]
    fn an_untitled_tab_memoizes_the_shell_placeholder() {
        assert_eq!(pill_display_title(""), "shell");
        let mut labels = HashMap::new();
        refresh_pill_label_map(&mut labels, &[pill_key(1, pill_display_title(""), true)]);
        let stored = labels.get(&TabKey::local(1)).expect("the pill is memoized");
        assert_eq!(stored.title, "shell");
        assert_eq!(stored.label, "shell", "a short title elides to itself");
        assert!(stored.width > 0.0);
        assert!(stored.matches(pill_key(1, "shell", true)));
    }

    #[test]
    fn a_changed_pill_key_recomputes_its_label() {
        let mut labels = HashMap::new();
        seed_stale(&mut labels, pill_key(1, "alpha", false));

        refresh_pill_label_map(&mut labels, &[pill_key(1, "beta", false)]);
        assert_eq!(
            labels[&TabKey::local(1)].label,
            "beta",
            "a new title re-elides"
        );

        seed_stale(&mut labels, pill_key(1, "beta", false));
        refresh_pill_label_map(&mut labels, &[pill_key(1, "beta", true)]);
        assert_eq!(
            labels[&TabKey::local(1)].label,
            "beta",
            "activation changes the font weight and the width budget, so it re-elides"
        );

        seed_stale(&mut labels, pill_key(1, "beta", true));
        let notified = PillKey {
            has_notification: true,
            ..pill_key(1, "beta", true)
        };
        refresh_pill_label_map(&mut labels, &[notified]);
        assert_eq!(
            labels[&TabKey::local(1)].label,
            "beta",
            "the badge reservation re-elides"
        );
    }

    #[test]
    fn an_unchanged_pill_key_skips_the_measurer() {
        let mut labels = HashMap::new();
        seed_stale(&mut labels, pill_key(1, "alpha", false));
        refresh_pill_label_map(&mut labels, &[pill_key(1, "alpha", false)]);
        assert_eq!(
            labels[&TabKey::local(1)].label,
            "STALE",
            "an identical key must not re-measure"
        );
    }

    #[test]
    fn a_closed_tab_drops_out_of_the_pill_memo() {
        let mut labels = HashMap::new();
        refresh_pill_label_map(
            &mut labels,
            &[pill_key(1, "alpha", true), pill_key(2, "beta", false)],
        );
        refresh_pill_label_map(&mut labels, &[pill_key(2, "beta", false)]);
        assert_eq!(
            labels.keys().copied().collect::<Vec<_>>(),
            vec![TabKey::local(2)]
        );
    }

    /// A title far past `TAB_PILL_MAX_WIDTH` must come back marked, and the
    /// cached width must be the measured one — the pill lays itself out
    /// from it.
    #[test]
    fn an_overlong_pill_title_caches_its_elided_form() {
        let long = "/Users/charliek/projects/roost/crates/roost-iced/src/app.rs";
        let mut labels = HashMap::new();
        refresh_pill_label_map(&mut labels, &[pill_key(1, long, false)]);
        let stored = &labels[&TabKey::local(1)];
        assert!(stored.label.ends_with(chrome::ELLIPSIS));
        assert!(stored.label.len() < long.len());
        let (_, budget) = pill_title_metrics(false, false);
        assert!(stored.width <= budget);
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
    fn a_drag_collapse_drops_the_live_width_so_a_reopen_restores_the_pre_drag_one() {
        let workspace = Workspace::new();
        workspace.set_sidebar_width(300.0);

        // What the app holds when the grip publishes `Collapse`: the drag's
        // last actionable width, pinned at the clamp floor on its way down.
        let mut drag_width = Some(160.0);
        drop_drag_and_collapse(&mut drag_width, &workspace);

        assert_eq!(drag_width, None, "the live drag is dropped, not committed");
        assert!(workspace.sidebar_collapsed());
        assert_eq!(
            workspace.sidebar_width(),
            300.0,
            "reopening restores the width the sidebar had before the drag"
        );
        assert_eq!(
            effective_sidebar_width(workspace.sidebar_collapsed(), 300.0),
            0.0
        );

        // The contrast that motivates the separate path: committing first —
        // what every other collapse does — would remember the floor.
        let committing = Workspace::new();
        committing.set_sidebar_width(300.0);
        committing.set_sidebar_width(160.0);
        committing.set_sidebar_collapsed(true);
        assert_eq!(committing.sidebar_width(), 160.0);
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
                tab: TabKey::local(7),
                result: Ok(CloseTabOutcome::AlreadyGone),
            },
            EngineOpResult::TabClosed {
                op: 1,
                tab: TabKey::local(7),
                result: Ok(CloseTabOutcome::Closed),
            },
            EngineOpResult::ProjectDeleted {
                project: ProjectKey::local(3),
                result: Ok(DeleteProjectOutcome::AlreadyGone),
            },
            EngineOpResult::ProjectDeleted {
                project: ProjectKey::local(3),
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
                tab: TabKey::local(7),
                result: Err("close exploded".into()),
            }),
            Some("close exploded".to_string())
        );
        assert_eq!(
            engine_op_status(EngineOpResult::ProjectDeleted {
                project: ProjectKey::local(3),
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
                tab: TabKey::local(7),
                result,
            },
        ));
        assert!(matches!(
            completed,
            EngineOpResult::TabClosed {
                op: 1,
                tab,
                result: Ok(CloseTabOutcome::Closed)
            } if tab == TabKey::local(7)
        ));

        let panicked = runtime.block_on(spawn_engine_op(
            runtime.handle().clone(),
            async { panic!("engine op panicked") },
            |result: Result<DeleteProjectOutcome, String>| EngineOpResult::ProjectDeleted {
                project: ProjectKey::local(3),
                result,
            },
        ));
        let EngineOpResult::ProjectDeleted { project, result } = panicked else {
            panic!("a delete's join failure must stay a delete completion")
        };
        assert_eq!(project, ProjectKey::local(3));
        assert!(result.is_err(), "a lost task is that op's own error");
        assert!(engine_op_status(EngineOpResult::ProjectDeleted { project, result }).is_some());
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
                project: ProjectKey::local(3),
                result: Ok(TabKey::local(9)),
            }),
            None
        );
        assert_eq!(
            engine_op_status(EngineOpResult::TabOpened {
                op: 1,
                project: ProjectKey::local(3),
                result: Err("spawn shell failed".into()),
            }),
            Some("spawn shell failed".to_string())
        );
        assert_eq!(
            engine_op_status(EngineOpResult::ProjectCreated {
                op: 2,
                result: Ok((ProjectKey::local(3), TabKey::local(9))),
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
                tab: TabKey::local(7),
                result: Ok(CloseTabOutcome::Closed),
            }
            .palette_op(),
            Some(4)
        );
        assert_eq!(
            EngineOpResult::TabOpened {
                op: 5,
                project: ProjectKey::local(3),
                result: Ok(TabKey::local(9)),
            }
            .palette_op(),
            Some(5)
        );
        assert_eq!(
            EngineOpResult::ProjectCreated {
                op: 6,
                result: Ok((ProjectKey::local(3), TabKey::local(9))),
            }
            .palette_op(),
            Some(6)
        );
        assert_eq!(
            EngineOpResult::ProjectDeleted {
                project: ProjectKey::local(3),
                result: Ok(DeleteProjectOutcome::Deleted),
            }
            .palette_op(),
            None
        );
        assert_eq!(
            EngineOpResult::Renamed {
                op: 7,
                target: RenameTarget::Tab(TabKey::local(9)),
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
            resolve_keyboard_route(false, false, false, TabKey::local(7), false),
            KeyboardRoute::None
        );
        assert_eq!(
            resolve_keyboard_route(false, false, false, TabKey::local(7), true),
            KeyboardRoute::Terminal(TabKey::local(7))
        );
        assert_eq!(
            resolve_keyboard_route(false, false, true, TabKey::local(7), true),
            KeyboardRoute::Palette
        );
        assert_eq!(
            resolve_keyboard_route(false, true, true, TabKey::local(7), true),
            KeyboardRoute::Editor
        );
        // An open confirm outranks every other surface, so no keystroke can
        // reach an accelerator or the active PTY while it is up.
        assert_eq!(
            resolve_keyboard_route(true, true, true, TabKey::local(7), true),
            KeyboardRoute::Confirm
        );
        assert_eq!(
            resolve_keyboard_route(true, false, false, TabKey::local(7), true),
            KeyboardRoute::Confirm
        );
    }

    #[test]
    fn composition_routes_only_to_a_focused_terminal_that_owns_the_keyboard() {
        assert_eq!(
            ime_preedit_target(KeyboardRoute::Terminal(TabKey::local(7))),
            Some(TabKey::local(7))
        );
        for route in [
            KeyboardRoute::None,
            KeyboardRoute::Confirm,
            KeyboardRoute::Editor,
            KeyboardRoute::Palette,
        ] {
            assert_eq!(
                ime_preedit_target(route),
                None,
                "{route:?} owns its own text input"
            );
        }

        assert!(terminal_ime_active(
            KeyboardRoute::Terminal(TabKey::local(7)),
            TabKey::local(7),
            true
        ));
        assert!(!terminal_ime_active(
            KeyboardRoute::Terminal(TabKey::local(7)),
            TabKey::local(7),
            false
        ));
        assert!(!terminal_ime_active(
            KeyboardRoute::Terminal(TabKey::local(8)),
            TabKey::local(7),
            true
        ));
        assert!(!terminal_ime_active(
            KeyboardRoute::Palette,
            TabKey::local(7),
            true
        ));
    }

    /// The terminal cursor is solid only while it owns the keyboard AND
    /// the window has focus — palette/rename/confirm owning the route (or
    /// an unfocused window) draws it hollow uniformly, mac parity.
    #[test]
    fn terminal_cursor_focused_requires_the_terminal_route_and_window_focus() {
        assert!(terminal_cursor_focused(
            KeyboardRoute::Terminal(TabKey::local(7)),
            true
        ));
        assert!(!terminal_cursor_focused(
            KeyboardRoute::Terminal(TabKey::local(7)),
            false
        ));

        for route in [
            KeyboardRoute::None,
            KeyboardRoute::Confirm,
            KeyboardRoute::Editor,
            KeyboardRoute::Palette,
        ] {
            assert!(
                !terminal_cursor_focused(route, true),
                "{route:?} should draw the cursor hollow even while the window is focused"
            );
        }
    }

    /// A commit can arrive after the active tab already moved. It belongs
    /// to the composition, so the tab holding one outranks the route.
    #[test]
    fn a_commit_follows_the_composition_not_the_active_route() {
        assert_eq!(
            ime_commit_target(
                Some(TabKey::local(3)),
                KeyboardRoute::Terminal(TabKey::local(9))
            ),
            Some(TabKey::local(3))
        );
        assert_eq!(
            ime_commit_target(Some(TabKey::local(3)), KeyboardRoute::Palette),
            Some(TabKey::local(3))
        );
        assert_eq!(
            ime_commit_target(None, KeyboardRoute::Terminal(TabKey::local(9))),
            Some(TabKey::local(9))
        );
        assert_eq!(ime_commit_target(None, KeyboardRoute::Palette), None);
    }

    #[test]
    fn the_discard_latch_is_one_shot_and_a_session_boundary_clears_it() {
        let mut latch = ImeDiscard::default();
        assert!(!latch.claims_commit(), "nothing cancelled, nothing dropped");

        latch.arm(false);
        assert!(
            !latch.claims_commit(),
            "a cancel that discarded no live composition arms nothing"
        );

        latch.arm(true);
        assert!(latch.claims_commit());
        assert!(
            !latch.claims_commit(),
            "the latch drops one commit, not every commit"
        );

        latch.arm(true);
        latch.disarm();
        assert!(
            !latch.claims_commit(),
            "a new composition or a session boundary disarms it"
        );
    }

    #[test]
    fn confirm_delete_targets_only_projects_present_in_the_snapshot() {
        let workspace = Workspace::new();
        let project = workspace.create_project("doomed", "/tmp").unwrap();
        workspace.open_tab(project.id, "/tmp", "one").unwrap();
        workspace.open_tab(project.id, "/tmp", "two").unwrap();
        let snapshot = workspace.snapshot();

        assert_eq!(
            confirm_delete_target(&snapshot, ProjectKey::local(project.id)),
            Some(ConfirmDeleteProject {
                project: ProjectKey::local(project.id),
                name: "doomed".into(),
                tab_count: 2,
            })
        );
        assert_eq!(confirm_delete_target(&snapshot, ProjectKey::local(0)), None);
        assert_eq!(
            confirm_delete_target(&snapshot, ProjectKey::local(project.id + 1)),
            None
        );
    }

    /// The dialog quotes the Mac's `closeActiveProject` alert verbatim
    /// (App.swift:3145-3182), plural "tabs" and all — the Mac has no
    /// singular form, so neither do we.
    #[test]
    fn the_confirm_body_reads_exactly_as_the_mac_alert_does() {
        let workspace = Workspace::new();
        let project = workspace.create_project("polish", "/tmp").unwrap();
        workspace.open_tab(project.id, "/tmp", "one").unwrap();
        let snapshot = workspace.snapshot();
        let confirm =
            confirm_delete_target(&snapshot, ProjectKey::local(project.id)).expect("target");

        assert_eq!(format!("Close {}?", confirm.name), "Close polish?");
        assert_eq!(
            format!(
                "This will close {} tabs in this project. The action can't be undone.",
                confirm.tab_count
            ),
            "This will close 1 tabs in this project. The action can't be undone."
        );
    }

    #[test]
    fn a_confirm_whose_project_vanished_externally_is_auto_dismissed() {
        let workspace = Workspace::new();
        let project = workspace.create_project("doomed", "/tmp").unwrap();
        let tab = workspace.open_tab(project.id, "/tmp", "one").unwrap();
        workspace.open_tab(project.id, "/tmp", "two").unwrap();
        let snapshot = workspace.snapshot();
        let mut confirm = confirm_delete_target(&snapshot, ProjectKey::local(project.id));

        reconcile_confirm_delete(&mut confirm, &snapshot);
        assert!(confirm.is_some(), "a live project keeps its confirm open");

        workspace.rename_project(project.id, "relabeled").unwrap();
        reconcile_confirm_delete(&mut confirm, &workspace.snapshot());
        assert_eq!(
            confirm.as_ref().map(|confirm| confirm.name.as_str()),
            Some("relabeled"),
            "an external rename must relabel the open confirm"
        );

        workspace.close_tab(tab.id).unwrap();
        reconcile_confirm_delete(&mut confirm, &workspace.snapshot());
        assert_eq!(
            confirm.as_ref().map(|confirm| confirm.tab_count),
            Some(1),
            "a tab closing under the open dialog must not leave it quoting a stale count"
        );

        workspace.delete_project(project.id).unwrap();
        reconcile_confirm_delete(&mut confirm, &workspace.snapshot());
        assert_eq!(confirm, None);
    }

    #[test]
    fn losing_window_focus_drops_gestures_and_ime_but_never_the_delete_confirm() {
        let unfocus = focus_teardown(false);
        assert!(
            !unfocus.confirm_delete,
            "the delete confirmation outlives an unfocus — dropping it read as a crash"
        );
        assert!(unfocus.drags, "a drag cannot continue under another window");
        assert!(unfocus.rename_completion_key);
        assert!(unfocus.ime_composition);
        assert!(!unfocus.ime_discard);

        let refocus = focus_teardown(true);
        assert_eq!(
            refocus,
            FocusTeardown {
                ime_discard: true,
                ..FocusTeardown::default()
            },
            "refocus only disarms the IME discard latch"
        );
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
        let mut tabs =
            HashMap::from([(TabKey::local(41), "origin"), (TabKey::local(42), "active")]);
        assert_eq!(
            pointer_origin_tab(&mut tabs, TabKey::local(41)).map(|value| *value),
            Some("origin")
        );
        tabs.remove(&TabKey::local(41));
        assert_eq!(pointer_origin_tab(&mut tabs, TabKey::local(41)), None);
        assert_eq!(
            pointer_origin_tab(&mut tabs, TabKey::local(42)).map(|value| *value),
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
    fn agent_colors_match_the_shipped_linux_and_appkit_palette() {
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
        let seven = TabKey::local(7);
        let eleven = TabKey::local(11);
        let mut states = HashMap::from([(seven, original), (eleven, original)]);
        let mut operations = Vec::new();
        let mut persisted = false;

        let result = apply_geometry_batch(&[seven, eleven], 92, 29, next_metrics, 5, |operation| {
            match operation {
                GeometryBatchOperation::Apply {
                    tab,
                    cols,
                    rows,
                    metrics,
                    metric_generation,
                } => {
                    operations.push(format!("apply:{}", tab.tab));
                    if tab == eleven {
                        return Err("injected tab failure".to_string());
                    }
                    let previous = states.insert(
                        tab,
                        TerminalGeometry {
                            cols,
                            rows,
                            metrics,
                            metric_generation,
                        },
                    );
                    Ok(Some(GeometryChange {
                        previous,
                        current: states[&tab],
                        grid_changed: true,
                        metrics_changed: true,
                        deferred_replies: Vec::new(),
                    }))
                }
                GeometryBatchOperation::Rollback { tab, previous } => {
                    operations.push(format!("rollback:{}", tab.tab));
                    states.insert(tab, previous);
                    Ok(None)
                }
            }
        });
        if result.is_ok() {
            persisted = true;
        }

        assert_eq!(
            result.expect_err("second tab must fail"),
            GeometryBatchFailure {
                tab: eleven,
                apply: "injected tab failure".to_string(),
                rollback: Vec::new(),
            }
        );
        assert_eq!(operations, ["apply:7", "apply:11", "rollback:7"]);
        assert_eq!(states[&seven], original);
        assert_eq!(states[&eleven], original);
        assert!(
            !persisted,
            "failed live application cannot reach persistence"
        );
    }
}
