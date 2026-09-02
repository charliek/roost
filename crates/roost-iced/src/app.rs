use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iced::keyboard::{self, key::Named, Key};
use iced::widget::Id;
use iced::widget::{
    button, column, container, image, mouse_area, row, scrollable, stack, text, text_input, Column,
    Row, Space,
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
    HostConnectionResult, PaletteItemView, PalettePresentResult, PaletteStateResult, Project,
    SidebarDumpAgentRow, SidebarDumpProject, SidebarDumpResult, WindowMetricsResult,
};
use roost_ipc::paths::{BundleProfile, BundleProfileKind};
use roost_ipc::IpcServer;
use roost_ui_model::theme::Theme;
use roost_ui_model::typography::{self, FamilyApply, TerminalTypography};
use roost_ui_model::{
    agent_palette,
    config::{self, RoostConfig},
    custom_command, host_sidebar, host_verbs,
    keybind::{self, Accel, AccelMods, KeybindAction},
    keys::{HostId, ProjectKey, TabKey},
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
pub(crate) mod bootstrap;
mod host_dialog;
pub(crate) mod host_notice;
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

/// `projects` is whichever instance's rows `project` names — the local
/// snapshot for a local key, a host's mirrored rows for a host key
/// (`App::project_rows` picks). Matching on the bare id against the
/// wrong instance's rows is what the key's `host` half exists to
/// prevent, so the choice is made once, at the caller.
fn confirm_delete_target(
    projects: &[Project],
    project: ProjectKey,
) -> Option<ConfirmDeleteProject> {
    projects
        .iter()
        .find(|candidate| candidate.id == project.project)
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

/// Where a creation lands (plan 037 §3.1's "creation follows context").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreationTarget {
    Local,
    Host(HostId),
}

/// Resolve a creation's instance.
///
/// Trivial by design, and that is the claim worth pinning: ⌘N and "+ New
/// Project" ask about the selected project's host, ⌘T and the tab bar's
/// "+" ask about the host the tab's project lives on, and the ⌘⇧N picker
/// hands one in directly — all three arrive as a single `HostId`, so
/// "a tab never lands on a different host than its project" holds by
/// construction rather than by a check somewhere that could be missed.
fn creation_target(host: HostId) -> CreationTarget {
    if host.is_local() {
        CreationTarget::Local
    } else {
        CreationTarget::Host(host)
    }
}

/// How long a creation on a host may wait for that host's mirror to
/// list its new row.
///
/// Generous by two orders of magnitude for the localhost round trip this
/// normally is (sub-millisecond, per the HS-1b measurements) and still
/// short enough that nothing waits on a row which is never coming.
const PENDING_HOST_SELECTION_DEADLINE: Duration = Duration::from_secs(10);

/// A creation on a host, parked until the mirror lists it (plan 037
/// §3.9).
#[derive(Debug, Clone, Copy)]
struct PendingHostSelection {
    tab: TabKey,
    /// When the wait started.
    ///
    /// The wait has to be bounded, because "the row will appear" is an
    /// assumption and not a guarantee: a tab whose command exits the
    /// instant it spawns is closed again before any batch lists it, and
    /// a creation that fails after its intent was enqueued never
    /// produces one at all. Neither ends the connection, so nothing else
    /// here would ever clear the entry.
    armed: Instant,
}

impl PendingHostSelection {
    /// Whether this wait has run out. Terminal: an expired entry is
    /// abandoned, never re-armed.
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.armed) >= PENDING_HOST_SELECTION_DEADLINE
    }
}

/// The localhost policy applied where a connection is *started*, rather
/// than only where verbs are listed (plan 037 §3.1, plan 041 §3.1).
///
/// A build that withholds the surface hides a localhost host's Connect
/// row, but the palette is not the only way in: a `localhost` target can
/// already be in `state.json`, `roostctl host add` can save one, and
/// either then gets a live "↻ Reconnect" row in the sidebar. Every one
/// of those lands on the same connect entry, so the policy belongs here
/// — one place, whatever the surface.
///
/// Only *spawning* is refused. `IfPresent` still probes and still
/// connects to a session that is already listening (one started by hand,
/// or reached over an `ssh -L` forward); it just declines to run the
/// launch ladder, and says "no session is running at …" instead.
fn spawn_gate(
    mode: crate::host_conn::ConnectMode,
    policy: host_verbs::VerbPolicy,
) -> crate::host_conn::ConnectMode {
    use crate::host_conn::ConnectMode;
    match mode {
        ConnectMode::SpawnIfMissing if !policy.localhost_surface => ConnectMode::IfPresent,
        mode => mode,
    }
}

/// What launch-time auto-reconnect does with one saved host: dial an
/// already-listening localhost session, or nothing at all.
///
/// Reading the policy here rather than dialing unconditionally is what
/// keeps the two halves in agreement — [`host_verbs::verbs`] withholds
/// Disconnect and Stop for a localhost host under the same flag, so a
/// build that auto-connected one anyway would hold a connection it
/// offers no verb to leave.
fn reconnect_mode(
    policy: host_verbs::VerbPolicy,
    localhost: bool,
) -> Option<crate::host_conn::ConnectMode> {
    (policy.localhost_surface && localhost).then_some(crate::host_conn::ConnectMode::IfPresent)
}

/// One modal overlay: the card, the message a press on the card sends
/// (swallowed, so it does not reach the backdrop underneath), and the
/// message the backdrop sends.
///
/// The three modals — delete confirmation, Add Host, Stop Session —
/// differ only in those three values, so the stacking, the backdrop and
/// the panel chrome are written once.
struct Modal<'a> {
    card: Element<'a, Message>,
    card_pressed: Message,
    dismiss: Message,
}

fn modal_over<'a>(content: Element<'a, Message>, modal: Modal<'a>) -> Element<'a, Message> {
    let overlay = container(mouse_area(modal.card).on_press(modal.card_pressed))
        .padding(16)
        .center(Fill);
    let catcher =
        mouse_area(iced::widget::Space::new().width(Fill).height(Fill)).on_press(modal.dismiss);
    stack![content, catcher, overlay]
        .width(Fill)
        .height(Fill)
        .into()
}

/// The panel every modal is drawn in.
fn modal_card<'a>(body: Column<'a, Message>) -> Element<'a, Message> {
    container(body.spacing(16))
        .width(Fill)
        .max_width(CONFIRM_PANEL_WIDTH)
        .height(Shrink)
        .padding(16)
        .style(chrome::palette_panel)
        .into()
}

/// A modal's title + explanatory paragraph.
fn modal_heading<'a>(title: String, body: &'a str) -> Column<'a, Message> {
    column![
        text(title)
            .size(15)
            .font(chrome::chrome_font(font::Weight::Semibold)),
        text(body).size(12).color(chrome::MUTED_TEXT),
    ]
    .spacing(6)
}

/// A modal's primary action.
///
/// `press` is `None` while the action is unavailable (Add Host's dial in
/// flight), which renders the button disabled rather than removing it —
/// the card must not resize under the pointer.
struct ConfirmButton<'a> {
    label: &'a str,
    style: fn(&iced::Theme, iced::widget::button::Status) -> iced::widget::button::Style,
    press: Option<Message>,
}

/// A modal's trailing button row: the dismissing action, then the
/// primary one.
///
/// Both halves are parameters because the four modals genuinely differ:
/// "Cancel"/"Close Project" and "Not now"/"Restart session" ask
/// different questions, and the upgrade dialog for a *remote* host has
/// no primary action at all — `confirm: None` leaves it with only the
/// dismiss, which is plan 037 §3.1's "no dead button" made literal.
fn modal_buttons<'a>(
    cancel_label: &'a str,
    cancel: Message,
    confirm: Option<ConfirmButton<'a>>,
) -> Row<'a, Message> {
    let mut buttons = row![
        iced::widget::Space::new().width(Fill),
        button(text(cancel_label).size(12))
            .padding([4, 12])
            .style(chrome::transparent_button)
            .on_press(cancel),
    ];
    if let Some(confirm) = confirm {
        let mut primary = button(text(confirm.label).size(12))
            .padding([4, 12])
            .style(confirm.style);
        if let Some(message) = confirm.press {
            primary = primary.on_press(message);
        }
        buttons = buttons.push(primary);
    }
    buttons.spacing(8).align_y(Alignment::Center)
}

/// A host tab's last frame, kept on screen under a scrim and a banner
/// (plan 037 §3.1's takeover treatment, reused for "session ended").
///
/// Not a modal: the rest of the window stays live, because the user's
/// local tabs and every other host are unaffected — only *this* frame
/// stopped being true. The scrim is a layer rather than a recolor
/// because the terminal draws from an owned snapshot; the banner sits
/// above it so its own text is not dimmed with the frame it describes.
fn frozen_frame<'a>(
    content: Element<'a, Message>,
    saved_id: &str,
    frame: host_notice::FrozenFrame,
    banner: host_notice::HostBanner,
) -> Element<'a, Message> {
    let strip = container(
        row![
            text(banner.message)
                .size(chrome::HOST_BANNER_TEXT_SIZE)
                .color(chrome::HOST_BANNER_TEXT),
            iced::widget::Space::new().width(Fill),
            // The frame travels with the press: the button's promise is
            // this frame's, and honoring it against a host that has
            // since moved on would mean either aborting a reconnect
            // already running or quietly starting a new session under a
            // button that said "reconnect".
            button(text(banner.action).size(chrome::HOST_BANNER_ACTION_SIZE))
                .padding([2, 9])
                .style(chrome::host_banner_button)
                .on_press(Message::HostFrameReconnect {
                    saved_id: saved_id.to_string(),
                    frame,
                }),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([6, 12])
    .style(chrome::host_banner);
    let edge = container(iced::widget::Space::new().width(Fill).height(1.0))
        .style(chrome::host_banner_edge);
    let scrim = container(iced::widget::Space::new().width(Fill).height(Fill))
        .style(chrome::host_frame_scrim);
    stack![content, scrim, column![strip, edge]]
        .width(Fill)
        .height(Fill)
        .into()
}

/// The Add Host card's heading, and the Stop confirmation's.
///
/// Constants rather than literals at the widget, so `app.dialog_dump`
/// reports what the card actually says instead of a second copy of it
/// that could drift.
const ADD_HOST_TITLE: &str = "Add Host";
const ADD_HOST_BODY: &str = "Point Roost at a running roost-session: an SSH destination \
                             (workbox, user@host, ssh://host:port) or a local socket path.";
const STOP_SESSION_BODY: &str = "Every shell on that host ends. The layout is saved, so \
                                 starting the session again reopens the same tabs as fresh \
                                 shells.";
const STOP_SESSION_CONFIRM: &str = "Stop Session";

fn stop_session_title(label: &str) -> String {
    format!("Stop the session on {label}?")
}

/// One labelled text field in the Add Host dialog (the mock's
/// `label` + `.field` pair).
fn dialog_field<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &str,
    id: Id,
    on_input: impl Fn(String) -> Message + 'a,
) -> Column<'a, Message> {
    column![
        text(label).size(11).color(chrome::MUTED_TEXT),
        text_input(placeholder, value)
            .id(id)
            .on_input(on_input)
            .on_submit(Message::AddHostSubmit)
            .size(12)
            .padding([5, 8])
            .style(chrome::palette_input),
    ]
    .spacing(3)
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
    /// The Add Host dialog's "Add & Connect" finished dialing (plan 037
    /// §3.1).
    ///
    /// `generation` is the submit this answers, echoed back: the dialog
    /// is still open and still owns the draft, and a reply it is no
    /// longer waiting on would otherwise save a host the user has since
    /// edited past — or, after a cancel and a reopen with the same
    /// fields, one they are describing for the second time and have not
    /// confirmed yet. The label and target ride along because the save
    /// itself needs them.
    HostVerified {
        generation: u64,
        label: String,
        target: String,
        result: Result<(), crate::host_conn::ConnectFailure>,
    },
    /// The stopping half of an upgrade restart finished (plan 037 §3.7).
    ///
    /// No generation guard: unlike the Add Host dial there is nothing
    /// still open to answer — the dialog closed when the button was
    /// pressed — and the only consumer is a relaunch addressed to the
    /// saved host, which is stable across everything but a `host.remove`
    /// (and that is refused for a host with a live connection).
    HostRestarted {
        saved_id: String,
        result: Result<(), String>,
    },
}

/// Build the future behind [`UiTask::EngineOp`]: the op runs on the
/// engine runtime, the Iced task only awaits the join, and `complete`
/// turns whichever way it went into the message the UI thread handles.
///
/// A join failure — the op panicked, or the runtime is shutting down —
/// becomes that op's own error rather than a dropped completion: every
/// dispatch owes the UI exactly one [`EngineOpResult`]. That is the one
/// thing an op's error type has to be able to express, hence the
/// `From<String>` bound: most ops fail as a `String`, and the Add Host
/// dial fails as a classified [`crate::host_conn::ConnectFailure`].
fn spawn_engine_op<T, E>(
    handle: tokio::runtime::Handle,
    op: impl Future<Output = Result<T, E>> + Send + 'static,
    complete: impl FnOnce(Result<T, E>) -> EngineOpResult + Send + 'static,
) -> EngineOpFuture
where
    T: Send + 'static,
    E: Send + 'static + From<String>,
{
    Box::pin(async move {
        let result = match handle.spawn(op).await {
            Ok(result) => result,
            Err(error) => Err(E::from(error.to_string())),
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
        // Same rule, one dialog over: a failed verify belongs *in* the
        // Add Host dialog, next to the field the user has to change —
        // a banner behind an open modal is the wrong place to say why
        // the modal did not close.
        // And a restart's failure already names the rung it stopped at,
        // so `host_restart_completed` puts that on the status bar itself.
        EngineOpResult::Renamed { .. }
        | EngineOpResult::TabsReordered { .. }
        | EngineOpResult::ProjectsReordered { .. }
        | EngineOpResult::HostVerified { .. }
        | EngineOpResult::HostRestarted { .. } => None,
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
            | Self::ProjectsReordered { .. }
            // Add Host and the upgrade prompt are dialogs, not palette
            // rows: the palette is already dismissed by the time either
            // opens, so the `palette.activate` that opened it was
            // answered then.
            | Self::HostVerified { .. }
            | Self::HostRestarted { .. } => None,
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

/// One control-plane op on a host's queue, typed (plan 037 §3.9).
///
/// The queue answers every intent, including the ones it refuses, so
/// this future always resolves — a caller awaiting a creation on a host
/// that drops mid-flight gets an error, never a hang.
async fn host_call<T: serde::de::DeserializeOwned>(
    ops: &crate::host_conn::HostOps,
    op: &'static str,
    params: serde_json::Value,
) -> Result<T, String> {
    let value = ops
        .call(op, params, false)
        .await
        .map_err(|error| format!("{op}: {error}"))?;
    serde_json::from_value(value).map_err(|error| format!("{op} answered unexpectedly: {error}"))
}

/// [`create_project_flow`]'s host twin: the same two ops, in the same
/// order, on the host's queue instead of the local client.
///
/// The cwd is deliberately left empty rather than filled with this
/// machine's `$HOME`. A remote host's home directory is not ours, and a
/// path that does not exist over there would spawn a shell in a
/// directory nobody chose — the session falls back to its own launch cwd
/// for an empty one, which is the closest thing to "wherever that
/// machine starts things".
async fn create_host_project_flow(ops: crate::host_conn::HostOps) -> Result<(i64, i64), String> {
    use roost_ipc::messages::{ops as wire, ProjectCreateResult, TabOpenResult};
    let created: ProjectCreateResult = host_call(
        &ops,
        wire::PROJECT_CREATE,
        serde_json::json!({ "name": "", "cwd": "" }),
    )
    .await?;
    let project = created.project;
    let opened: TabOpenResult = host_call(
        &ops,
        wire::TAB_OPEN,
        host_tab_open_params(project.id, &project.cwd),
    )
    .await?;
    Ok((project.id, opened.tab.id))
}

/// A host op that removes something, where "it is already gone" is the
/// state the user asked for rather than a failure.
///
/// The local twins tolerate exactly this (`close_tab_by_id`,
/// `delete_project_flow` both fold their not-found into an
/// `AlreadyGone`), and they have to agree: the same palette row, the
/// same ✕, and the same confirmation reach both, so a host tab that
/// closed a moment ago must not raise a banner a local one would not.
async fn host_remove_call<T>(
    ops: &crate::host_conn::HostOps,
    op: &'static str,
    params: serde_json::Value,
    removed: T,
    already_gone: T,
) -> Result<T, String> {
    match ops.call(op, params, false).await {
        Ok(_) => Ok(removed),
        Err(crate::host_conn::HostOpError::Rejected {
            code: roost_ipc::client::ServerCode::NotFound,
            ..
        }) => Ok(already_gone),
        Err(error) => Err(format!("{op}: {error}")),
    }
}

/// [`close_tab_by_id`]'s host twin. Awaited rather than fired and
/// forgotten so a refusal reaches the user, on the same status banner an
/// ordinary validation error uses (plan 037 §3.9's error surfacing).
async fn close_host_tab_flow(
    ops: crate::host_conn::HostOps,
    tab_id: i64,
) -> Result<CloseTabOutcome, String> {
    host_remove_call(
        &ops,
        roost_ipc::messages::ops::TAB_CLOSE,
        serde_json::json!({ "tab_id": tab_id.to_string() }),
        CloseTabOutcome::Closed,
        CloseTabOutcome::AlreadyGone,
    )
    .await
}

/// [`delete_project_flow`]'s host twin, for the same reason.
async fn delete_host_project_flow(
    ops: crate::host_conn::HostOps,
    project_id: i64,
) -> Result<DeleteProjectOutcome, String> {
    host_remove_call(
        &ops,
        roost_ipc::messages::ops::PROJECT_DELETE,
        serde_json::json!({ "project_id": project_id.to_string() }),
        DeleteProjectOutcome::Deleted,
        DeleteProjectOutcome::AlreadyGone,
    )
    .await
}

/// Open one tab on a host, in an existing project.
async fn open_host_tab_flow(
    ops: crate::host_conn::HostOps,
    project_id: i64,
    cwd: String,
) -> Result<i64, String> {
    use roost_ipc::messages::{ops as wire, TabOpenResult};
    let opened: TabOpenResult =
        host_call(&ops, wire::TAB_OPEN, host_tab_open_params(project_id, &cwd)).await?;
    Ok(opened.tab.id)
}

/// `tab.open` params for a host, geometry included.
///
/// The same defaults the local path opens with: the tab is resized to
/// the window's real grid at attach (`tab.attach` carries the geometry
/// and the server resizes there), so this only has to be a legal
/// starting size, not the right one.
fn host_tab_open_params(project_id: i64, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "project_id": project_id.to_string(),
        "cwd": cwd,
        "cols": u32::from(DEFAULT_COLS),
        "rows": u32::from(DEFAULT_ROWS),
    })
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

/// The core half of every focus change: the one op the UI owes the
/// workspace. `focus_tab` acknowledges the tab's notification itself, and
/// its error is also the "is this tab still there?" guard — a route holding
/// only a `&Workspace` (the notification banner's click, which arrives with
/// no `&mut App` in hand) gets all of that from this one call.
fn focus_tab_in_core(workspace: &Workspace, tab: TabKey) -> Result<(), String> {
    // The local workspace owns only the local id-space; another
    // instance's tab is not this workspace's to focus, and applying its
    // number here would jump to whatever local tab shares it.
    let tab_id = tab.local_tab().ok_or_else(|| {
        format!("tab {tab:?} belongs to another instance; the local workspace cannot focus it")
    })?;
    workspace
        .focus_tab(tab_id)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// See [`App::terminal_event_key`]. Split out so the "a host terminal is
/// showing, so a bare id is that host's" rule can be checked without an
/// `App` (which needs a bundle profile, the instance lock and the Iced
/// runtime to build).
fn terminal_event_key(active: TabKey, local_host: HostId, tab_id: i64) -> TabKey {
    if active.tab == tab_id {
        active
    } else {
        TabKey::new(local_host, tab_id)
    }
}

/// The host tab whose attach a selection move releases, if any. See
/// [`App::set_host_selection`] — this is the decision half, split out to
/// be checkable without an `App`.
fn host_selection_detach(
    previous: Option<HostSelection>,
    next: Option<HostSelection>,
) -> Option<TabKey> {
    let previous = previous?;
    if next.is_some_and(|selection| selection.tab == previous.tab) {
        return None;
    }
    Some(previous.tab)
}

/// Which host tab this client is *looking at*, as `session.set_focus`
/// states it: the selected host tab when the window has focus, and
/// nothing otherwise.
///
/// The whole edge computation, split out from [`App`] because it is the
/// part worth pinning: a session mutes the tab it believes is focused,
/// so "the window is unfocused" and "the selection moved to another
/// host" both have to read as *no* claim rather than as a stale one.
/// Only host tabs appear here — a local selection is `None`, which is
/// how every connected host hears null.
fn host_focus_claim(window_focused: bool, selection: Option<HostSelection>) -> Option<TabKey> {
    selection.filter(|_| window_focused).map(|it| it.tab)
}

fn clamped_tab_index(current: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 || current >= len {
        return None;
    }
    Some((current as isize + delta).clamp(0, len as isize - 1) as usize)
}

fn dispatch_keybind_once_unless_repeat<T>(repeat: bool, dispatch: impl FnOnce() -> T) -> Option<T> {
    (!repeat).then(dispatch)
}

/// The sidebar's band strip. The "PROJECTS" header and every host band
/// are the same chrome, so the height and insets live in one place —
/// that parity is the whole reason a host section reads as a band and
/// not as a second visual language.
fn sidebar_band<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .center_y(chrome::BAND_HEIGHT)
        .width(Fill)
        .padding([0, 12])
        .style(chrome::band)
        .into()
}

/// A band's label, in the one weight and size every band uses.
fn sidebar_band_label(label: &str) -> Element<'_, Message> {
    text(label)
        .size(11)
        .color(chrome::MUTED_TEXT)
        .font(chrome::chrome_font(font::Weight::Semibold))
        .into()
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
    /// Add Host, or the Stop Session confirmation (plan 037 §3.1).
    /// Distinct from [`Self::Confirm`] because one of the two owns text
    /// fields: Enter submits the draft rather than confirming a
    /// deletion, and Escape closes this dialog rather than that one.
    HostDialog,
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
        | KeyboardRoute::HostDialog
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

/// Whether the terminal the window is drawing can take a keystroke: it
/// has a session, and the frame it is showing is still being updated.
///
/// A frozen host frame is a picture of a session this window no longer
/// drives (plan 037 §3.1) — its input queue is gone with the connection,
/// so routing keys there swallows them silently, which reads to a user
/// as a hung terminal. Answering `false` sends the route to
/// [`KeyboardRoute::None`], where accelerators still fire and the modal
/// routes above still win, and the banner's button is the only thing the
/// dead frame offers.
fn active_terminal_live(has_session: bool, frame_frozen: bool) -> bool {
    has_session && !frame_frozen
}

fn resolve_keyboard_route(
    confirm_open: bool,
    host_dialog_open: bool,
    editor_open: bool,
    palette_open: bool,
    active_tab: TabKey,
    active_terminal_live: bool,
) -> KeyboardRoute {
    if confirm_open {
        KeyboardRoute::Confirm
    } else if host_dialog_open {
        KeyboardRoute::HostDialog
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

/// What a delivered SIGTERM/SIGINT should do — request the same graceful
/// quit a menu Quit would, or (a repeat, meaning the graceful path is
/// wedged) force the process down immediately. A pure decision over a
/// shared latch, so it is testable without tokio or a real `App`.
#[derive(Debug, PartialEq, Eq)]
enum QuitSignalAction {
    RequestQuit,
    Escalate,
}

/// `handled` is shared across every signal listener `spawn_quit_signals`
/// installs, so a repeat of the *same* signal and a *different* one both
/// escalate — "already handled one" is what matters, not which.
fn observe_quit_signal(handled: &AtomicBool) -> QuitSignalAction {
    if handled.swap(true, Ordering::SeqCst) {
        QuitSignalAction::Escalate
    } else {
        QuitSignalAction::RequestQuit
    }
}

/// Route SIGTERM and SIGINT into the same exit request a menu Quit makes
/// (plan 039 §3.9) — closing plan 038's known gap, where a bare SIGTERM
/// behaved exactly like a crash (no `Drop for App`, so no workspace flush
/// and no tunnel `-O exit`; an ssh ControlMaster would strand until its
/// `ControlPersist` window expired). A second signal past the first
/// escalates to `process::exit(1)`: insurance against a wedged graceful
/// path, which is the very freeze this exists to prevent.
///
/// Mirrors `roost-session`'s `spawn_signal_stops` (`serve.rs`), adapted to
/// this app's own runtime and its own drain instead of a wire call.
///
/// Unlike that sibling, both streams are registered **synchronously**,
/// before either listener task is spawned — under `runtime.enter()` so
/// `signal()` has the reactor context it needs (`bootstrap()` is
/// synchronous, pre-run-loop code that isn't itself executing on the
/// runtime; `Handle::current()` panics with "no reactor running" without
/// the guard). Registering from *inside* the spawned task, as an earlier
/// version of this did to route around that panic, left a window between
/// `spawn` returning and the task's first poll where the signal still had
/// its OS default disposition — a SIGTERM/SIGINT landing in that window
/// killed the process with no flush and no tunnel `-O exit`, the exact
/// gap this function exists to close. Registering synchronously here
/// closes it: by the time this returns, both signals are already routed
/// through `observe_quit_signal`, before the caller can be interrupted.
///
/// Failure to register is treated as fatal to startup, like every other
/// fallible step in `bootstrap()` (`?` throughout) — this call is *the*
/// safety net C7 adds; starting anyway and logging-and-continuing would
/// silently ship the app back into the pre-C7 bare-kill behavior with no
/// visible signal that the protection is missing. `signal()` registration
/// realistically only fails on a broken environment (e.g. the process is
/// already out of OS signal-handling resources), so refusing to start is
/// the defensible choice: the failure is surfaced immediately in the
/// startup error rather than discovered later as a shutdown that dropped
/// state.
fn spawn_quit_signals(runtime: &tokio::runtime::Runtime, feed_tx: &EngineFeedSender) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let streams = {
        let _guard = runtime.enter();
        let term = signal(SignalKind::terminate()).context("install a SIGTERM handler")?;
        let int = signal(SignalKind::interrupt()).context("install a SIGINT handler")?;
        [("SIGTERM", term), ("SIGINT", int)]
    };

    let handled = Arc::new(AtomicBool::new(false));
    for (name, mut stream) in streams {
        let handled = Arc::clone(&handled);
        let feed_tx = feed_tx.clone();
        runtime.spawn(async move {
            loop {
                if stream.recv().await.is_none() {
                    return;
                }
                match observe_quit_signal(&handled) {
                    QuitSignalAction::RequestQuit => {
                        tracing::info!(
                            signal = name,
                            "signal received; requesting a graceful quit"
                        );
                        if !feed_tx.send(EngineFeed::Quit) {
                            return;
                        }
                    }
                    QuitSignalAction::Escalate => {
                        tracing::warn!(
                            signal = name,
                            "second signal received before the graceful quit finished; forcing exit"
                        );
                        std::process::exit(1);
                    }
                }
            }
        });
    }
    Ok(())
}

/// Restores SIGTERM/SIGINT to `SIG_DFL` when dropped. An `App` field of
/// this type, placed immediately before `runtime` in the struct so it
/// drops immediately before it, is what keeps a wedged shutdown killable.
///
/// Tokio never unregisters the libc handlers `spawn_quit_signals` installs
/// — it just stops polling the streams once the runtime that ran them is
/// gone. Left alone, that means: `Runtime::drop` cancels the listener
/// tasks (no more `observe_quit_signal`/escalate), but the OS-level
/// handler is still armed, so a signal arriving *during* runtime shutdown
/// is consumed and silently dropped — neither escalated nor left to the
/// default disposition that would kill the process. If teardown itself
/// hangs there, no further SIGTERM/SIGINT can force it down: the inverse
/// of the freeze `spawn_quit_signals` exists to prevent.
///
/// Field placement makes the ordering explicit without extra plumbing:
/// every field declared above `runtime` — including `hosts`, whose
/// `SshTunnel` Drop blocks on `-O exit` — has already finished dropping
/// by the time this one is reached, so the graceful path has always had
/// its chance before defaults are restored; `runtime` itself hasn't
/// started dropping yet, so `process::exit(1)` escalation is still live
/// for the entire window this guard is protecting.
///
/// A signal delivered after this restore terminates the process by its
/// own default disposition (143 for SIGTERM, 130 for SIGINT) rather than
/// the escalate path's `exit(1)` — that's an accepted, arguably more
/// correct, difference: the exit status then reflects that the process
/// was killed by a signal rather than choosing to exit(1) on its own.
struct RestoreDefaultQuitSignalsOnDrop;

impl Drop for RestoreDefaultQuitSignalsOnDrop {
    fn drop(&mut self) {
        // SAFETY: SIG_DFL is always a valid disposition, and libc::signal
        // has no preconditions beyond a valid (signum, handler) pair.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
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
/// Which of a host's projects lists this tab, if any.
///
/// The shared half of [`App::host_project_of`] and its non-interactive
/// twin [`App::host_listed_project_of`] — so the only difference between
/// them is the one word that is the point: which view lookup they ask.
fn listed_project_of(view: &HostView, tab: TabKey) -> Option<ProjectKey> {
    let project = view
        .projects
        .iter()
        .find(|project| project.tabs.iter().any(|row| row.id == tab.tab))?;
    Some(ProjectKey::new(tab.host, project.id))
}

/// Pure half of [`App::window_title`]: `project` is the active project's
/// `(name, effective cwd)`, `None` when no project is active. Split out so
/// tests can pin that BOTH branches thread the profile-chosen fallback.
///
/// `host` is the label of the session that project lives on, `None` for
/// the local workspace (plan 037 §3.1). It is appended rather than
/// prefixed so the project name stays where a user's eye already looks
/// for it, and it is omitted entirely for local rows — the zero-host
/// title is byte-identical to the one before hosts existed.
fn compose_window_title(
    fallback: &str,
    project: Option<(&str, &str)>,
    host: Option<&str>,
    home: &str,
) -> String {
    let Some((name, cwd)) = project else {
        return fallback.to_string();
    };
    let Some(host) = host else {
        return window_title::window_title_with_fallback(fallback, name, cwd, home);
    };
    // The fallback is applied here rather than left to the composer:
    // an unnamed project on a host must still say which host, and
    // "Roost-Iced (pop-os)" is more use than "Roost-Iced".
    let named = if name.is_empty() { fallback } else { name };
    window_title::window_title_with_fallback(fallback, &format!("{named} ({host})"), cwd, home)
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
    /// Host tabs that have rung since they were last focused.
    ///
    /// A bell is an effect, not workspace state (plan 037 §3.6): the
    /// session reports it once and keeps no flag, so nothing on the
    /// server side will ever say "this tab is still ringing". This is
    /// that memory, and it is an *input* to the notification-inbox
    /// derivation rather than a row written behind the derivation's
    /// back — which is what stops the next reconcile pruning it.
    /// Cleared on focus, and swept wherever the tab itself goes.
    host_bells: HashSet<TabKey>,
    /// Which host row the window is showing, when it is showing one.
    ///
    /// The app's notion of "the active tab" is `workspace.active()`, and
    /// the local workspace has no opinion about a host's tabs — so a host
    /// selection is an override laid over it: [`Self::active_tab_key`]
    /// and [`Self::active_project_key`] answer from here while it is set,
    /// and focusing any local tab clears it (`focus_tab_and_clear`).
    /// Every surface that asks "what is focused?" already goes through
    /// those two joints, so nothing else has to know.
    ///
    /// Invalidated by [`Self::reconcile_host_selection`]: a tab that
    /// closed, a host that dropped, or an incarnation that was replaced
    /// all fall back to the local workspace's own selection. C7's
    /// creation routing reads the same field to answer "which host does
    /// ⌘N create on?".
    host_selection: Option<HostSelection>,
    /// The Add Host / Stop Session modal, when one is up (plan 037
    /// §3.1). One field for both: they are the same kind of thing — an
    /// answer the user still owes — and only one can be up at a time.
    host_dialog: Option<host_dialog::HostDialog>,
    /// The upgrade restarts under way (plan 037 §3.7). The prompt stays
    /// reachable while one runs — the host is still `NeedsRestart` until
    /// the relaunch connects — so the ladder is claimed here rather than
    /// left for a second press to start again on the same socket.
    host_restarts: crate::host_conn::restart::RestartsInFlight,
    /// The probes and install jobs a bootstrap offer has in flight
    /// (plan 039 §3.5) — the probes keyed by saved host, the jobs by
    /// normalized target token.
    bootstraps: bootstrap::BootstrapsInFlight,
    /// The Add Host dialog's Name field, and whether it still owes a
    /// focus. Same one-shot shape as the rename editor's: the request is
    /// raised where the dialog opens and drained by whichever route
    /// returns a task.
    add_host_name_id: Id,
    add_host_socket_id: Id,
    add_host_focus_requested: bool,
    /// A creation on a host, waiting for the mirror to list it.
    ///
    /// Event-confirmed selection (§3.9): the op's reply names the new
    /// tab, but the event batch that puts it in the mirror is a separate
    /// message and may land after. The key is parked here and resolved
    /// by the first reconcile that can see the row.
    pending_host_selection: Option<PendingHostSelection>,
    /// One entry per saved host, in registry order — the sidebar's host
    /// sections, refreshed by `reconcile`. **Empty with no saved hosts**,
    /// and every host-aware branch in the view is gated on that, which is
    /// what keeps the zero-host sidebar byte-identical to today's.
    host_views: Vec<HostView>,
    /// The bands drawn above those rows — LOCAL first, then one per
    /// entry of `host_views`, so `host_sections[1..]` pairs off with it
    /// positionally. Cached for the same reason `host_views` is: the
    /// labels and rollups are `String`s the widget tree borrows, and
    /// rebuilding them per frame would allocate on every PTY burst.
    /// Rebuilt at the tail of `refresh_sidebar_agents`, which is where
    /// the per-host agent counts the rollups read get filled in.
    host_sections: Vec<host_sidebar::Section>,
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
    /// Drops immediately before `runtime` — see
    /// [`RestoreDefaultQuitSignalsOnDrop`] for why that position matters.
    _restore_quit_signals: RestoreDefaultQuitSignalsOnDrop,
    runtime: tokio::runtime::Runtime,
    _locks: InstanceLocks,
}

/// One saved host as the view reads it, cached at reconcile exactly the
/// way [`App::projects`] caches the local workspace's snapshot.
///
/// The cache is not an optimization — it is a lifetime requirement. The
/// live mirror is behind a mutex, and a widget tree borrows the strings
/// it renders, so the view cannot both hold the guard and hand its
/// contents back to iced. Reconcile takes the copy; the view borrows it.
struct HostView {
    /// `HostSnapshot.id`, which is what a reconnect verb is addressed to.
    saved_id: String,
    label: String,
    /// Whether this host's target is this machine's own session. Read
    /// from the registry rather than from the connection, so it is known
    /// for a host that has never connected — which is exactly when the
    /// macOS gate has to decide whether to offer a Connect verb.
    localhost: bool,
    /// The incarnation these rows are keyed at. `HostId::LOCAL` stands
    /// for "no connection has ever published rows for this host" — the
    /// section is then header-only, and `projects` is empty, so the
    /// placeholder can never collide with a real local key.
    host: HostId,
    state: host_sidebar::SectionState,
    /// The connection's own one-line reason for the state it is in, when
    /// it has one — an ssh failure, a transport drop. Folded into the
    /// band's rollup; `None` renders the bare word.
    reason: Option<String>,
    /// The last rows this host's connection published. Kept across a
    /// drop: those shells are still running over there, so the section
    /// lists them dimmed rather than pretending they are gone.
    projects: Vec<Project>,
    active_tab_id: i64,
    /// How many agent rows this host contributes, for the band's rollup.
    agents: usize,
}

/// The host row the window is showing (plan 037 §3.1). Both halves are
/// carried because the tab bar renders the project's tabs and the
/// terminal renders the tab — the local path reads the same pair out of
/// `workspace.active()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostSelection {
    project: ProjectKey,
    tab: TabKey,
    /// The local workspace's active tab when this selection was made.
    ///
    /// The override has to yield to anything that focuses a local tab,
    /// and not every such route goes through `focus_tab_and_clear` — an
    /// IPC `tab.focus`, a notification banner, a close that refocuses
    /// elsewhere all mutate the workspace and reach the UI as a
    /// reconcile. Watching the value those routes move is what covers
    /// them all at once, rather than a clear at each call site.
    local_active: i64,
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
        spawn_quit_signals(&runtime, &feed_tx)?;
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
            host_bells: HashSet::new(),
            host_selection: None,
            host_dialog: None,
            host_restarts: crate::host_conn::restart::RestartsInFlight::default(),
            bootstraps: bootstrap::BootstrapsInFlight::default(),
            add_host_name_id: Id::unique(),
            add_host_socket_id: Id::unique(),
            add_host_focus_requested: false,
            pending_host_selection: None,
            host_views: Vec::new(),
            host_sections: Vec::new(),
            runtime_handle: runtime.handle().clone(),
            feed_rx,
            feed_tx,
            _restore_quit_signals: RestoreDefaultQuitSignalsOnDrop,
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
    /// would otherwise make every launch wait on a dial. And the dial
    /// happens only where the build offers the localhost surface at all,
    /// which is [`reconnect_mode`]'s whole job (plan 041 §3.1).
    ///
    /// With no saved hosts this is a no-op over an empty list, which is
    /// the zero-change baseline.
    fn reconnect_saved_hosts(&mut self) {
        for host in self.workspace.hosts() {
            // Launch-time, so nobody asked and nobody is waiting: an
            // `Ipc` origin, same as `roostctl`'s, and an attempt no
            // person caused — which is what `AutoReconnect` says.
            self.connect_saved_host(
                &host,
                crate::host_conn::RequestOrigin::Ipc,
                |localhost| reconnect_mode(host_verbs::VerbPolicy::current(), localhost),
                crate::host_conn::AttemptCause::AutoReconnect,
            );
        }
        if !self.hosts.is_empty() {
            tracing::info!("probing saved host sessions");
        }
    }

    /// Classify a saved host's target and hand it to the connection set.
    /// `mode` answers what to do with a resolved target, and `None`
    /// declines the connection — which is how the launch probe skips a
    /// remote host without duplicating the classification.
    ///
    /// Three transports, two shapes. A local session and a socket path
    /// both resolve to a socket that already exists, so they dial
    /// straight away. An ssh target has no socket until a tunnel binds
    /// one, so it goes through [`crate::host_conn::HostConnSet::open_ssh`]
    /// and reaches `connect` one feed item later — see
    /// [`crate::engine_feed::EngineFeed::HostTunnel`].
    ///
    /// Every dial route ends here, which is why the shutdown gate is here
    /// too. `abandon_reconnects` rides the `ExitState` latch and so sweeps
    /// exactly once; a bootstrap job whose success arm asks for a
    /// reconnect, or an IPC `host.connect` serviced in a later `update`,
    /// would otherwise establish a fresh `ControlPersist` master that
    /// outlives the app. The two earlier guards stay where they are —
    /// they short-circuit before doing other work.
    fn connect_saved_host(
        &mut self,
        host: &roost_engine::persistence::HostSnapshot,
        origin: crate::host_conn::RequestOrigin,
        mode: impl FnOnce(bool) -> Option<crate::host_conn::ConnectMode>,
        cause: crate::host_conn::AttemptCause,
    ) {
        use crate::host_conn::HostTransport;
        use roost_ipc::ssh::ResolvedTransport;

        if self.exit_state != ExitState::Running {
            tracing::debug!(host = %host.id, "not connecting a host during shutdown");
            return;
        }
        // A new attempt replaces the origin and the failure an in-flight
        // probe's question was built on, so that probe is now asking
        // about something that is no longer happening: user Connect →
        // probe out → an IPC `host.connect` supersedes it → a consent
        // card raised at nobody. Superseding the probe is not enough —
        // nothing would re-arm it.
        self.cancel_bootstrap_probe(&host.id);
        let transport = match roost_ipc::ssh::classify(&host.target) {
            Ok(transport) => transport,
            Err(error) => {
                tracing::warn!(host = %host.id, ?error, "cannot resolve a saved host's target");
                return;
            }
        };
        let localhost = transport.is_localhost();
        let Some(mode) = mode(localhost) else {
            return;
        };
        let mode = spawn_gate(mode, host_verbs::VerbPolicy::current());
        // The one place the transport becomes the connection set's own
        // vocabulary. Everything downstream that used to ask "is this
        // localhost?" — the spawn ladder, the auto-retry policy, and now
        // what a build mismatch can offer — reads it off this value, so
        // the three answers cannot disagree.
        match transport {
            ResolvedTransport::LocalSession(socket) => self.hosts.connect(
                &host.id,
                &host.label,
                socket,
                HostTransport::LocalSession,
                mode,
                cause,
            ),
            ResolvedTransport::UnixSocket(socket) => self.hosts.connect(
                &host.id,
                &host.label,
                socket,
                HostTransport::UnixSocket,
                mode,
                cause,
            ),
            ResolvedTransport::Ssh(target) => {
                self.hosts
                    .open_ssh(&host.id, &host.label, target, mode, origin, cause)
            }
        }
    }

    /// An ssh tunnel finished coming up, or failed to. Dialing is the
    /// set's; the toast and the offer are the app's.
    fn host_tunnel_ready(&mut self, ready: crate::host_conn::HostTunnelReady) {
        // The same guard `host_reconnect_due` carries, for the window it
        // cannot see (plan 040 §3.4): `EngineFeed::Quit` only latches the
        // exit, so an establish that lands behind it in the same drain
        // would otherwise dial a session while the app is shutting down
        // — and `abandon_reconnects`, which runs after the drain, has
        // nothing left to abort by then. The tunnel is still retired
        // properly rather than dropped.
        if self.exit_state != ExitState::Running {
            tracing::debug!(host = %ready.host, "not connecting a host during shutdown");
            self.hosts.discard_ready(ready);
            return;
        }
        let saved_id = ready.host.clone();
        if let Some(reason) = self.hosts.tunnel_ready(ready) {
            self.set_status(reason);
        }
        // The establish is one of the two places a `NotFound` can land —
        // the other is a per-connection exec failing later, which
        // arrives as a state change (see `service_engine`).
        self.maybe_offer_bootstrap(&saved_id);
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
    fn engine_op<T, E>(
        &self,
        op: impl Future<Output = Result<T, E>> + Send + 'static,
        complete: impl FnOnce(Result<T, E>) -> EngineOpResult + Send + 'static,
    ) -> UiTask
    where
        T: Send + 'static,
        E: Send + 'static + From<String>,
    {
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
        // Before the match consumes it: a creation on a host owes the
        // selection the local path gets for free (plan 037 §3.9).
        self.arm_pending_host_selection(&result);
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
            EngineOpResult::HostVerified {
                generation,
                label,
                target,
                result,
            } => self.add_host_verified(generation, &label, &target, result),
            EngineOpResult::HostRestarted { saved_id, result } => {
                self.host_restart_completed(&saved_id, result)
            }
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
    ///
    /// The once-only edge is also where the host set is told to stop
    /// dialing (plan 040 §3.4). Here rather than in `Drop for App`,
    /// which is the other candidate: this is the edge where the
    /// *decision* to exit is made, so the abort lands before the run
    /// loop unwinds rather than in the middle of it — and the two of
    /// them are the same latch, so it cannot fire twice.
    pub fn take_exit_task(&mut self) -> UiTask {
        if self.exit_state.take() {
            self.hosts.abandon_reconnects();
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
            // The terminal on screen: the link underline follows the
            // cursor over the visible grid, which is the host's while a
            // host row is selected.
            let key = self.active_tab_key();
            if let Some(tab) = self.tabs.get_mut(&key) {
                if let Err(error) = tab
                    .set_link_modifier_held(held)
                    .and_then(|()| tab.refresh_snapshot())
                {
                    tracing::warn!(
                        ?error,
                        tab_id = key.tab,
                        "terminal link hover refresh failed"
                    );
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
        if matches!(self.keyboard_route(), KeyboardRoute::HostDialog) {
            let mut task = UiTask::None;
            if let keyboard::Event::KeyPressed { key, repeat, .. } = &event {
                match (key.as_ref(), repeat) {
                    (Key::Named(Named::Escape), false) => {
                        self.rename_completion_key = Some(RenameCompletionKey::Escape);
                        self.host_dialog_cancel();
                    }
                    (Key::Named(Named::Enter), false) => {
                        self.rename_completion_key = Some(RenameCompletionKey::Enter);
                        // One key, four dialogs: Add Host's Enter is its
                        // "Add & Connect", and the other three are their
                        // confirming buttons. Each is that panel's
                        // primary action, which is what Enter means in a
                        // dialog — and an upgrade prompt for a host
                        // nothing can be offered for has only "Close",
                        // so Enter is that.
                        match &self.host_dialog {
                            Some(host_dialog::HostDialog::Add(_)) => task = self.submit_add_host(),
                            Some(host_dialog::HostDialog::ConfirmStop { .. }) => {
                                self.host_stop_confirmed();
                            }
                            Some(host_dialog::HostDialog::ConfirmRestart { .. }) => {
                                task = self.host_restart_dialog_confirmed();
                            }
                            Some(host_dialog::HostDialog::Bootstrap(_)) => {
                                self.host_bootstrap_confirmed();
                            }
                            None => {}
                        }
                    }
                    _ => {}
                }
            }
            // The modal owns the keyboard. `TextInput` still sees
            // printable input through its own widget events; nothing
            // leaks to an accelerator or the active PTY.
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
            KeyboardRoute::HostDialog => {
                self.host_dialog_cancel();
                UiTask::None
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
                || self.host_dialog.is_some()
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
                // The toast alone is silent to anything outside the
                // window (a test harness reading the log, a bug report
                // after the fact) — this is the one boundary every
                // keybind refusal passes through.
                tracing::info!(%error, "keybind action refused");
                self.set_status(error);
                UiTask::None
            }
        }
    }

    fn dispatch_keybind_action_once(&mut self, action: KeybindAction) -> Result<UiTask, String> {
        match action {
            KeybindAction::NewTab => Ok(self.new_tab_dispatch().task),
            KeybindAction::CloseTab => {
                // The selection, not the local workspace's: with a host
                // row showing, the tab under the keybind is that host's,
                // and closing the hidden local one would be a destructive
                // action on something the user cannot even see.
                let tab = self.active_tab_key();
                // The sentinel id 0 is the empty local workspace's; no
                // host row is ever keyed at it.
                if tab.tab == 0 {
                    return Ok(UiTask::None);
                }
                Ok(self.close_tab_dispatch(tab).task)
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
            KeybindAction::Paste => self.paste_into_active(ClipboardOp::System),
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
            // ⌘⇧N opens the picker as its own root frame rather than
            // drilling through the command palette — the shifted form of
            // ⌘N is "ask me which host", not "go find the row".
            KeybindAction::NewProjectOnHost => {
                self.open_bound_palette_result(palettes::HOST_PICKER_FRAME_ID)
            }
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
        self.rename_editor.is_none() && self.confirm_delete.is_none() && self.host_dialog.is_none()
    }

    /// The active project, host-qualified. The workspace's active
    /// selection is a pinned-bare boundary, so this and
    /// [`Self::active_tab_key`] are the joints that qualify it at the
    /// backend that owns it.
    fn active_project_key(&self) -> ProjectKey {
        match self.host_selection {
            Some(selection) => selection.project,
            None => ProjectKey::new(self.backend.host(), self.workspace.active().0),
        }
    }

    /// [`Self::active_project_key`]'s twin: the one place the workspace's
    /// bare active tab id becomes a key.
    fn active_tab_key(&self) -> TabKey {
        match self.host_selection {
            Some(selection) => selection.tab,
            None => self.backend.tab_key(self.workspace.active().1),
        }
    }

    /// The key an event off the terminal widget names. The widget renders
    /// the selected tab and stamps its bare id onto every pointer, wheel
    /// and hover event, so an id that still matches the selection belongs
    /// to whatever host that selection is on — qualifying it at the local
    /// backend would land the gesture on whichever LOCAL tab happens to
    /// share the number while a host terminal is showing. An id that no
    /// longer matches is a straggler from a previous frame and keeps
    /// resolving exactly as it did before, at the local backend.
    ///
    /// With no host selection both branches are the same key, which is
    /// what keeps the zero-host path byte-identical.
    pub(super) fn terminal_event_key(&self, tab_id: i64) -> TabKey {
        terminal_event_key(self.active_tab_key(), self.backend.host(), tab_id)
    }

    fn keyboard_route(&self) -> KeyboardRoute {
        let active_tab = self.active_tab_key();
        resolve_keyboard_route(
            self.confirm_delete.is_some(),
            self.host_dialog.is_some(),
            self.rename_editor.is_some(),
            self.palette.is_some(),
            active_tab,
            active_terminal_live(
                self.tabs.contains_key(&active_tab),
                self.frozen_host_frame().is_some(),
            ),
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
        // The same statement the local workspace just took, for whichever
        // session owns the selected tab: unfocusing releases the claim,
        // refocusing re-states it.
        self.push_host_focus();
        if let Some(tab) = self.tabs.get(&self.active_tab_key()) {
            tab.set_window_focus(focused);
        }
        if focused {
            self.refresh_notification_authorization();
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let started = Instant::now();
        let content = self.view_body();
        // At most one modal is ever up: each of the three cancels the
        // others where it opens, so this is a preference order and not a
        // stack.
        let Some(modal) = self
            .confirm_delete_modal()
            .or_else(|| self.host_dialog_modal())
        else {
            crate::perf::record_view(started.elapsed());
            return content;
        };
        let result = modal_over(content, modal);
        crate::perf::record_view(started.elapsed());
        result
    }

    /// The delete-project confirmation, as a modal.
    fn confirm_delete_modal(&self) -> Option<Modal<'_>> {
        let confirm = self.confirm_delete.as_ref()?;
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
        let card = modal_card(column![
            heading,
            modal_buttons(
                "Cancel",
                Message::ConfirmDeleteCancel,
                Some(ConfirmButton {
                    label: "Close Project",
                    style: chrome::danger_button,
                    press: Some(Message::ConfirmDeleteConfirm),
                }),
            )
        ]);
        Some(Modal {
            card,
            card_pressed: Message::ConfirmDeleteCardPressed,
            dismiss: Message::ConfirmDeleteCancel,
        })
    }

    /// Add Host, the Stop Session confirmation, or the upgrade prompt
    /// (plan 037 §3.1, §3.7).
    ///
    /// One builder for all three because they share the panel: the
    /// mock's Add Host dialog and its "Restart session" confirmation are
    /// the same card with different contents, and the delete
    /// confirmation above is the fourth instance of it.
    fn host_dialog_modal(&self) -> Option<Modal<'_>> {
        let body = match self.host_dialog.as_ref()? {
            host_dialog::HostDialog::Add(draft) => self.add_host_body(draft),
            host_dialog::HostDialog::ConfirmStop { label, .. } => column![
                modal_heading(stop_session_title(label), STOP_SESSION_BODY),
                modal_buttons(
                    "Cancel",
                    Message::HostDialogCancel,
                    Some(ConfirmButton {
                        label: STOP_SESSION_CONFIRM,
                        style: chrome::danger_button,
                        press: Some(Message::HostStopConfirm),
                    }),
                )
            ],
            host_dialog::HostDialog::ConfirmRestart { prompt, .. } => column![
                modal_heading(prompt.title.clone(), &prompt.body),
                // A host nothing can be offered for gets the state and
                // the pointer, and no button that would fail (§3.1).
                // The other two differ only in what the button promises;
                // which flow it starts is `host_restart_dialog_confirmed`'s
                // to decide, so Enter and a click cannot route differently.
                modal_buttons(
                    prompt.dismiss_label(),
                    Message::HostDialogCancel,
                    prompt.confirm.as_deref().map(|label| ConfirmButton {
                        label,
                        style: chrome::danger_button,
                        press: Some(Message::HostRestartConfirm),
                    }),
                )
            ],
            // Consent to touch a host over ssh (plan 039 §3.5). The card
            // is the offer: what, where, from where, and — for an update
            // over a live session — what it ends.
            host_dialog::HostDialog::Bootstrap(draft) => column![
                modal_heading(draft.copy.title.clone(), &draft.copy.body),
                modal_buttons(
                    "Cancel",
                    Message::HostDialogCancel,
                    Some(ConfirmButton {
                        label: draft.copy.confirm,
                        style: chrome::primary_button,
                        press: Some(Message::HostBootstrapConfirm),
                    }),
                )
            ],
        };
        Some(Modal {
            card: modal_card(body),
            card_pressed: Message::HostDialogCardPressed,
            dismiss: Message::HostDialogCancel,
        })
    }

    /// The Add Host dialog's contents: two fields, an inline error, two
    /// buttons — the approved mock, widget for widget.
    fn add_host_body(&self, draft: &host_dialog::AddHostDraft) -> Column<'_, Message> {
        let mut body = column![
            modal_heading(ADD_HOST_TITLE.to_string(), ADD_HOST_BODY),
            dialog_field(
                "Name",
                "pop-os",
                &draft.name,
                self.add_host_name_id.clone(),
                Message::AddHostNameChanged,
            ),
            dialog_field(
                "Target",
                "workbox, user@host, or /path/to.sock",
                &draft.socket,
                self.add_host_socket_id.clone(),
                Message::AddHostSocketChanged,
            ),
        ];
        if let Some(error) = &draft.error {
            body = body.push(text(error.clone()).size(11).color(chrome::ERROR_TEXT));
        }
        // Inert while the dial is in flight rather than hidden: a button
        // that vanishes mid-press moves the card under the pointer.
        let confirm = (!draft.is_verifying()).then_some(Message::AddHostSubmit);
        body.push(modal_buttons(
            "Cancel",
            Message::HostDialogCancel,
            Some(ConfirmButton {
                label: draft.confirm_label(),
                style: chrome::primary_button,
                press: confirm,
            }),
        ))
    }

    /// One sidebar project row plus its agent rows — the same widgets
    /// under every host, which is plan 037 §3.1's "project rows render
    /// identically" made literal: there is one builder, and the section
    /// header above a row is the only thing that says where it runs.
    ///
    /// `dim` is the disconnected-section treatment. Colors drop to
    /// [`chrome::HOST_SECTION_DIM`] and no row publishes a message, so a
    /// click and a keyboard traversal both pass the section by — those
    /// shells are still running on the host, but nothing here can act on
    /// them until the connection is back. With `dim` false this is the
    /// local sidebar's path, widget for widget.
    fn sidebar_project_group<'a>(
        &'a self,
        project: &'a Project,
        project_key: ProjectKey,
        dragged_project: Option<i64>,
        dim: bool,
    ) -> Element<'a, Message> {
        let active = project_key == self.active_project_key();
        let active_key = self.active_tab_key();
        let hide_agent_rows = agent_rows_hidden(self.project_drag_preview.as_ref());
        let alpha = if dim { chrome::HOST_SECTION_DIM } else { 1.0 };
        let rollup = project_rollup(
            project
                .tabs
                .iter()
                .map(|tab| agent::effective_lifecycle(&tab.agent_state())),
        );
        let notifying = project.tabs.iter().any(|tab| tab.has_notification);
        let stripe = container(
            iced::widget::Space::new()
                .width(chrome::PROJECT_STRIPE_WIDTH)
                .height(chrome::ROW_HEIGHT - 2.0 * chrome::PROJECT_STRIPE_INSET_Y),
        )
        .style(move |_| {
            let color = if rollup == roost_ipc::agent::AgentLifecycle::Inactive {
                Color::TRANSPARENT
            } else {
                agent_color(rollup).scale_alpha(alpha)
            };
            iced::widget::container::Style::default()
                .background(color)
                .border(iced::border::rounded(2))
        });
        let project_label: Element<'a, Message> = match self.rename_editor.as_ref() {
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
                }
                .scale_alpha(alpha);
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
        // Local rows take their click from the reorder strip that wraps
        // them (it owns press, drag and double-click as one gesture). A
        // host section sits outside that strip — reordering a host's
        // projects is a workspace mutation, which routes through the op
        // queue rather than the local client — so its rows carry their
        // own press, and a dimmed one carries none.
        let project_row: Element<'a, Message> = if project_key.is_local() || dim {
            project_row.into()
        } else {
            mouse_area(project_row)
                .on_press(Message::ProjectSelected(project_key))
                .into()
        };
        let mut project_group = column![project_row].spacing(2);
        if self.config.show_sidebar_agents && !hide_agent_rows {
            for agent in self.sidebar_agents.get(&project_key).into_iter().flatten() {
                let name = agent.name.clone();
                let detail = format!("{} · {}", agent.status_text, agent.time_text);
                let dot_color = agent_color(agent.lifecycle).scale_alpha(alpha);
                let dot =
                    container(iced::widget::Space::new().width(8).height(8)).style(move |_| {
                        iced::widget::container::Style::default()
                            .background(dot_color)
                            .border(iced::border::rounded(3))
                    });
                let agent_row = row![
                    dot,
                    text(name).size(11),
                    text(detail)
                        .size(9)
                        .color(chrome::MUTED_TEXT.scale_alpha(alpha))
                ]
                .spacing(6)
                .align_y(Alignment::Center);
                // A dimmed row keeps its `button` so the layout is
                // identical; without `on_press` iced renders it Disabled,
                // which is the same reduced-contrast reading the colors
                // above give the rest of the row.
                let mut agent_button = button(agent_row)
                    .width(Fill)
                    .height(chrome::ROW_HEIGHT)
                    .padding(iced::Padding {
                        top: 3.0,
                        right: 8.0,
                        bottom: 3.0,
                        left: chrome::AGENT_DOT_INSET,
                    })
                    .style(chrome::agent_button(agent.tab == active_key));
                if !dim {
                    agent_button = agent_button.on_press(Message::AgentSelected(agent.tab));
                }
                project_group = project_group.push(agent_button);
            }
        }
        project_group.into()
    }

    /// One host section's band: the dot, the uppercase label, and the
    /// right-aligned rollup. Same band as the "PROJECTS" header it
    /// replaces — `chrome::band`, [`chrome::BAND_HEIGHT`], the same 11pt
    /// semibold [`chrome::MUTED_TEXT`] label — so the sidebar gains a
    /// structure without gaining a second visual language.
    fn host_band<'a>(&'a self, section: &'a host_sidebar::Section) -> Element<'a, Message> {
        let dot_color = match section.state.dot() {
            host_sidebar::HostDot::Connected => chrome::HOST_DOT_CONNECTED,
            host_sidebar::HostDot::Pending => chrome::HOST_DOT_PENDING,
            host_sidebar::HostDot::Offline => chrome::HOST_DOT_OFFLINE,
        };
        let dot = container(
            iced::widget::Space::new()
                .width(chrome::HOST_DOT_SIZE)
                .height(chrome::HOST_DOT_SIZE),
        )
        .style(move |_| {
            iced::widget::container::Style::default()
                .background(dot_color)
                .border(iced::border::rounded(chrome::HOST_DOT_SIZE / 2.0))
        });
        let mut band = row![
            dot,
            sidebar_band_label(&section.label),
            iced::widget::Space::new().width(Fill)
        ]
        .spacing(chrome::HOST_BAND_SPACING)
        .align_y(Alignment::Center);
        if let Some(rollup) = &section.rollup {
            band = band.push(
                text(rollup.as_str())
                    .size(chrome::HOST_ROLLUP_SIZE)
                    .color(chrome::HOST_ROLLUP_TEXT),
            );
        }
        sidebar_band(band)
    }

    /// The inline "↻ Reconnect" row under a section that is not
    /// connected — the only affordance a dimmed section offers. The same
    /// verb lives in the palette (C7).
    fn host_reconnect_row(&self, saved_id: &str) -> Element<'_, Message> {
        button(
            text("↻ Reconnect")
                .size(12)
                .color(chrome::HOST_RECONNECT_TEXT),
        )
        .width(Fill)
        .padding([6, 12])
        .style(chrome::transparent_button)
        .on_press(Message::HostReconnect(saved_id.to_string()))
        .into()
    }

    fn view_body(&self) -> Element<'_, Message> {
        // `self.projects` is this backend's snapshot, so every id read out
        // of it below qualifies at this backend's instance.
        let host = self.backend.host();
        // The selection, which is the local workspace's unless a host row
        // is showing (`host_selection`). With no saved hosts these are
        // exactly `workspace.active()`, qualified at this backend.
        let active_project_key = self.active_project_key();
        let active_key = self.active_tab_key();
        let active_project = active_project_key.project;
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
        let mut sidebar_body = column![].spacing(2).padding([4, 0]);
        for project in visual_project_ids.iter().filter_map(|project_id| {
            self.projects
                .iter()
                .find(|project| project.id == *project_id)
        }) {
            let project_key = ProjectKey::new(host, project.id);
            sidebar_body = sidebar_body.push(self.sidebar_project_group(
                project,
                project_key,
                dragged_project,
                false,
            ));
        }
        let sidebar_header = || sidebar_band(sidebar_band_label("PROJECTS"));
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
        // The two sidebars (plan 037 §3.1). With no saved hosts this is
        // exactly today's — one sticky "PROJECTS" band over the strip.
        // With hosts the band becomes one per host and moves *into* the
        // scroll flow, because a section header only means anything
        // sitting directly above the rows it names.
        let sections = &self.host_sections;
        let (sidebar_header, sidebar_list): (Option<Element<'_, Message>>, Element<'_, Message>) =
            if sections.is_empty() {
                (
                    Some(sidebar_header()),
                    scrollable(project_strip).height(Fill).into(),
                )
            } else {
                let mut list = column![self.host_band(&sections[0]), project_strip];
                for (section, view) in sections[1..].iter().zip(&self.host_views) {
                    list = list.push(self.host_band(section));
                    let dim = !section.state.interactive();
                    let mut rows = column![].spacing(2).padding([4, 0]);
                    for project in &view.projects {
                        let project_key = ProjectKey::new(view.host, project.id);
                        rows =
                            rows.push(self.sidebar_project_group(project, project_key, None, dim));
                    }
                    list = list.push(rows);
                    if section.state.offers_reconnect() {
                        list = list.push(self.host_reconnect_row(&view.saved_id));
                    }
                }
                (None, scrollable(list).height(Fill).into())
            };
        let sidebar_list = container(sidebar_list)
            .width(Fill)
            .height(Fill)
            .style(chrome::list);
        let mut sidebar_column = column![];
        if let Some(header) = sidebar_header {
            sidebar_column = sidebar_column.push(header);
        }
        // The hairline lives inside the sidebar's own width — the outer
        // container paints it and pads the three region fills off it — so the
        // terminal grid keeps every pixel `sidebar_width` leaves it and the
        // resize grip's seam still lands on the sidebar's right edge.
        let sidebar = container(sidebar_column.push(sidebar_list).push(sidebar_footer))
            .width(self.live_sidebar_width())
            .height(Fill)
            .padding(iced::Padding::default().right(chrome::DIVIDER_WIDTH))
            .style(chrome::divider);

        // The tab bar renders the selected project's tabs, whichever host
        // it lives on — the pills themselves are host-blind, so only the
        // list they come from changes.
        let active_project_model = if active_project_key.is_local() {
            self.projects
                .iter()
                .find(|project| project.id == active_project)
        } else {
            self.host_views
                .iter()
                .find(|view| view.host == active_project_key.host)
                .and_then(|view| {
                    view.projects
                        .iter()
                        .find(|project| project.id == active_project)
                })
        };
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
            let tab_key = TabKey::new(active_project_key.host, tab.id);
            let title = pill_display_title(&tab.title);
            let active = tab_key == active_key;
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
            let pill_container = container(pill)
                .id(tab_pill_id(tab_key))
                .width(pill_width)
                .height(chrome::PILL_HEIGHT)
                .padding([0.0, chrome::TAB_PILL_PADDING_X])
                .style(chrome::tab_pill(
                    active,
                    self.tab_drag_preview
                        .as_ref()
                        .is_some_and(|preview| preview.drags(tab.id)),
                ));
            // Local pills take their click from the strip below, which
            // owns press + drag + double-click-to-rename as one gesture.
            // A host project's strip is disabled (reordering its tabs is
            // an op-queue mutation, not a local reorder), so its pills
            // carry the plain press themselves.
            tab_pills = tab_pills.push(if tab_key.is_local() {
                Element::from(pill_container)
            } else {
                mouse_area(pill_container)
                    .on_press(Message::TabSelected(tab_key))
                    .into()
            });
        }
        let tab_strip = ReorderStrip::tabs(
            tab_pills,
            active_project_key.host,
            active_project,
            visual_tab_ids,
            self.tab_strip_generation,
            self.strip_gestures_enabled() && active_project_key.is_local(),
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
                tab_id: active_key.tab,
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
        // A frozen host frame keeps its pixels and says why (plan 037
        // §3.1). With no host selection this is `None` and the terminal
        // element goes through untouched.
        let terminal = match self.host_frame_banner() {
            Some((saved_id, frame, banner)) => frozen_frame(terminal, saved_id, frame, banner),
            None => terminal,
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
        // Focusing the row's preferred tab is what selects the project —
        // on either side of the ring there is no separate selection to
        // set, since the sidebar draws off `active_project_key`.
        let Some(tab) = self.preferred_tab_key(project) else {
            tracing::debug!(?project, "project selection resolves to no tab");
            return;
        };
        let _ = self.focus_tab_and_clear(tab, false);
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
        // Creation follows context (plan 037 §3.1): a tab opens on the
        // host its project lives on, never on a different one.
        let project = self.active_project_key();
        if creation_target(project.host) != CreationTarget::Local {
            return self.open_host_tab_dispatch(project);
        }
        let (project_id, _) = self.workspace.active();
        if project_id == 0 {
            return EngineDispatch::default();
        }
        let cwd = self.launch_cwd(project_id);
        self.open_tab_dispatch(project_id, cwd, String::new(), Vec::new())
    }

    /// ⌘T / the tab bar's "+" on a host project (plan 037 §3.1: a tab
    /// never lands on a different host than its project).
    ///
    /// Event-confirmed like every other host mutation: the reply names
    /// the new id, and the selection waits for the mirror to list it
    /// (`arm_pending_host_selection`).
    fn open_host_tab_dispatch(&mut self, project: ProjectKey) -> EngineDispatch {
        let Some(ops) = self.hosts.ops_for(project.host).cloned() else {
            self.set_status("that host is not accepting operations".to_string());
            return EngineDispatch::default();
        };
        // The project's own cwd, off the mirror — the host's answer to
        // "where does a new tab here start", and the only cwd this side
        // knows that means anything over there.
        let cwd = self
            .host_project_row(project)
            .map(|(_, row)| row.cwd.clone())
            .unwrap_or_default();
        let op = self.take_engine_op_id();
        let project_id = project.project;
        EngineDispatch {
            task: self.engine_op(
                async move { open_host_tab_flow(ops, project_id, cwd).await },
                move |result| EngineOpResult::TabOpened {
                    op,
                    project,
                    result: result.map(|tab_id| TabKey::new(project.host, tab_id)),
                },
            ),
            op: Some(op),
        }
    }

    /// ⌘N / "+ New Project" when the selected project lives on a host,
    /// and every row of the ⌘⇧N picker but LOCAL.
    fn create_host_project_dispatch(&mut self, host: HostId) -> EngineDispatch {
        let Some(ops) = self.hosts.ops_for(host).cloned() else {
            self.set_status("that host is not accepting operations".to_string());
            return EngineDispatch::default();
        };
        let op = self.take_engine_op_id();
        EngineDispatch {
            task: self.engine_op(
                async move { create_host_project_flow(ops).await },
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
        let host = self.active_project_key().host;
        self.create_project_on(host)
    }

    /// The create-project route, host-qualified (plan 037 §3.1's
    /// "creation follows context"): ⌘N and "+ New Project" pass the
    /// selected project's host, the ⌘⇧N picker passes whichever host the
    /// user chose.
    fn create_project_on(&mut self, host: HostId) -> EngineDispatch {
        self.set_sidebar_collapsed(false);
        if let CreationTarget::Host(host) = creation_target(host) {
            return self.create_host_project_dispatch(host);
        }
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

    /// The rows an instance owns: the local snapshot for the local
    /// backend, that host's mirrored rows for a host.
    ///
    /// `None` for a host that is not connected — the same gate
    /// `host_project_row` applies, so a dimmed section's project cannot
    /// be renamed, closed, or confirmed against from any route. Stated
    /// once because every route that resolves a key to rows has to apply
    /// the same gate, or two of them would disagree about whether the
    /// same section is actionable.
    pub(super) fn instance_rows(&self, host: HostId) -> Option<&[Project]> {
        if host.is_local() {
            return Some(self.projects.as_slice());
        }
        self.interactive_host_view(host)
            .map(|view| view.projects.as_slice())
    }

    /// [`Self::instance_rows`] for a project key.
    fn project_rows(&self, project: ProjectKey) -> Option<&[Project]> {
        self.instance_rows(project.host)
    }

    /// Re-resolve an open delete confirmation against the rows it names.
    ///
    /// The `take` is what lets the row lookup and the write coexist: the
    /// rows borrow `self`, so the value being rewritten has to be off it
    /// while they are held.
    pub(super) fn reconcile_confirm_delete(&mut self) {
        let mut confirm = self.confirm_delete.take();
        if let Some(project) = confirm.as_ref().map(|open| open.project) {
            match self.project_rows(project) {
                Some(rows) => reconcile_confirm_delete(&mut confirm, rows),
                // The host dropped while the dialog was up. Closing it
                // is the same answer a deleted local project gets.
                None => confirm = None,
            }
        }
        self.confirm_delete = confirm;
    }

    fn confirm_close_project(&mut self, project: ProjectKey) -> Result<(), String> {
        let Some(target) = self
            .project_rows(project)
            .and_then(|rows| confirm_delete_target(rows, project))
        else {
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
            // A host's project: the delete goes on that host's queue and
            // the mirror's event is what retires the section's rows
            // (plan 037 §3.9, event-confirmed — a refused delete leaves
            // the sidebar honest). The reply is awaited all the same, so
            // "refused" is something the user is told rather than
            // something only the log knows.
            let Some(ops) = self.hosts.ops_for(project.host).cloned() else {
                self.set_status("that host is not accepting operations".to_string());
                return UiTask::None;
            };
            let host_project_id = project.project;
            return self.engine_op(
                async move { delete_host_project_flow(ops, host_project_id).await },
                move |result| EngineOpResult::ProjectDeleted { project, result },
            );
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
            return self.close_host_tab_dispatch(tab);
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

    /// Close a tab on the host that owns it: the intent goes on that
    /// host's op queue, and the mirror's `tab.closed` event is what
    /// retires the row (plan 037 §3.9 — event-confirmed, never
    /// optimistic, so a refused close leaves the sidebar honest).
    ///
    /// The reply is awaited exactly as the local close's is, and for the
    /// same reason: a refusal the user never sees is a row that silently
    /// did not go away. The op id comes with that — a `palette.activate`
    /// closing a host tab now hears the host's verdict rather than an
    /// immediate "fine" the host may be about to contradict.
    fn close_host_tab_dispatch(&mut self, tab: TabKey) -> EngineDispatch {
        let Some(ops) = self.hosts.ops_for(tab.host).cloned() else {
            self.set_status("that host is not accepting operations".to_string());
            return EngineDispatch::default();
        };
        let op = self.take_engine_op_id();
        let tab_id = tab.tab;
        EngineDispatch {
            task: self.engine_op(
                async move { close_host_tab_flow(ops, tab_id).await },
                move |result| EngineOpResult::TabClosed { op, tab, result },
            ),
            op: Some(op),
        }
    }

    /// The tabs the tab bar is showing, in strip order — the selected
    /// project's, whichever host it lives on. Tab hotkeys walk exactly
    /// this, which is what "tab hotkeys walk the visible tab bar
    /// unchanged" means once a host project can be the selected one.
    fn visible_tab_keys(&self) -> Vec<TabKey> {
        let project = self.active_project_key();
        let Some(project_id) = project.local_project() else {
            return self
                .host_project_row(project)
                .map(|(_, row)| {
                    row.tabs
                        .iter()
                        .map(|tab| TabKey::new(project.host, tab.id))
                        .collect()
                })
                .unwrap_or_default();
        };
        self.workspace
            .snapshot()
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| {
                project
                    .tabs
                    .iter()
                    .map(|tab| self.backend.tab_key(tab.id))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn cycle_tab(&mut self, delta: isize) -> Result<(), String> {
        let tabs = self.visible_tab_keys();
        if tabs.is_empty() {
            return Ok(());
        }
        let active_tab = self.active_tab_key();
        let current = tabs.iter().position(|tab| *tab == active_tab).unwrap_or(0);
        let Some(next) = clamped_tab_index(current, tabs.len(), delta) else {
            return Ok(());
        };
        if next == current {
            return Ok(());
        }
        self.focus_tab_and_clear(tabs[next], false)?;
        Ok(())
    }

    /// Every section the navigation ring walks, top to bottom. The local
    /// workspace leads; a saved host contributes its mirrored projects,
    /// and a section that is not connected is listed but never traversed
    /// (plan 037 §3.1).
    fn ring_sections(&self) -> Vec<host_sidebar::RingSection> {
        // The local rows come off a fresh snapshot, not the reconciled
        // cache: `switch_project_N` resolved against `workspace.snapshot()`
        // before the ring existed, and a dispatch that arrives ahead of
        // the reconcile carrying a reorder must still count the same way
        // it did then. A host's rows have no such source — its mirror is
        // the only copy there is.
        let local = host_sidebar::RingSection {
            host: self.backend.host(),
            navigable: true,
            projects: self
                .workspace
                .snapshot()
                .iter()
                .map(|project| project.id)
                .collect(),
        };
        if self.host_views.is_empty() {
            return vec![local];
        }
        let mut sections = Vec::with_capacity(self.host_views.len() + 1);
        sections.push(local);
        sections.extend(
            self.host_views
                .iter()
                .map(|view| host_sidebar::RingSection {
                    host: view.host,
                    navigable: view.state.interactive(),
                    projects: view.projects.iter().map(|row| row.id).collect(),
                }),
        );
        sections
    }

    fn switch_project_by_index(&mut self, index: u8) -> Result<(), String> {
        let Some(project) = host_sidebar::ring_index(&self.ring_sections(), index) else {
            return Ok(());
        };
        let Some(tab) = self.preferred_tab_key(project) else {
            return Ok(());
        };
        self.focus_tab_and_clear(tab, false)
    }

    /// The tab a project row lands on, on either side of the ring.
    fn preferred_tab_key(&self, project: ProjectKey) -> Option<TabKey> {
        match project.local_project() {
            Some(project_id) => self
                .workspace
                .preferred_tab(project_id)
                .map(|tab_id| self.backend.tab_key(tab_id)),
            None => self.host_preferred_tab(project),
        }
    }

    fn switch_tab_by_index(&mut self, index: u8) -> Result<(), String> {
        let Some(tab) = index
            .checked_sub(1)
            .and_then(|index| self.visible_tab_keys().get(usize::from(index)).copied())
        else {
            return Ok(());
        };
        self.focus_tab_and_clear(tab, false)
    }

    /// Every focus change in the UI — strip clicks, sidebar rows, the
    /// cycle/switch keybinds, jump-to-unread, the agent and notification
    /// palettes — funnels through here, so this is the one place that owes
    /// them a reconcile.
    fn focus_tab_and_clear(&mut self, tab: TabKey, reveal_sidebar: bool) -> Result<(), String> {
        if !tab.is_local() {
            return self.focus_host_tab_and_clear(tab, reveal_sidebar);
        }
        // A local focus ends any host selection: the two are one
        // selection, and the local workspace is the one that persists it.
        self.set_host_selection(None);
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

    /// [`Self::focus_tab_and_clear`] for a connected host's tab: record
    /// the selection the view renders from, then attach (C5's entry —
    /// attach-on-focus detaches whatever was attached before).
    ///
    /// Refused for a host that is not connected, which is the other half
    /// of "dimmed rows are non-interactive": the row publishes nothing,
    /// and a selection arriving by any other route (a stale palette row,
    /// a notification jump) still lands nowhere.
    fn focus_host_tab_and_clear(
        &mut self,
        tab: TabKey,
        reveal_sidebar: bool,
    ) -> Result<(), String> {
        let Some(project) = self.host_project_of(tab) else {
            return Err(format!("tab {tab} is not listed by a connected host"));
        };
        self.set_host_selection(Some(HostSelection {
            project,
            tab,
            local_active: self.workspace.active().1,
        }));
        if reveal_sidebar {
            self.set_sidebar_collapsed(false);
        }
        self.host_clear_notification(tab);
        self.host_focus_tab(tab);
        self.reconcile();
        Ok(())
    }

    /// The "and clear" half for a host tab — `focus_tab_in_core`'s
    /// counterpart, which is what the local path gets it from.
    ///
    /// Fire-and-forget, and **event-confirmed**: the session answers by
    /// committing `tab.notification { has_pending: false }`, and that
    /// envelope is what retires the row and the desktop banner (plan 037
    /// §3.9's no-optimistic-rows rule). Clearing here as well would take
    /// the row down before the host agreed, and put it back on the next
    /// reconcile if the op was refused.
    fn host_clear_notification(&mut self, tab: TabKey) {
        // The bell half is ours alone: the session kept no flag for it,
        // so nothing coming back over the wire would ever retire it.
        self.host_bells.remove(&tab);
        let intent = crate::host_conn::HostIntent::new(
            roost_ipc::messages::ops::TAB_CLEAR_NOTIFICATION,
            serde_json::json!({ "tab_id": tab.tab.to_string() }),
        );
        if self.hosts.send_at(tab.host, intent).is_err() {
            tracing::debug!(%tab, "could not clear the attention marker on a host tab");
        }
    }

    /// The cached view of the host serving an incarnation, when its rows
    /// are live enough to act on. Dimmed sections answer `None`, which is
    /// what makes them non-interactive from every route at once — the
    /// sidebar click, the palette row, a notification jump.
    fn interactive_host_view(&self, host: HostId) -> Option<&HostView> {
        self.host_views
            .iter()
            .find(|view| view.host == host && view.state.interactive())
    }

    /// The cached view of the host serving an incarnation, whatever
    /// state its section is in. The ungated lookup, for the questions
    /// that are about what the window is *showing* rather than what the
    /// user may act on.
    fn host_view(&self, host: HostId) -> Option<&HostView> {
        self.host_views.iter().find(|view| view.host == host)
    }

    /// The host whose frame the window is showing, when that frame is
    /// frozen (plan 037 §3.1).
    ///
    /// `None` unless a host row is selected *and* that host reached one
    /// of the two terminal states — so with no host selection, and on
    /// every ordinary connected frame, this costs one `Option` check.
    fn frozen_host_frame(&self) -> Option<(&HostView, host_notice::FrozenFrame)> {
        let selection = self.host_selection?;
        let view = self.host_view(selection.tab.host)?;
        let section = self.hosts.section(&view.saved_id)?;
        Some((view, host_notice::frozen_frame(section.state)?))
    }

    /// [`Self::frozen_host_frame`]'s twin for a specific tab rather than
    /// whichever host row is currently selected — what a keybind dispatch
    /// that already resolved its own [`TabKey`] (paste, most notably) has
    /// to ask instead. `is_local` short-circuits before the lookup: a
    /// never-connected saved host's [`HostView::host`] is also
    /// `HostId::LOCAL` as a placeholder, and a local tab must never match
    /// that entry.
    fn frozen_host_frame_for(&self, tab: TabKey) -> Option<host_notice::FrozenFrame> {
        if tab.is_local() {
            return None;
        }
        let view = self.host_view(tab.host)?;
        let section = self.hosts.section(&view.saved_id)?;
        host_notice::frozen_frame(section.state)
    }

    /// The banner the window owes the frame it is showing, the host its
    /// button reconnects, and the frame that button is a promise about.
    /// The wording is composed here, at the draw, so the reconcile can
    /// ask the same question without it.
    fn host_frame_banner(
        &self,
    ) -> Option<(&str, host_notice::FrozenFrame, host_notice::HostBanner)> {
        let (view, frozen) = self.frozen_host_frame()?;
        Some((view.saved_id.as_str(), frozen, frozen.banner(&view.label)))
    }

    /// The frozen-frame banner's button (plan 037 §3.1).
    ///
    /// Re-checked against the host's state **now**, not against the
    /// frame that was drawn: see [`host_notice::click_still_lands`] for
    /// the two ways a stale click does damage.
    pub fn host_frame_reconnect_requested(
        &mut self,
        saved_id: &str,
        frame: host_notice::FrozenFrame,
    ) {
        let current = self
            .hosts
            .section(saved_id)
            .and_then(|section| host_notice::frozen_frame(section.state));
        if !host_notice::click_still_lands(frame, current) {
            tracing::debug!(
                host = %saved_id,
                ?frame,
                ?current,
                "banner click ignored: the host is no longer showing that frame"
            );
            return;
        }
        self.host_connect_requested(saved_id, crate::host_conn::RequestOrigin::User);
    }

    /// The project a host *lists* a tab under, whatever state its
    /// section is in.
    ///
    /// [`Self::host_project_of`]'s twin for the one question that is not
    /// about acting on a row: is the window still showing something this
    /// host told us about? A taken-over or stopped session answers yes —
    /// its rows are still listed, dimmed — which is what lets the frozen
    /// frame stay up instead of snapping back to a local tab.
    fn host_listed_project_of(&self, tab: TabKey) -> Option<ProjectKey> {
        listed_project_of(self.host_view(tab.host)?, tab)
    }

    /// The project a connected host lists a tab under. `None` for a tab
    /// on a host that is not connected, or one its mirror never listed.
    fn host_project_of(&self, tab: TabKey) -> Option<ProjectKey> {
        listed_project_of(self.interactive_host_view(tab.host)?, tab)
    }

    /// One connected host's project row, as its last mirror listed it.
    /// The single spelling of that lookup, so every host-aware surface
    /// applies the same "dimmed sections answer nothing" gate.
    fn host_project_row(&self, project: ProjectKey) -> Option<(&HostView, &Project)> {
        let view = self.interactive_host_view(project.host)?;
        let row = view.projects.iter().find(|row| row.id == project.project)?;
        Some((view, row))
    }

    /// The tab a host project row selects: the host's own active tab when
    /// it lives in that project, else the project's first. Mirrors
    /// `Workspace::preferred_tab`, read off the mirror instead.
    fn host_preferred_tab(&self, project: ProjectKey) -> Option<TabKey> {
        let (view, rows) = self.host_project_row(project)?;
        let tab = rows
            .tabs
            .iter()
            .find(|tab| tab.id == view.active_tab_id)
            .or_else(|| rows.tabs.first())?;
        Some(TabKey::new(project.host, tab.id))
    }

    /// Drop a host selection the sidebar can no longer draw: its host
    /// dropped, its incarnation was replaced, or its tab closed. Falls
    /// back to the local workspace's own selection, which never went
    /// anywhere.
    fn reconcile_host_selection(&mut self) {
        let Some(selection) = self.host_selection else {
            return;
        };
        if self.workspace.active().1 != selection.local_active {
            tracing::debug!("host selection dropped: a local tab took the focus");
            self.set_host_selection(None);
            return;
        }
        if self.host_project_of(selection.tab) == Some(selection.project) {
            return;
        }
        // A takeover or a stop leaves a frame nothing will ever update
        // again — and that frame is the last true thing this window
        // knows about that session, so it stays (plan 037 §3.1's
        // "keeps its last frame dimmed") with the banner over it. The
        // row must still be listed: a tab the mirror dropped before the
        // connection died has nothing left to show.
        if self.frozen_host_frame().is_some()
            && self.host_listed_project_of(selection.tab) == Some(selection.project)
        {
            return;
        }
        tracing::debug!(
            tab = %selection.tab,
            "host selection dropped: the tab is no longer listed"
        );
        self.set_host_selection(None);
    }

    /// The one writer of [`Self::host_selection`].
    ///
    /// Attach-on-focus owes a detach-on-defocus, and the two have to be
    /// spelled in the same place or they drift: a selection leaving a
    /// host tab — for a local tab, for another host's tab, or for
    /// nothing at all — releases that tab's attach here, exactly once,
    /// whichever route moved it. C5's detach keeps the resume point, so
    /// refocusing picks the stream back up where it left off rather than
    /// re-snapshotting.
    ///
    /// Re-selecting the same tab is not a move and does not detach —
    /// which is what makes `focus_host_tab_and_clear`'s refocus path
    /// (set the selection, then attach) idempotent.
    fn set_host_selection(&mut self, next: Option<HostSelection>) {
        let released = host_selection_detach(self.host_selection, next);
        self.host_selection = next;
        if let Some(tab) = released {
            self.host_detach_tab(tab);
        }
        // Every selection move is a focus move as far as a session is
        // concerned: the host that lost the selection hears null and the
        // one that gained it hears the tab, so exactly one session
        // believes it is being looked at.
        self.push_host_focus();
    }

    /// State this client's focus to every connected host — the one
    /// caller of [`HostConnSet::set_focus`], so the value a session
    /// holds is always derived from the same two fields rather than
    /// assembled at each edge.
    ///
    /// Called at the three edges that can move it: a host reaching
    /// `Connected` (a fresh session believes its own headless default
    /// until told), the selection moving, and the window gaining or
    /// losing focus. The set dedups, so calling it on a change that
    /// turns out not to move anything costs nothing.
    fn push_host_focus(&mut self) {
        self.hosts
            .set_focus(host_focus_claim(self.window_focused, self.host_selection));
    }

    /// The gated Connect: the sidebar's ↻ row, the takeover banner's
    /// button, and the `Connect Host` palette verb — which
    /// `palette.activate` also reaches over the IPC socket, hence the
    /// `origin`.
    ///
    /// One gate sits here, and it is plan 037 §3.7's: a host whose
    /// compatibility check already failed does not get dialed again —
    /// dialing would reproduce the same refusal — it raises the upgrade
    /// prompt instead. Everything else connects.
    ///
    /// **Only for a person**, per [`host_notice::connect_route`]: the
    /// prompt's button now starts a remote install, and a machine that
    /// activated the same palette row would otherwise be answered with
    /// a modal (plan 039 §3.5).
    pub fn host_connect_requested(
        &mut self,
        saved_id: &str,
        origin: crate::host_conn::RequestOrigin,
    ) {
        let needs_restart = matches!(
            self.hosts.state(saved_id),
            Some(crate::host_conn::HostConnState::NeedsRestart(_))
        );
        if host_notice::connect_route(origin, needs_restart) == host_notice::ConnectRoute::Dial {
            return self.host_reconnect_requested(
                saved_id,
                origin,
                crate::host_conn::AttemptCause::Explicit,
            );
        }
        let Some(crate::host_conn::HostConnState::NeedsRestart(mismatch)) =
            self.hosts.state(saved_id)
        else {
            unreachable!("connect_route only prompts for a NeedsRestart host")
        };
        let mismatch = mismatch.clone();
        let Some(label) = self.host_label(saved_id) else {
            tracing::debug!(host = %saved_id, "connect requested for a host that is not saved");
            return;
        };
        // A restart already running leaves the host in `NeedsRestart`
        // until its relaunch connects, so this door stays open under it —
        // and re-raising the prompt is how a second ladder gets started.
        // Say what is happening instead.
        if self.host_restarts.contains(saved_id) {
            self.set_status(format!("{label} is already restarting"));
            return;
        }
        self.open_host_restart_dialog(saved_id, host_notice::restart_prompt(&label, &mismatch));
    }

    /// Connect a saved host again, from the sidebar's inline ↻ row.
    ///
    /// An explicit connect is unconditional takeover and may spawn a
    /// localhost session that is not running (§3.2's explicit-connect
    /// rule) — the launch-time probe is the only connect that does
    /// neither. C7's `Connect Host` verb and the Add Host dialog land on
    /// this same entry.
    ///
    /// Deliberately ungated: [`Self::host_connect_requested`] is the
    /// user-facing door with the upgrade check on it, and the restart
    /// flow's own relaunch has to come through *here* — the host is
    /// still in `NeedsRestart` at that moment, and re-raising the dialog
    /// the user just answered would be a loop.
    ///
    /// It is also the door a scheduled reconnect is meant to re-enter
    /// through (plan 040 §3.4), which is why the attempt says why it
    /// exists: the guards this door supplies — the saved-host lookup,
    /// the bootstrap-probe cancel, the target re-classification — are
    /// exactly the ones a raw `open_ssh` re-entry would skip.
    pub fn host_reconnect_requested(
        &mut self,
        saved_id: &str,
        origin: crate::host_conn::RequestOrigin,
        cause: crate::host_conn::AttemptCause,
    ) {
        let Ok(host) = self.saved_host(saved_id) else {
            tracing::debug!(host = %saved_id, "reconnect requested for a host that is not saved");
            return;
        };
        tracing::info!(host = %host.id, "connecting a saved host session");
        self.connect_saved_host(
            &host,
            origin,
            |localhost| {
                Some(if localhost {
                    crate::host_conn::ConnectMode::SpawnIfMissing
                } else {
                    crate::host_conn::ConnectMode::Dial
                })
            },
            cause,
        );
    }

    /// An armed auto-reconnect came due (plan 040 §3.4).
    ///
    /// It re-enters through [`Self::host_reconnect_requested`] rather
    /// than calling `open_ssh`, which is the whole point of the door:
    /// the saved-host lookup catches a host removed between arm and
    /// fire, `connect_saved_host`'s `cancel_bootstrap_probe` closes the
    /// consent path a raw re-entry would leave open, and its
    /// re-classification catches a host edited to a non-ssh target.
    fn host_reconnect_due(&mut self, saved_id: &str, request: u64) {
        // `EngineFeed::Quit` does not stop the drain, so a due message
        // sitting behind one would re-enter the connect path after the
        // user asked to quit.
        if self.exit_state != ExitState::Running {
            tracing::debug!(host = %saved_id, "not reconnecting a host during shutdown");
            return;
        }
        let Some((origin, cause)) = self.hosts.reconnect_due(saved_id, request) else {
            return;
        };
        self.host_reconnect_requested(saved_id, origin, cause);
    }

    // ── host verbs (plan 037 §3.1/§3.5) ─────────────────────────────
    //
    // Every one of these is reachable three ways — a palette row, a
    // `roostctl host` verb, and (for reconnect) the sidebar's inline ↻ —
    // and all three land here rather than each doing its own thing.

    /// The saved hosts as the verb policy reads them.
    fn host_verb_rows(&self) -> Vec<host_verbs::HostRow<'_>> {
        self.host_views
            .iter()
            .map(|view| host_verbs::HostRow {
                saved_id: view.saved_id.as_str(),
                label: view.label.as_str(),
                state: view.state,
                localhost: view.localhost,
            })
            .collect()
    }

    /// Disconnect a host, and drop everything the connection published.
    ///
    /// The incarnation is purged here rather than off a feed item
    /// because a disconnect with no reconnect never publishes the
    /// `Connecting { previous }` that consumers purge off — this is that
    /// message's stand-in (`HostConnSet::disconnect`'s contract).
    pub(crate) fn host_disconnect_requested(&mut self, saved_id: &str) {
        self.cancel_bootstrap_probe(saved_id);
        let Some(incarnation) = self.hosts.disconnect(saved_id) else {
            tracing::debug!(host = %saved_id, "disconnect requested for a host with no connection");
            self.reconcile();
            return;
        };
        self.purge_host_incarnation(incarnation);
        self.reconcile();
    }

    /// Forget a saved host. Disconnects it first: Remove is only offered
    /// while disconnected, but the op set is not the palette and
    /// `roostctl host remove` can arrive at any moment.
    pub(crate) fn host_remove_requested(
        &mut self,
        saved_id: &str,
    ) -> Result<(), roost_engine::WorkspaceError> {
        self.cancel_bootstrap_probe(saved_id);
        // `remove`, not `disconnect`: a disconnect keeps the last mirror
        // so the section can list its shells dimmed, and once the host
        // is forgotten there is no section left to list them in.
        if let Some(incarnation) = self.hosts.remove(saved_id) {
            self.purge_host_incarnation(incarnation);
        }
        let removed = self.workspace.remove_host(saved_id);
        self.reconcile();
        removed
    }

    /// Save a host and connect it — the Add Host dialog's tail, and
    /// `host.add`'s whole body.
    ///
    /// `connect` is what separates the two callers: the dialog's button
    /// says "Add & Connect" and means it, while `roostctl host add` is
    /// documented as registry-only (a `host connect` follows if you want
    /// one). It names *who* is connecting rather than merely whether to,
    /// so a caller cannot start a connection without saying whether
    /// there is anybody there to answer for it.
    pub(crate) fn host_add_requested(
        &mut self,
        label: &str,
        target: &str,
        connect: Option<crate::host_conn::RequestOrigin>,
    ) -> Result<roost_engine::persistence::HostSnapshot, roost_engine::WorkspaceError> {
        let host = self.workspace.add_host(label, target)?;
        if let Some(origin) = connect {
            self.host_reconnect_requested(
                &host.id,
                origin,
                crate::host_conn::AttemptCause::Explicit,
            );
        }
        self.reconcile();
        Ok(host)
    }

    /// Stop the session on a host: every shell over there ends, the
    /// layout survives for the next start (plan 037 §3.1's Stop ≠
    /// Disconnect).
    ///
    /// Fire-and-forget on the host's own queue. The session answers by
    /// pushing its `session.stopping` envelope, which is what moves the
    /// section to "session ended" — waiting on the reply here would
    /// report the same thing twice, and later.
    pub(crate) fn host_stop_requested(&mut self, saved_id: &str) {
        let intent = crate::host_conn::HostIntent::new(
            roost_ipc::messages::ops::SESSION_STOP,
            serde_json::json!({}),
        );
        if self.hosts.send(saved_id, intent).is_err() {
            self.set_status(format!("host {saved_id} is not accepting operations"));
        }
    }

    // ── the Add Host dialog ─────────────────────────────────────────

    /// Open Add Host. The one free-text flow in the family, so it is the
    /// one verb that is a dialog rather than a palette row that acts.
    pub(crate) fn open_add_host_dialog(&mut self) {
        self.open_host_dialog(host_dialog::HostDialog::add());
        self.add_host_focus_requested = true;
    }

    /// Open the Stop confirmation for a connected host.
    fn open_host_stop_dialog(&mut self, saved_id: &str, label: &str) {
        self.open_host_dialog(host_dialog::HostDialog::ConfirmStop {
            saved_id: saved_id.to_string(),
            label: label.to_string(),
        });
    }

    /// Open the upgrade prompt for a host whose compatibility gate
    /// refused (plan 037 §3.7).
    fn open_host_restart_dialog(&mut self, saved_id: &str, prompt: host_notice::RestartPrompt) {
        self.open_host_dialog(host_dialog::HostDialog::ConfirmRestart {
            saved_id: saved_id.to_string(),
            prompt,
        });
    }

    /// Raise a host modal, and put down whatever the pointer and the
    /// keyboard were doing first.
    ///
    /// The one place that list of cancels lives: a modal owns both while
    /// it is up, so anything half-finished underneath it (a drag, an
    /// inline rename, the delete confirmation, an IME composition) has to
    /// be resolved before it opens rather than left to surface when it
    /// closes.
    fn open_host_dialog(&mut self, dialog: host_dialog::HostDialog) {
        self.cancel_drags();
        self.cancel_editor_for_interaction();
        self.cancel_confirm_delete();
        self.host_dialog = Some(dialog);
        self.cancel_ime_composition();
    }

    pub fn host_dialog_cancel(&mut self) {
        self.host_dialog = None;
        self.add_host_focus_requested = false;
    }

    pub fn add_host_name_changed(&mut self, value: String) {
        self.edit_add_host_draft(|draft| draft.name = value);
    }

    pub fn add_host_socket_changed(&mut self, value: String) {
        self.edit_add_host_draft(|draft| draft.socket = value);
    }

    /// Every field edit, so `AddHostDraft::edited`'s "the error and the
    /// dial both described the draft as it was" is applied once rather
    /// than per field.
    fn edit_add_host_draft(&mut self, edit: impl FnOnce(&mut host_dialog::AddHostDraft)) {
        if let Some(draft) = self.host_dialog.as_mut().and_then(|d| d.draft_mut()) {
            edit(draft);
            draft.edited();
        }
    }

    /// The Stop confirmation's destructive button.
    pub fn host_stop_confirmed(&mut self) {
        let Some(host_dialog::HostDialog::ConfirmStop { saved_id, .. }) = self.host_dialog.take()
        else {
            return;
        };
        self.host_stop_requested(&saved_id);
    }

    /// The upgrade prompt's confirm, before either branch acts on it.
    ///
    /// **The state is re-read here, and that is the load-bearing part.**
    /// The dialog is modal to the pointer, not to the world: an IPC
    /// `host connect`, a launch-time retry, or another window's takeover
    /// can move this host off `NeedsRestart` while the prompt is still on
    /// screen. Acting then would reap a session that is healthy and
    /// attached — every shell on it, for a mismatch that no longer
    /// exists. So the question is asked again at the moment the answer is
    /// acted on, and a host that moved on is told so instead — `undone`
    /// naming what the branch that was about to run did not do.
    ///
    /// Answers with the host, its label, and whether the session over
    /// there is the newer of the two, which only the remote branch has a
    /// use for.
    fn take_confirmed_restart_prompt(&mut self, undone: &str) -> Option<(String, String, bool)> {
        let Some(host_dialog::HostDialog::ConfirmRestart { saved_id, .. }) =
            self.host_dialog.take()
        else {
            return None;
        };
        let label = self
            .host_label(&saved_id)
            .unwrap_or_else(|| saved_id.clone());
        let Some(crate::host_conn::HostConnState::NeedsRestart(mismatch)) =
            self.hosts.state(&saved_id)
        else {
            tracing::info!(
                host = %saved_id,
                "restart abandoned: the host left NeedsRestart while the prompt was up"
            );
            self.set_status(format!(
                "{label} is no longer waiting for a restart — nothing was {undone}"
            ));
            return None;
        };
        let session_is_newer = host_notice::session_is_newer(mismatch);
        Some((saved_id, label, session_is_newer))
    }

    /// "Restart session" — the client-side composition, step by step
    /// (plan 037 §3.7).
    ///
    /// The two waiting rungs run on the engine runtime because they are
    /// socket work with minute-scale budgets, and the third
    /// ([`crate::host_conn::restart::RestartStep::Relaunch`]) is an
    /// ordinary Connect run on the UI thread when they answer.
    ///
    /// The socket and the may-I-restart-it answer both come from the
    /// *connection*, not from the dialog's copy or a second resolve of
    /// the registry: this stop has to be aimed at the session the prompt
    /// is about, and "only a localhost session is ours to stop" is a fact
    /// about the endpoint we are dialing. A remote host never reaches
    /// here — its dialog has no button — and the live flag is what says
    /// so rather than a snapshot taken when the dialog opened.
    ///
    /// The state is re-read through
    /// [`Self::take_confirmed_restart_prompt`], which is the
    /// load-bearing part; `undone` is what a host that moved on is told
    /// did not happen.
    pub fn host_restart_confirmed(&mut self) -> UiTask {
        let Some((saved_id, label, _)) = self.take_confirmed_restart_prompt("stopped") else {
            return UiTask::None;
        };
        let Some((socket, localhost)) = self.hosts.endpoint(&saved_id) else {
            tracing::debug!(host = %saved_id, "restart requested for a host with no connection");
            return UiTask::None;
        };
        if !localhost {
            return UiTask::None;
        }
        let socket = socket.to_path_buf();
        // One ladder per host: the prompt is re-raisable while this runs
        // (the host stays `NeedsRestart` until the relaunch connects), and
        // two stop+spawn ladders racing for one socket is the failure that
        // makes.
        if !self.host_restarts.begin(&saved_id) {
            tracing::debug!(host = %saved_id, "a restart is already running for this host");
            self.set_status(format!("{label} is already restarting"));
            return UiTask::None;
        }
        tracing::info!(host = %saved_id, socket = %socket.display(), "restarting a host session");
        self.set_status(format!("restarting the session on {label}…"));
        self.engine_op(
            crate::host_conn::restart::stop_and_wait_owned(socket),
            move |result| EngineOpResult::HostRestarted { saved_id, result },
        )
    }

    /// The stop half of a restart answered: relaunch, or say where it
    /// stopped.
    ///
    /// The claim is released on both outcomes and before the relaunch:
    /// what it guards is the stop+spawn ladder, and the relaunch is an
    /// ordinary Connect from here on, with the connection state machine's
    /// own replace-in-flight rules.
    fn host_restart_completed(&mut self, saved_id: &str, result: Result<(), String>) {
        self.host_restarts.finish(saved_id);
        match result {
            // The session is gone; connecting again spawns a fresh one
            // through the shared ladder and hydrates the saved layout.
            Ok(()) => self.host_reconnect_requested(
                saved_id,
                crate::host_conn::RequestOrigin::User,
                crate::host_conn::AttemptCause::Explicit,
            ),
            Err(error) => self.set_status(error),
        }
    }

    // ── the test-mode dialog seam (plan 039 §3.5) ───────────────────
    //
    // `tools/roosttest/` drives a real UI over the IPC socket and
    // nothing else, so a consent dialog is otherwise unreachable from
    // it — and the bootstrap job, unlike the upgrade prompt, has no
    // "compose the ops the button runs" back door on purpose. These two
    // are the seam, gated on `ROOST_TEST_MODE=1` at the servicing edge
    // and never a production surface.

    /// What the host modal on screen is saying.
    ///
    /// Reads the same values the widgets do, one per arm, so the dump
    /// and the card cannot disagree about the copy — the four titles and
    /// bodies come from the very fields `host_dialog_modal` renders,
    /// and the shared literals are named constants for exactly that
    /// reason.
    pub(crate) fn dialog_dump(&self) -> roost_ipc::messages::AppDialogDumpResult {
        use roost_ipc::messages::AppDialogDumpResult;

        let Some(dialog) = self.host_dialog.as_ref() else {
            return AppDialogDumpResult::default();
        };
        let (kind, variant, title, body, buttons, host) = match dialog {
            host_dialog::HostDialog::Add(draft) => (
                "add",
                None,
                ADD_HOST_TITLE.to_string(),
                ADD_HOST_BODY.to_string(),
                vec!["Cancel".to_string(), draft.confirm_label().to_string()],
                None,
            ),
            host_dialog::HostDialog::ConfirmStop { saved_id, label } => (
                "confirm_stop",
                None,
                stop_session_title(label),
                STOP_SESSION_BODY.to_string(),
                vec!["Cancel".to_string(), STOP_SESSION_CONFIRM.to_string()],
                Some(saved_id.clone()),
            ),
            host_dialog::HostDialog::ConfirmRestart { saved_id, prompt } => (
                "confirm_restart",
                None,
                prompt.title.clone(),
                prompt.body.clone(),
                std::iter::once(prompt.dismiss_label().to_string())
                    .chain(prompt.confirm.clone())
                    .collect(),
                Some(saved_id.clone()),
            ),
            host_dialog::HostDialog::Bootstrap(draft) => (
                "bootstrap",
                Some(draft.plan.variant.wire_name()),
                draft.copy.title.clone(),
                draft.copy.body.clone(),
                vec!["Cancel".to_string(), draft.copy.confirm.to_string()],
                Some(draft.saved_id.clone()),
            ),
        };
        AppDialogDumpResult {
            dialog: Some(kind.to_string()),
            variant: variant.map(str::to_string),
            title,
            body,
            buttons,
            host,
        }
    }

    /// Press the visible host modal's primary button, or dismiss it.
    ///
    /// Through the production handlers, not around them: `confirm` is
    /// the same call the button's `Message` makes, so every guard the
    /// button has — the re-read of state, the claim, the refusal to run
    /// twice — is a guard this op also passes through.
    ///
    /// A dialog with no primary action refuses `confirm` rather than
    /// silently dismissing: a test that thinks it pressed a button that
    /// is not there should fail loudly.
    pub(crate) fn dialog_answer(&mut self, action: &str) -> Result<UiTask, String> {
        if self.host_dialog.is_none() {
            return Err("no host dialog is open".to_string());
        }
        if action == "cancel" {
            self.host_dialog_cancel();
            self.reconcile();
            return Ok(UiTask::None);
        }
        let task = match &self.host_dialog {
            Some(host_dialog::HostDialog::Add(draft)) => {
                if draft.is_verifying() {
                    return Err("the Add Host dialog is already dialing".to_string());
                }
                self.submit_add_host()
            }
            Some(host_dialog::HostDialog::ConfirmStop { .. }) => {
                self.host_stop_confirmed();
                UiTask::None
            }
            Some(host_dialog::HostDialog::ConfirmRestart { prompt, .. }) => {
                if prompt.confirm.is_none() {
                    return Err("this dialog has no confirming action".to_string());
                }
                self.host_restart_dialog_confirmed()
            }
            Some(host_dialog::HostDialog::Bootstrap(_)) => {
                self.host_bootstrap_confirmed();
                UiTask::None
            }
            None => unreachable!("checked above"),
        };
        self.reconcile();
        Ok(task)
    }

    // ── the bootstrap offer (plan 039 §3.5) ─────────────────────────

    /// Route the upgrade prompt's primary button to whichever flow it
    /// promised.
    ///
    /// One place rather than two arms at every call site: the button's
    /// label and the flow it starts are decided together in
    /// [`host_notice::restart_prompt`], and this is the only thing that
    /// reads that decision back out.
    fn host_restart_dialog_confirmed(&mut self) -> UiTask {
        let remote = matches!(
            &self.host_dialog,
            Some(host_dialog::HostDialog::ConfirmRestart { prompt, .. })
                if prompt.action == crate::host_conn::state::RestartAction::OfferRemoteUpdate
        );
        if remote {
            self.host_remote_update_requested();
            UiTask::None
        } else {
            self.host_restart_confirmed()
        }
    }

    /// "Update roost-session on <label>" — the remote branch of the
    /// upgrade prompt (plan 039 §3.5, entry point 2).
    ///
    /// It does **not** start an install: it replaces one dialog with the
    /// probe that decides which of the three consent cards is the honest
    /// one. Offer first, resolve at confirm — the far side is read, never
    /// written, before the second dialog is answered.
    ///
    /// The state is re-read through
    /// [`Self::take_confirmed_restart_prompt`]: the prompt is modal to
    /// the pointer, not to the world, and a host that reconnected
    /// underneath it has nothing left to update.
    fn host_remote_update_requested(&mut self) {
        let Some((saved_id, _, session_is_newer)) = self.take_confirmed_restart_prompt("changed")
        else {
            return;
        };
        self.start_bootstrap_probe(
            &saved_id,
            bootstrap::OfferContext {
                session: bootstrap::SessionState::Running,
                session_is_newer,
                // A running session, not a failed connect — there is no
                // family to still agree with when this is confirmed.
                failure: None,
            },
        );
    }

    /// A user-driven connect failed; decide whether Roost has an offer.
    ///
    /// Two families have one — `NotFound` ("nothing to exec over there")
    /// and `NoSession` ("a binary, but nothing running") — and the probe
    /// is what turns either into a specific card. Everything else is
    /// left to the band and the toast exactly as plan 038 left it.
    ///
    /// **The origin is the gate, not attendedness.** An IPC
    /// `host.connect` from `roostctl` arrives as the same
    /// `ConnectMode::Dial` a click does, and raising a modal to ask a
    /// machine a question is the one thing this must never do (plan 039
    /// §3.5's non-interactive refusal). `RequestOrigin` is the only
    /// place that difference survives.
    fn maybe_offer_bootstrap(&mut self, saved_id: &str) {
        let failure = self.hosts.ssh_failure(saved_id).cloned();
        let Some(session) = bootstrap::offer_for(
            self.hosts.ssh_origin(saved_id),
            failure.as_ref(),
            self.hosts.ssh_reached_connected(saved_id),
        ) else {
            return;
        };
        self.start_bootstrap_probe(
            saved_id,
            bootstrap::OfferContext {
                session,
                session_is_newer: false,
                failure,
            },
        );
    }

    /// Look at the far side, then raise the card that fits what is
    /// there.
    ///
    /// Read-only from end to end, which is what makes it safe to run
    /// before anybody has agreed to anything: nothing is written,
    /// started or stopped until the dialog this opens is confirmed.
    fn start_bootstrap_probe(&mut self, saved_id: &str, offer: bootstrap::OfferContext) {
        // A modal already up owns the pointer and the keyboard, and the
        // one that would open here is a question about a host the user
        // is not currently being asked about.
        if self.host_dialog.is_some() {
            tracing::debug!(host = %saved_id, "not offering a bootstrap over an open dialog");
            return;
        }
        let Ok(host) = self.saved_host(saved_id) else {
            return;
        };
        let target = match roost_ipc::ssh::classify(&host.target) {
            Ok(roost_ipc::ssh::ResolvedTransport::Ssh(target)) => target,
            // Only an ssh host has a transport this can reach a binary
            // over; the other two are somebody else's process.
            _ => return,
        };
        // The debounce, and the anti-race. A second click while the
        // first probe is out would open two cards for one host; a probe
        // for a box a job is already setting up would offer to do it
        // again.
        if self.bootstraps.probing(saved_id) {
            tracing::debug!(host = %saved_id, "a bootstrap probe is already out for this host");
            return;
        }
        if self.bootstraps.job_running(&target.claim_key) {
            self.set_status(format!("{} is already being set up", host.label));
            return;
        }
        let generation = self.take_engine_op_id();
        self.bootstraps.begin_probe(saved_id, generation);
        let checking = format!("checking {}…", host.label);
        self.hosts
            .set_bootstrap_note(saved_id, Some(checking.clone()));
        // The band carries this only where its own rule allows a reason
        // — beside the `disconnected` word (`status_text_with_reason`),
        // which is the NotFound/NoSession entry. The remote-update entry
        // leaves the host in `needs restart`, so the toast is what says
        // something is happening there.
        self.set_status(checking);
        self.reconcile();

        let feed = self.feed_tx.clone();
        let ssh = roost_ipc::ssh::SshTunnelOptions::from_env();
        let request = bootstrap::BootstrapRequest {
            saved_id: saved_id.to_string(),
            generation,
            label: host.label.clone(),
            target: host.target.clone(),
            token: target.token.clone(),
            claim: target.claim_key.clone(),
        };
        // Spawned rather than dispatched as an engine op: this is
        // reached from the feed drain and from a modal button, neither
        // of which can hand Iced a task, and the answer has to be
        // ordered against the connects around it anyway.
        self.runtime_handle.spawn(async move {
            // Built here rather than at the call site: `from_env` walks
            // `$PATH` for a sibling binary, and that is a blocking stat
            // per entry on the thread that draws frames.
            let options =
                roost_ipc::bootstrap::BootstrapOptions::from_env(bootstrap::client_identity());
            let result = bootstrap::run_probe(target, ssh, options).await;
            feed.send(crate::engine_feed::EngineFeed::HostBootstrap(Box::new(
                bootstrap::BootstrapEvent::Probed {
                    request,
                    offer,
                    result,
                },
            )));
        });
    }

    /// Drop an in-flight probe, and the band line it left.
    ///
    /// Called wherever a connect or a disconnect begins: both replace
    /// the state the probe's question was asked about, so its answer can
    /// only describe something that has already stopped being true.
    fn cancel_bootstrap_probe(&mut self, saved_id: &str) {
        if self.bootstraps.cancel_probe(saved_id) {
            tracing::debug!(host = %saved_id, "a new attempt superseded a bootstrap probe");
            self.hosts.set_bootstrap_note(saved_id, None);
        }
    }

    /// A bootstrap step reported back.
    pub(crate) fn host_bootstrap_event(&mut self, event: bootstrap::BootstrapEvent) {
        match event {
            bootstrap::BootstrapEvent::Probed {
                request,
                offer,
                result,
            } => self.host_bootstrap_probed(request, offer, result),
            bootstrap::BootstrapEvent::Finished { request, result } => {
                self.host_bootstrap_finished(request, result)
            }
        }
    }

    /// The probe answered: raise the card, or say why there is nothing
    /// to offer.
    ///
    /// The round trip is seconds long, and [`bootstrap::Landed`] is
    /// every way the app can have moved inside it — the request
    /// superseded, the host removed or re-targeted, or a modal opened.
    /// That last one is the sharp edge: `open_host_dialog` *replaces*
    /// whatever is up, and Enter routes to the visible dialog's confirm
    /// — so a card that displaced a Stop confirmation would take the
    /// Enter aimed at it, which is precisely the consent this whole flow
    /// exists to protect. `start_bootstrap_probe` refuses to start under
    /// an open dialog; this is the same refusal at the other end.
    ///
    /// The label and the target are re-read rather than taken from the
    /// request, so a renamed or re-pointed host cannot be described by
    /// the words it had when the probe went out.
    fn host_bootstrap_probed(
        &mut self,
        request: bootstrap::BootstrapRequest,
        offer: bootstrap::OfferContext,
        result: Result<bootstrap::Probed, roost_ipc::bootstrap::BootstrapError>,
    ) {
        let bootstrap::BootstrapRequest {
            saved_id,
            generation,
            label: asked_label,
            target,
            token,
            ..
        } = request;
        let saved_id = saved_id.as_str();
        let claimed = self.bootstraps.claim_probe(saved_id, generation);
        let live = self.saved_host(saved_id).ok().and_then(|host| {
            match roost_ipc::ssh::classify(&host.target) {
                Ok(roost_ipc::ssh::ResolvedTransport::Ssh(live)) if live.token == token => {
                    Some((host.label, live))
                }
                _ => None,
            }
        });
        let landed = bootstrap::Landed {
            claimed,
            same_host: live.is_some(),
            dialog_open: self.host_dialog.is_some(),
        };
        match landed.landing() {
            bootstrap::ProbeLanding::Offer => {}
            bootstrap::ProbeLanding::Stale => {
                tracing::debug!(host = %saved_id, generation, "dropped a stale bootstrap probe");
                return;
            }
            bootstrap::ProbeLanding::Moved => {
                tracing::debug!(
                    host = %saved_id,
                    "dropped a bootstrap probe for a host that was removed or re-targeted"
                );
                self.hosts.set_bootstrap_note(saved_id, None);
                self.reconcile();
                return;
            }
            bootstrap::ProbeLanding::Deferred => {
                let label = live.map_or(asked_label, |(label, _)| label);
                tracing::info!(
                    host = %saved_id,
                    "a bootstrap offer landed under an open dialog; leaving it on the band"
                );
                // Said as the entry point knew it, because the plan the
                // card would have carried is not composed on this path.
                let note = match offer.session {
                    bootstrap::SessionState::Running => {
                        format!("roost-session on {label} can be updated — connect again")
                    }
                    bootstrap::SessionState::NoSession => {
                        format!("{label} needs roost-session set up — connect again")
                    }
                };
                self.hosts.set_bootstrap_note(saved_id, Some(note.clone()));
                self.set_status(note);
                self.reconcile();
                return;
            }
        }
        let (label, live_target) = live.expect("Offer implies the host is still the same one");
        self.hosts.set_bootstrap_note(saved_id, None);
        let probed = match result {
            Ok(probed) => probed,
            Err(error) => return self.report_bootstrap_failure(saved_id, &target, &error),
        };
        let plan = bootstrap::plan_bootstrap(&probed.probe.outcome, offer.session);
        // Predicted, not resolved: choosing a rung for real means a
        // subprocess and possibly a download, and that is the job's
        // first phase rather than the offer's (plan 039 §3.3). The
        // preview names a fall-through where one is possible rather
        // than promising a rung it cannot vouch for.
        let source = if plan.install {
            match probed.source {
                Ok(source) => source,
                // Nothing can supply this build — the honest answer
                // before consent rather than a card whose confirm
                // would fail.
                Err(error) => return self.report_bootstrap_failure(saved_id, &target, &error),
            }
        } else {
            String::new()
        };
        let copy = bootstrap::bootstrap_copy(bootstrap::CopyInputs {
            label: &label,
            identity: &bootstrap::client_identity(),
            dest: &bootstrap::card_dest(&plan),
            dest_on_disk: &bootstrap::dest_on_disk(&plan, &probed.probe.home),
            source: &source,
            plan: &plan,
            session_is_newer: offer.session_is_newer,
        });
        self.open_host_dialog(host_dialog::HostDialog::Bootstrap(
            bootstrap::BootstrapDraft {
                saved_id: saved_id.to_string(),
                label,
                token,
                claim: live_target.claim_key,
                arch: probed.probe.arch,
                plan,
                copy,
                offer,
            },
        ));
        self.reconcile();
    }

    /// The far side as it is *now*, for [`bootstrap::offer_still_stands`].
    ///
    /// Two facts, both read live: what the connection set says about the
    /// host, and — for a cold offer that a connect attempt produced —
    /// whether that attempt still says the same thing. A host with no
    /// attempt on record answers `None` rather than `false`: the Add
    /// Host entry verifies and never dials, so "nothing dialed" is its
    /// normal shape and not evidence that anything moved.
    fn live_bootstrap_state(&self, saved_id: &str) -> bootstrap::LiveState {
        match self.hosts.state(saved_id) {
            Some(crate::host_conn::HostConnState::NeedsRestart(_)) => {
                bootstrap::LiveState::NeedsRestart
            }
            None | Some(crate::host_conn::HostConnState::Disconnected(_)) => {
                bootstrap::LiveState::Cold {
                    qualifies: self.hosts.ssh_origin(saved_id).map(|origin| {
                        bootstrap::offer_for(
                            Some(origin),
                            self.hosts.ssh_failure(saved_id),
                            self.hosts.ssh_reached_connected(saved_id),
                        ) == Some(bootstrap::SessionState::NoSession)
                    }),
                }
            }
            _ => bootstrap::LiveState::Other,
        }
    }

    /// The consent card's primary button.
    ///
    /// Four guards, and the first three are [`Self::host_restart_confirmed`]'s
    /// three questions: is the host still there, is it still the machine
    /// the card described, and is anybody else already doing this. The
    /// third is keyed on [`roost_ipc::ssh::SshTarget::claim_key`] rather
    /// than the saved id — two labels, or two spellings, naming one box
    /// must not race two installs onto one `~/.local/bin/roost-session`.
    ///
    /// The fourth is the card's own: **the plan is only honest while the
    /// far side is still what it was planned against**. The card is a
    /// deliberate snapshot — it must not rewrite itself mid-read — and
    /// both directions of the window that opens are damaging, a stale
    /// `Update` reaping a session that came back healthy and a stale
    /// cold plan installing under one that started. See
    /// [`bootstrap::offer_still_stands`].
    pub fn host_bootstrap_confirmed(&mut self) {
        let Some(host_dialog::HostDialog::Bootstrap(draft)) = self.host_dialog.take() else {
            return;
        };
        // Re-read the registry rather than trusting the snapshot: the
        // host may have been removed or re-targeted while the card was
        // up, and the second of those would aim this job at a machine
        // the user is no longer describing.
        let Ok(host) = self.saved_host(&draft.saved_id) else {
            self.set_status(format!("{} is no longer saved", draft.label));
            return;
        };
        let target = match roost_ipc::ssh::classify(&host.target) {
            Ok(roost_ipc::ssh::ResolvedTransport::Ssh(target)) if target.token == draft.token => {
                target
            }
            _ => {
                self.set_status(format!(
                    "{} no longer names the host this offer was about — nothing was changed",
                    host.label
                ));
                return;
            }
        };
        let live = self.live_bootstrap_state(&draft.saved_id);
        if !bootstrap::offer_still_stands(draft.offer.session, live) {
            tracing::info!(
                host = %draft.saved_id,
                ?live,
                offered = ?draft.offer.session,
                "bootstrap abandoned: the far side moved while the card was up"
            );
            self.set_status(format!(
                "{} is no longer what that offer described — nothing was changed",
                host.label
            ));
            self.reconcile();
            return;
        }
        let generation = self.take_engine_op_id();
        if !self.bootstraps.begin_job(&draft.claim, generation) {
            tracing::debug!(claim = %draft.claim, "a bootstrap is already running for this target");
            self.set_status(format!("{} is already being set up", host.label));
            return;
        }
        tracing::info!(
            host = %draft.saved_id,
            variant = ?draft.plan.variant,
            install = draft.plan.install,
            stop = draft.plan.stop,
            "setting up roost-session on a host"
        );
        self.hosts.set_bootstrap_note(
            &draft.saved_id,
            Some("setting up roost-session…".to_string()),
        );
        self.set_status(format!("setting up roost-session on {}…", host.label));
        self.reconcile();

        let feed = self.feed_tx.clone();
        let ssh = roost_ipc::ssh::SshTunnelOptions::from_env();
        let bootstrap::BootstrapDraft {
            saved_id,
            label,
            token,
            claim,
            arch,
            plan,
            ..
        } = draft;
        let request = bootstrap::BootstrapRequest {
            saved_id,
            generation,
            label,
            target: host.target.clone(),
            token,
            claim,
        };
        self.runtime_handle.spawn(async move {
            // Off the UI thread for `start_bootstrap_probe`'s reason:
            // `from_env` walks `$PATH` looking for a sibling binary.
            let options =
                roost_ipc::bootstrap::BootstrapOptions::from_env(bootstrap::client_identity());
            let result = bootstrap::run_bootstrap(target, ssh, options, plan, arch).await;
            feed.send(crate::engine_feed::EngineFeed::HostBootstrap(Box::new(
                bootstrap::BootstrapEvent::Finished { request, result },
            )));
        });
    }

    /// The job finished. Reconnect on success; say where it stopped
    /// otherwise.
    ///
    /// The claim is released first and on **both** outcomes: a job that
    /// failed at its first rung must not wedge the box out of ever being
    /// set up again.
    fn host_bootstrap_finished(
        &mut self,
        request: bootstrap::BootstrapRequest,
        result: Result<bootstrap::BootstrapSuccess, roost_ipc::bootstrap::BootstrapError>,
    ) {
        let bootstrap::BootstrapRequest {
            saved_id,
            generation,
            label,
            target,
            claim,
            ..
        } = request;
        let saved_id = saved_id.as_str();
        if !self.bootstraps.claim_job(&claim, generation) {
            tracing::debug!(%claim, generation, "dropped a superseded bootstrap completion");
            return;
        }
        match result {
            Ok(success) => {
                self.hosts.set_bootstrap_note(saved_id, None);
                let mut message = format!("roost-session is running on {label}");
                // Not a failure — Roost execs the absolute path — so it
                // rides along on the success line rather than becoming
                // one of its own. Nothing edits a dotfile over it.
                if let Some(warning) = success.path_warning {
                    message.push_str(" — ");
                    message.push_str(&warning);
                }
                // `%message` mirrors `report_bootstrap_failure`'s own
                // `tracing::warn!(%message, ...)`: the toast is
                // ephemeral and carries no op, so the log is the only
                // place a test (or a developer) can read the PATH
                // warning this success line may be carrying.
                tracing::info!(host = %saved_id, %message, "roost-session is set up; reconnecting");
                self.set_status(message);
                self.host_reconnect_requested(
                    saved_id,
                    crate::host_conn::RequestOrigin::User,
                    crate::host_conn::AttemptCause::Explicit,
                );
            }
            Err(error) => self.report_bootstrap_failure(saved_id, &target, &error),
        }
        self.reconcile();
    }

    /// One classified bootstrap failure, through plan 038's own
    /// plumbing: the band gets it because it is the most recent true
    /// thing about the host, and the toast gets it because a person
    /// asked for this and is waiting.
    fn report_bootstrap_failure(
        &mut self,
        saved_id: &str,
        target: &str,
        error: &roost_ipc::bootstrap::BootstrapError,
    ) {
        let message = error.message(target);
        tracing::warn!(host = %saved_id, %message, "bootstrap failed");
        // Only for a host the registry still has. A `host.remove` while
        // the job was out leaves no band to render this on and nothing
        // that would ever clear the entry — `HostConnSet::remove` has
        // already swept it once.
        if self.saved_host(saved_id).is_ok() {
            self.hosts
                .set_bootstrap_note(saved_id, Some(message.clone()));
        }
        self.set_status(message);
    }

    /// "Add & Connect": validate what can be answered now, then dial.
    ///
    /// The dial runs on the engine runtime and reports back as an
    /// ordinary engine op — never on the UI thread, which is the whole
    /// reason this is two steps instead of one (CLAUDE.md's threading
    /// rule; a wedged socket would otherwise freeze the window for the
    /// IPC budget).
    pub fn submit_add_host(&mut self) -> UiTask {
        let Some(host_dialog::HostDialog::Add(draft)) = self.host_dialog.as_ref() else {
            return UiTask::None;
        };
        if draft.is_verifying() {
            // A second Enter while the first is in flight is refused
            // rather than queued, same as the rename editor's.
            return UiTask::None;
        }
        let checked = host_dialog::validate_draft(draft, |label| {
            self.workspace
                .check_host_label(label)
                .map_err(|error| error.to_string())
        });
        let target = match checked {
            Ok(target) => target,
            Err(error) => {
                if let Some(draft) = self.host_dialog.as_mut().and_then(|d| d.draft_mut()) {
                    draft.error = Some(error);
                }
                return UiTask::None;
            }
        };
        // Minted before the dial and echoed back with its answer: this
        // submit, not "a submit that happened to describe the same
        // fields".
        let generation = self.take_engine_op_id();
        if let Some(draft) = self.host_dialog.as_mut().and_then(|d| d.draft_mut()) {
            draft.begin_verify(generation);
        }
        let (label, dial) = (target.label.clone(), target.target.clone());
        self.engine_op(
            async move { host_dialog::verify_target(dial).await },
            move |result| EngineOpResult::HostVerified {
                generation,
                label,
                target: target.target,
                result,
            },
        )
    }

    /// The dial answered. Save + connect on success; show why in the
    /// dialog on failure.
    ///
    /// A reply the dialog is no longer waiting on is dropped: the user
    /// cancelled, opened the other dialog, or edited past this submit,
    /// and saving the host they *were* describing would be a surprise.
    /// The generation is what decides that — a draft whose fields happen
    /// to match is not the same question.
    fn add_host_verified(
        &mut self,
        generation: u64,
        label: &str,
        target: &str,
        result: Result<(), crate::host_conn::ConnectFailure>,
    ) {
        let Some(draft) = self.host_dialog.as_mut().and_then(|d| d.draft_mut()) else {
            tracing::debug!(%label, "dropped an Add Host verify for a dialog that is closed");
            return;
        };
        if !draft.claim_verify(generation) {
            tracing::debug!(%label, generation, "dropped a stale Add Host verify");
            return;
        }
        if let Err(failure) = result {
            // Two families are an offer rather than a refusal (plan 039
            // §3.5): the target classified, ssh reached it, and the only
            // thing missing is a binary or a running session — both of
            // which Roost can put there. The rule is
            // `bootstrap::offer_for`'s, the same one the connect path
            // applies; a dialog the user is typing into is `User` by
            // construction.
            //
            // **The host is saved first**, and that is the point of
            // doing it here rather than after the offer is answered:
            // the band, the job's claim and a retry all need a
            // `saved_id`, and cancelling then leaves a saved,
            // disconnected host the user can remove — rather than a
            // dialog whose Cancel silently discards everything they
            // typed. Registry-only (`connect: None`), because what
            // follows is the offer rather than a dial that would only
            // fail the same way again.
            let session = bootstrap::offer_for(
                Some(crate::host_conn::RequestOrigin::User),
                failure.family.as_ref(),
                // The dialog verified; it never held a connection to
                // lose.
                false,
            );
            let Some(session) = session else {
                draft.error = Some(failure.message);
                return;
            };
            let family = failure.family.clone();
            match self.host_add_requested(label, target, None) {
                Ok(host) => {
                    tracing::info!(host = %host.id, "saved a host whose roost-session needs setting up");
                    self.host_dialog = None;
                    self.add_host_focus_requested = false;
                    self.start_bootstrap_probe(
                        &host.id,
                        bootstrap::OfferContext {
                            session,
                            session_is_newer: false,
                            failure: family,
                        },
                    );
                }
                Err(error) => {
                    if let Some(draft) = self.host_dialog.as_mut().and_then(|d| d.draft_mut()) {
                        draft.error = Some(error.to_string());
                    }
                }
            }
            return;
        }
        match self.host_add_requested(label, target, Some(crate::host_conn::RequestOrigin::User)) {
            Ok(host) => {
                tracing::info!(host = %host.id, "added a host from the Add Host dialog");
                self.host_dialog = None;
                self.add_host_focus_requested = false;
            }
            Err(error) => {
                // The registry re-validates under its own lock, so a
                // label that raced another add lands here rather than at
                // the pre-check above.
                if let Some(draft) = self.host_dialog.as_mut().and_then(|d| d.draft_mut()) {
                    draft.error = Some(error.to_string());
                }
            }
        }
    }

    /// Hand out the Add Host dialog's pending focus, once.
    pub(crate) fn take_add_host_focus_task(&mut self) -> UiTask {
        let open = matches!(self.host_dialog, Some(host_dialog::HostDialog::Add(_)));
        if std::mem::take(&mut self.add_host_focus_requested) && open {
            UiTask::FocusWidget(self.add_host_name_id.clone())
        } else {
            UiTask::None
        }
    }

    // ── event-confirmed creation (plan 037 §3.9) ────────────────────

    /// A creation on a host: remember to select the new tab once the
    /// mirror lists it.
    ///
    /// The local path needs none of this — `Workspace::open_tab` steals
    /// the selection in the same commit that creates the tab — but a
    /// host's rows only exist once its event batch lands, which is a
    /// different message from the op's own reply and may arrive after.
    fn arm_pending_host_selection(&mut self, result: &EngineOpResult) {
        let tab = match result {
            EngineOpResult::TabOpened {
                result: Ok(tab), ..
            } => *tab,
            EngineOpResult::ProjectCreated {
                result: Ok((_, tab)),
                ..
            } => *tab,
            _ => return,
        };
        if !tab.is_local() {
            self.pending_host_selection = Some(PendingHostSelection {
                tab,
                armed: Instant::now(),
            });
        }
    }

    /// Resolve a pending host creation, if the mirror has caught up.
    ///
    /// Four outcomes, all terminal-or-wait: the row is listed (select
    /// it), its host is no longer connected (drop it — a selection
    /// waiting on a session nothing is attached to would never resolve),
    /// the wait ran out (drop it — see [`PendingHostSelection::armed`]),
    /// or none of those yet (wait for the next reconcile).
    ///
    /// Deliberately not `focus_host_tab_and_clear`: this runs *inside*
    /// reconcile, and that helper ends with a reconcile of its own.
    fn resolve_pending_host_selection(&mut self) {
        let Some(pending) = self.pending_host_selection else {
            return;
        };
        let tab = pending.tab;
        // Read off the views this reconcile has already refreshed, so
        // "still connected" is the same fact the section is drawing.
        // Anything else — dropped, reconnecting, taken over, gone — can
        // no longer answer for this row.
        let connected = self.host_views.iter().any(|view| {
            view.host == tab.host && view.state == host_sidebar::SectionState::Connected
        });
        if !connected {
            tracing::debug!(%tab, "dropped a pending selection whose host is no longer connected");
            self.pending_host_selection = None;
            return;
        }
        if pending.expired(Instant::now()) {
            tracing::debug!(%tab, "abandoned a pending selection the mirror never listed");
            self.pending_host_selection = None;
            return;
        }
        let Some(project) = self.host_project_of(tab) else {
            return;
        };
        self.pending_host_selection = None;
        self.set_host_selection(Some(HostSelection {
            project,
            tab,
            local_active: self.workspace.active().1,
        }));
        self.set_sidebar_collapsed(false);
        self.host_focus_tab(tab);
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
        // The override-aware selection, not `workspace.active()`: while a
        // host row is showing, the local workspace's own selection is
        // still whatever it was, and titling the window with it would
        // name a project the user cannot see (plan 037 §3.1).
        let tab = self.active_tab_key();
        let project = self.active_project_key();
        // A host's rows come from its mirror, the local workspace's from
        // this backend's snapshot. The `is_local` guard is load-bearing:
        // a saved host that has never connected carries `HostId::LOCAL`
        // as its placeholder, and matching on the id alone would read a
        // local project out of that host's (empty) rows.
        let host_view = if project.is_local() {
            None
        } else {
            self.host_view(project.host)
        };
        let (rows, host) = match host_view {
            Some(view) => (view.projects.as_slice(), Some(view.label.as_str())),
            None => (self.projects.as_slice(), None),
        };
        let named = rows
            .iter()
            .find(|row| row.id == project.project)
            .map(|row| {
                let cwd = row
                    .tabs
                    .iter()
                    .find(|row| row.id == tab.tab)
                    .map(|row| row.cwd.as_str())
                    .filter(|cwd| !cwd.is_empty())
                    .unwrap_or(row.cwd.as_str());
                (row.name.as_str(), cwd)
            });
        compose_window_title(self.title_fallback, named, host, home)
    }

    /// The cwd a new local tab launches in.
    ///
    /// Deliberately local-only, and a no-op for a host selection: every
    /// caller is a *local* open (`new_tab_dispatch` routes a host project
    /// to the op queue before it gets here, and a custom-command launcher
    /// row opens on the local workspace by construction). A host tab's
    /// cwd lives in that session's mirror and is the server's to answer
    /// when the op queue opens a tab there.
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
            Self::HostReconnect(saved_id) => {
                // A widget press, so a person by construction.
                app.host_connect_requested(&saved_id, crate::host_conn::RequestOrigin::User)
            }
            Self::HostFrameReconnect { saved_id, frame } => {
                app.host_frame_reconnect_requested(&saved_id, frame)
            }
            Self::AddHostNameChanged(value) => app.add_host_name_changed(value),
            Self::AddHostSocketChanged(value) => app.add_host_socket_changed(value),
            Self::AddHostSubmit => return app.submit_add_host(),
            Self::HostDialogCancel => app.host_dialog_cancel(),
            Self::HostDialogCardPressed => {}
            Self::HostStopConfirm => app.host_stop_confirmed(),
            Self::HostRestartConfirm => return app.host_restart_dialog_confirmed(),
            Self::HostBootstrapConfirm => app.host_bootstrap_confirmed(),
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
    fn the_first_quit_signal_requests_and_every_later_one_escalates() {
        let handled = AtomicBool::new(false);
        assert_eq!(observe_quit_signal(&handled), QuitSignalAction::RequestQuit);
        // A repeat of the same signal escalates...
        assert_eq!(observe_quit_signal(&handled), QuitSignalAction::Escalate);
        // ...and so does a *different* one arriving after the first: the
        // latch is "already handled one", not "already saw this kind".
        assert_eq!(observe_quit_signal(&handled), QuitSignalAction::Escalate);
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
            compose_window_title(fallback, None, None, "/Users/me"),
            "Roost-Iced"
        );
        assert_eq!(
            compose_window_title(fallback, Some(("", "/tmp")), None, "/Users/me"),
            "Roost-Iced – /tmp"
        );
        assert_eq!(
            compose_window_title(fallback, Some(("strix", "/Users/me/w")), None, "/Users/me"),
            "strix – ~/w"
        );
    }

    /// A host row names where it runs (plan 037 §3.1). Two windows on
    /// two machines can hold projects with the same name, and the
    /// titlebar is the only place that ever says which is which.
    #[test]
    fn a_host_selection_says_which_host_in_the_title() {
        let fallback = title_fallback(BundleProfileKind::Iced);
        assert_eq!(
            compose_window_title(
                fallback,
                Some(("strix", "/Users/me/w")),
                Some("pop-os"),
                "/Users/me"
            ),
            "strix (pop-os) – ~/w"
        );
        // An unnamed project on a host still says which host — the
        // fallback is applied to the name, not to the whole title.
        assert_eq!(
            compose_window_title(fallback, Some(("", "/tmp")), Some("pop-os"), "/Users/me"),
            "Roost-Iced (pop-os) – /tmp"
        );
        // And with no project there is nothing to qualify.
        assert_eq!(
            compose_window_title(fallback, None, Some("pop-os"), "/Users/me"),
            "Roost-Iced"
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

    /// The terminal widget stamps a bare tab id on every pointer, wheel
    /// and hover event. While a host row is showing, that id is the
    /// host's — resolving it at the local backend would land the gesture
    /// on whichever local tab happens to share the number.
    #[test]
    fn a_terminal_event_qualifies_at_the_host_whose_terminal_is_showing() {
        let local = HostId::LOCAL;
        let remote = HostId::new(4);

        // No host selection: every id is the local backend's, exactly as
        // before the override existed.
        assert_eq!(
            terminal_event_key(TabKey::new(local, 7), local, 7),
            TabKey::new(local, 7)
        );
        assert_eq!(
            terminal_event_key(TabKey::new(local, 7), local, 9),
            TabKey::new(local, 9)
        );

        // A host terminal is showing: its own id is its own key…
        assert_eq!(
            terminal_event_key(TabKey::new(remote, 7), local, 7),
            TabKey::new(remote, 7),
            "the same number under the local backend is a different tab"
        );
        // …and a straggler from a previous frame keeps resolving where it
        // always did.
        assert_eq!(
            terminal_event_key(TabKey::new(remote, 7), local, 9),
            TabKey::new(local, 9)
        );
    }

    fn a_host_selection(host: HostId, project: i64, tab: i64) -> HostSelection {
        HostSelection {
            project: ProjectKey::new(host, project),
            tab: TabKey::new(host, tab),
            local_active: 1,
        }
    }

    /// Attach-on-focus owes a detach-on-defocus. Every route that moves
    /// the selection off a host tab — to a local tab, to another host's
    /// tab, or to nothing — releases that tab's attach exactly once, and
    /// re-asserting the same tab is not a move.
    #[test]
    fn leaving_a_host_tab_releases_its_attach_exactly_once() {
        let host = HostId::new(4);
        let other = HostId::new(5);
        let showing = a_host_selection(host, 1, 7);

        assert_eq!(
            host_selection_detach(Some(showing), None),
            Some(TabKey::new(host, 7)),
            "falling back to the local workspace detaches"
        );
        assert_eq!(
            host_selection_detach(Some(showing), Some(a_host_selection(host, 1, 8))),
            Some(TabKey::new(host, 7)),
            "so does moving within the same host"
        );
        assert_eq!(
            host_selection_detach(Some(showing), Some(a_host_selection(other, 1, 7))),
            Some(TabKey::new(host, 7)),
            "and so does the same number on a different host"
        );
        assert_eq!(
            host_selection_detach(Some(showing), Some(showing)),
            None,
            "re-asserting the same tab is not a move: refocus stays idempotent"
        );
        assert_eq!(
            host_selection_detach(None, Some(showing)),
            None,
            "and there is nothing to release on the way in"
        );
        assert_eq!(host_selection_detach(None, None), None);
    }

    /// What each connected session is told it owns. A session mutes the
    /// tab it believes is focused, so an unfocused window and a
    /// selection on another host both have to read as *no* claim — the
    /// per-host null falls out of that at the send site.
    #[test]
    fn only_a_selected_host_tab_in_a_focused_window_claims_focus() {
        let host = HostId::new(4);
        let showing = a_host_selection(host, 1, 7);

        assert_eq!(
            host_focus_claim(true, Some(showing)),
            Some(TabKey::new(host, 7))
        );
        assert_eq!(
            host_focus_claim(false, Some(showing)),
            None,
            "an unfocused window is looking at nothing, whatever is selected"
        );
        assert_eq!(
            host_focus_claim(true, None),
            None,
            "a local selection claims nothing on any host"
        );
        assert_eq!(host_focus_claim(false, None), None);
    }

    /// What every shipping build answers, and the withheld answer no
    /// build produces today — the two values `spawn_gate` and
    /// [`reconnect_mode`] are written against.
    const FULL_POLICY: host_verbs::VerbPolicy = host_verbs::VerbPolicy {
        localhost_surface: true,
    };
    const GATED_POLICY: host_verbs::VerbPolicy = host_verbs::VerbPolicy {
        localhost_surface: false,
    };

    /// The policy applied where a connection is started, not only where
    /// verbs are listed. A saved `localhost` host can reach the connect
    /// entry with no palette row at all — persisted in `state.json`,
    /// added by `roostctl host add`, then reconnected from the sidebar's
    /// inline ↻ — so a build withholding the surface must refuse the
    /// spawn ladder there too. Dialing a session that IS running still
    /// has to work: only the spawning mode is downgraded.
    #[test]
    fn a_gated_policy_refuses_to_spawn_a_session_but_still_connects_to_one() {
        use crate::host_conn::ConnectMode;

        assert_eq!(
            spawn_gate(ConnectMode::SpawnIfMissing, GATED_POLICY),
            ConnectMode::IfPresent,
            "a client without the surface probes and reports honestly instead of spawning"
        );
        assert_eq!(
            spawn_gate(ConnectMode::SpawnIfMissing, FULL_POLICY),
            ConnectMode::SpawnIfMissing
        );
        // Every other mode is already spawn-free, under both answers.
        for mode in [ConnectMode::IfPresent, ConnectMode::Dial] {
            assert_eq!(spawn_gate(mode, GATED_POLICY), mode);
            assert_eq!(spawn_gate(mode, FULL_POLICY), mode);
        }
    }

    /// Launch auto-reconnect reads the same policy the palette does. A
    /// withholding build declines the dial outright rather than holding
    /// a localhost connection `verbs()` offers no way to leave; a remote
    /// host is never dialed at launch under either answer (D8).
    #[test]
    fn launch_reconnect_dials_only_a_localhost_host_the_policy_offers() {
        use crate::host_conn::ConnectMode;

        assert_eq!(
            reconnect_mode(FULL_POLICY, true),
            Some(ConnectMode::IfPresent),
            "connect-if-present, never a spawn"
        );
        assert_eq!(reconnect_mode(GATED_POLICY, true), None);
        assert_eq!(reconnect_mode(FULL_POLICY, false), None);
        assert_eq!(reconnect_mode(GATED_POLICY, false), None);
    }

    /// The pending-selection wait is bounded. A tab that exits the
    /// instant it spawns is closed again before any batch lists it, and
    /// the connection stays up throughout — so without a deadline the
    /// entry would sit armed for the rest of the session.
    #[test]
    fn a_pending_host_selection_gives_up_eventually() {
        let armed = Instant::now();
        let pending = PendingHostSelection {
            tab: TabKey::new(HostId::new(3), 7),
            armed,
        };
        assert!(
            !pending.expired(armed),
            "the round trip has not happened yet"
        );
        assert!(
            !pending.expired(armed + PENDING_HOST_SELECTION_DEADLINE - Duration::from_millis(1))
        );
        assert!(pending.expired(armed + PENDING_HOST_SELECTION_DEADLINE));
        assert!(
            !pending.expired(armed - Duration::from_secs(1)),
            "a clock that reads backwards waits rather than abandoning"
        );
    }

    /// `switch_project_N` resolves against the navigation ring now, and
    /// the ring's local section is the authoritative snapshot order — so
    /// the numbering a bare install sees is unchanged.
    #[test]
    fn numeric_switch_helpers_follow_authoritative_snapshot_order() {
        let workspace = Workspace::new();
        let first = workspace.create_project("first", "/tmp").unwrap();
        let first_tab = workspace.open_tab(first.id, "/tmp", "one").unwrap();
        let second_tab = workspace.open_tab(first.id, "/tmp", "two").unwrap();
        let second = workspace.create_project("second", "/tmp").unwrap();
        let second_project_tab = workspace.open_tab(second.id, "/tmp", "three").unwrap();

        let local_ring = |projects: &[Project]| {
            vec![host_sidebar::RingSection {
                host: HostId::LOCAL,
                navigable: true,
                projects: projects.iter().map(|project| project.id).collect(),
            }]
        };
        let at = |sections: &[host_sidebar::RingSection], index: u8| {
            host_sidebar::ring_index(sections, index).map(|key| key.project)
        };
        let sections = local_ring(&workspace.snapshot());
        assert_eq!(at(&sections, 1), Some(first.id));
        assert_eq!(at(&sections, 2), Some(second.id));
        assert_eq!(at(&sections, 0), None);
        assert_eq!(at(&sections, 10), None);

        workspace.reorder_projects(&[second.id, first.id]).unwrap();
        let sections = local_ring(&workspace.snapshot());
        assert_eq!(at(&sections, 1), Some(second.id));
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
            resolve_keyboard_route(false, false, false, false, TabKey::local(7), false),
            KeyboardRoute::None
        );
        assert_eq!(
            resolve_keyboard_route(false, false, false, false, TabKey::local(7), true),
            KeyboardRoute::Terminal(TabKey::local(7))
        );
        assert_eq!(
            resolve_keyboard_route(false, false, false, true, TabKey::local(7), true),
            KeyboardRoute::Palette
        );
        assert_eq!(
            resolve_keyboard_route(false, false, true, true, TabKey::local(7), true),
            KeyboardRoute::Editor
        );
        // A host dialog owns the keyboard over the editor and the
        // palette, both of which it dismisses on the way up (plan 037
        // §3.1) — and yields only to the delete confirmation.
        assert_eq!(
            resolve_keyboard_route(false, true, true, true, TabKey::local(7), true),
            KeyboardRoute::HostDialog
        );
        // An open confirm outranks every other surface, so no keystroke can
        // reach an accelerator or the active PTY while it is up.
        assert_eq!(
            resolve_keyboard_route(true, true, true, true, TabKey::local(7), true),
            KeyboardRoute::Confirm
        );
        assert_eq!(
            resolve_keyboard_route(true, false, false, false, TabKey::local(7), true),
            KeyboardRoute::Confirm
        );
    }

    /// A frozen host frame is not a keyboard target (plan 037 §3.1). The
    /// tab still has a session object — that is what draws the pixels
    /// under the scrim — so liveness has to be the *frame's*, not the
    /// map's, or every keystroke is queued at a connection that is gone
    /// and the terminal reads as hung.
    ///
    /// What it must NOT do is take the keyboard away from the surfaces
    /// that still need it: accelerators run off `KeyboardRoute::None`,
    /// and the upgrade dialog's Esc/Enter outrank the terminal route
    /// either way.
    #[test]
    fn a_frozen_host_frame_stops_being_a_keyboard_target() {
        let host = TabKey::new(HostId::new(3), 7);
        assert!(active_terminal_live(true, false));
        assert!(!active_terminal_live(true, true), "the frame is a corpse");
        assert!(!active_terminal_live(false, false), "and it has no session");

        assert_eq!(
            resolve_keyboard_route(
                false,
                false,
                false,
                false,
                host,
                active_terminal_live(true, true)
            ),
            KeyboardRoute::None,
            "typing into the dimmed frame reaches no PTY"
        );
        assert_eq!(
            resolve_keyboard_route(
                false,
                true,
                false,
                false,
                host,
                active_terminal_live(true, true)
            ),
            KeyboardRoute::HostDialog,
            "and the upgrade prompt over it still owns Esc and Enter"
        );
        assert_eq!(
            ime_preedit_target(KeyboardRoute::None),
            None,
            "a composition has nowhere to land either"
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

    /// Creation follows context (plan 037 §3.1). The selected project's
    /// host decides, and the ⌘⇧N picker hands one in the same shape — so
    /// every route resolves through one function and a tab can never
    /// land on a host its project does not live on.
    #[test]
    fn creation_routes_to_the_instance_the_selection_names() {
        // ⌘N / ⌘T / "+" with a local project selected.
        assert_eq!(
            creation_target(ProjectKey::local(4).host),
            CreationTarget::Local
        );
        // …and with a host project selected.
        let host = HostId::new(9);
        assert_eq!(
            creation_target(ProjectKey::new(host, 4).host),
            CreationTarget::Host(host)
        );
        // A tab's route asks the same question of the same key, which is
        // what makes "a tab never lands on a different host than its
        // project" structural.
        assert_eq!(
            creation_target(TabKey::new(host, 7).host),
            CreationTarget::Host(host)
        );
        // The picker's LOCAL row resolves to the local backend, whose
        // host is `HostId::LOCAL` by construction.
        assert_eq!(creation_target(HostId::LOCAL), CreationTarget::Local);
    }

    /// A confirmation carries the *key* it was asked about all the way
    /// to the delete, not the bare id it matched on.
    ///
    /// That is what routes the confirmed delete to the right place:
    /// `execute_confirmed_delete` reads `project.host` to decide between
    /// the local client and a host's op queue, so a host row's
    /// confirmation has to arrive there still host-qualified. Which rows
    /// the id is matched against is the caller's choice
    /// (`App::project_rows`) — this only pins that the choice survives.
    #[test]
    fn a_confirmation_keeps_the_key_it_was_asked_about() {
        let workspace = Workspace::new();
        let project = workspace.create_project("mirrored", "/tmp").unwrap();
        workspace.open_tab(project.id, "/tmp", "t").unwrap();
        let rows = workspace.snapshot();

        let host = HostId::new(3);
        let target = confirm_delete_target(&rows, ProjectKey::new(host, project.id))
            .expect("the caller supplied the rows this key names");
        assert_eq!(target.project, ProjectKey::new(host, project.id));
        assert_eq!(target.name, "mirrored");
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
