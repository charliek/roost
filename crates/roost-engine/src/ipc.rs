//! Toolkit-neutral JSON IPC handler for a Roost UI adapter.
//!
//! M3a of the daemon-removal refactor — adds the handler so a UI
//! adapter's `app.rs` can wire it in (M3b, first done for the
//! now-removed gtk4-rs UI; iced followed the same seam). The handler
//! consumes a shared [`daemon::Workspace`] + [`daemon::PtySupervisor`]
//! and dispatches each request from the [`roost_ipc::IpcServer`]
//! against them.
//!
//! Threading: the handler trait is `Send + Sync`. tokio drives the
//! accept + read loops on worker threads; the handler itself
//! mutates the workspace via its own internal `Mutex`, so there's
//! no need for the UI adapter's main loop to be involved. The actual
//! UI updates flow through `Workspace::subscribe` — each adapter
//! installs a receiver on its own main-loop mechanism and listens
//! there (Iced drains its subscription on its own event loop).

use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use roost_ipc::agent::{self, TabAgentReportParams};
use roost_ipc::messages::{
    ops, AppActivateParams, AppActiveTerminalFocusedParams, AppActiveTerminalFocusedResult,
    AppCursorShapeParams, AppCursorShapeResult, AppDockBadgeParams, AppDockBadgeResult,
    AppMenuActivateParams, AppMenuDumpParams, AppMenuDumpResult, AppNotificationStatusParams,
    AppNotificationStatusResult, AppRenderStatsParams, AppRenderStatsResult,
    AppSelectedTabIdParams, AppSelectedTabIdResult, AppSetWindowFocusParams, AppUpdateCheckParams,
    AppUpdateStatusParams, AppUpdateStatusResult, AttachPayloadKind, ClipboardDumpParams,
    ClipboardDumpResult, ClipboardWriteParams, EventsSubscribeParams, EventsSubscribeResult,
    IdentifyParams, IdentifyResult, NotificationCreateParams, PaletteActivateParams,
    PaletteDismissParams, PaletteOpenParams, PalettePresentParams, PalettePresentResult,
    PaletteQueryParams, PaletteStateParams, PaletteStateResult, ProjectCreateParams,
    ProjectCreateResult, ProjectDeleteParams, ProjectRenameParams, ProjectReorderParams,
    ResolvedCell, ScreenshotParams, ScreenshotResult, SelectionClearParams, SelectionDumpParams,
    SelectionDumpResult, SelectionSetParams, SessionIdentify, SessionIdentifyParams,
    SessionStopParams, SessionStopResult, SidebarDumpParams, SidebarDumpResult,
    SidebarSetWidthParams, TabAgentReportResult, TabCapturePtyInputParams,
    TabCapturePtyInputResult, TabClearNotificationParams, TabCloseParams,
    TabDispatchMouseEventParams, TabDumpCursor, TabDumpParams, TabDumpResolvedParams,
    TabDumpResolvedResult, TabDumpResult, TabExpandSelectionAtParams, TabExpandSelectionAtResult,
    TabFeedImeParams, TabFeedPtyBytesParams, TabFocusParams, TabFocusResult, TabListResult,
    TabOpenParams, TabOpenResult, TabReorderParams, TabResizeParams, TabSetHookActiveParams,
    TabSetStateParams, TabSetTitleParams, TabWriteParams, WindowMetricsParams, WindowMetricsResult,
    WindowResizeParams, SESSION_PROTOCOL_VERSION,
};
use roost_ipc::{ConnAction, Handler, HandlerError, HandlerOutcome, StopFinalizer};

/// Text snapshot of a tab's terminal viewport, produced on the UI
/// adapter's main thread for the `tab.dump` op. Neutral (lib-side) types so this crate
/// stays independent of the bin's `TerminalView`; the UI fills it from
/// `TerminalView::dump`. `cursor` is `(row, col, visible)`.
pub struct DumpData {
    pub cols: u32,
    pub rows: u32,
    pub cursor: Option<(u32, u32, bool)>,
    pub rows_text: Vec<String>,
}

/// Reply for a [`UiRequest::Screenshot`]: `(png_bytes, width, height)`
/// on success, an error message on failure.
type ScreenshotReply = tokio::sync::oneshot::Sender<Result<(Vec<u8>, u32, u32), String>>;

/// Reply for a [`UiRequest::WindowMetrics`]: the window/sidebar/terminal
/// geometry in logical points. The `Result<_, String>` envelope shape
/// matches every sibling reply (so the shared `ui_call` helper works), but
/// the UI side always answers `Ok` — UI adapter widget/state queries
/// never fail.
type WindowMetricsReply = tokio::sync::oneshot::Sender<Result<WindowMetricsResult, String>>;

/// Reply for [`UiRequest::SidebarDump`]. Read-only: always answers
/// `Ok`, matching `WindowMetricsReply`.
type SidebarDumpReply = tokio::sync::oneshot::Sender<Result<SidebarDumpResult, String>>;

/// Reply for [`UiRequest::AppRenderStats`]: the UI's render-path
/// counters. Read-only; always answers `Ok`, matching
/// `WindowMetricsReply`. A UI with no instrumentation answers with a
/// zeroed struct rather than an error.
type RenderStatsReply = tokio::sync::oneshot::Sender<Result<AppRenderStatsResult, String>>;

/// Reply for a [`UiRequest::Dump`]: the viewport text on success, an
/// error message (e.g. tab not found / no live terminal) on failure.
type DumpReply = tokio::sync::oneshot::Sender<Result<DumpData, String>>;

/// Reply for the `palette.*` [`UiRequest`]s: the resulting palette state.
/// Shared by all five — each mutating op answers with the state it
/// produced, so a driver needs no follow-up `palette.state`. Only
/// `PaletteActivate` ever returns the `Err` arm (no palette open, or no
/// row with the given id); the rest always answer `Ok`.
type PaletteReply = tokio::sync::oneshot::Sender<Result<PaletteStateResult, String>>;

/// Reply for [`UiRequest::PalettePresent`]: the user's choice, delivered
/// once the palette closes (a pick or a dismissal). Unlike the other
/// palette ops, `palette.present` does not reply on open — it blocks
/// like `wait` until the user acts.
type PalettePresentReply = tokio::sync::oneshot::Sender<Result<PalettePresentResult, String>>;

/// Snapshot of a tab's selection for the `selection.dump` op. Mirrors
/// `terminal_view::SelectionDumpData` but lives in this crate so `ipc.rs`
/// stays independent of the bin's `TerminalView`.
pub struct SelectionData {
    pub text: Option<String>,
    pub anchor_visible: bool,
    pub cursor_visible: bool,
}

/// Reply for a [`UiRequest::SelectionDump`]: `Some` carries the current
/// selection (which may itself have `text == None` for an off-screen
/// selection); `None` means no selection is active on the tab.
/// `Err` means the tab id has no live terminal.
type SelectionDumpReply = tokio::sync::oneshot::Sender<Result<Option<SelectionData>, String>>;

/// Reply for [`UiRequest::SelectionSet`] / [`UiRequest::SelectionClear`]:
/// `Ok(())` when applied, `Err` with a `not-found` style message when no
/// live tab matches.
type SelectionMutReply = tokio::sync::oneshot::Sender<Result<(), String>>;

/// Reply for [`UiRequest::ClipboardDump`]: the pasteboard contents
/// (`Ok(Some)` = text present, `Ok(None)` = empty target / PRIMARY off
/// Linux). The `Err` arm is never used today but kept for shape
/// compatibility with `ui_call`'s `Result<T, String>` envelope.
type ClipboardDumpReply = tokio::sync::oneshot::Sender<Result<Option<String>, String>>;

/// Reply for [`UiRequest::TabFeedPtyBytes`]: `Ok(())` when the bytes
/// were enqueued onto the tab's output channel, `Err` when the tab id
/// has no live terminal or `ROOST_TEST_MODE=1` was absent at launch.
type UnitReply = tokio::sync::oneshot::Sender<Result<(), String>>;

/// Reply for [`UiRequest::TabCapturePtyInput`]: the bytes the UI has
/// queued onto this tab's PTY-input channel since the last drain.
/// `Err` for unknown tab or missing test-mode env var.
type CapturedBytesReply = tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>;

/// Reply for [`UiRequest::TabDumpResolved`]: every cell on the tab's
/// terminal viewport after the production color resolver has run.
/// `Err` only for unknown tab; this op is ungated.
type DumpResolvedReply = tokio::sync::oneshot::Sender<Result<ResolvedCellsData, String>>;

/// Reply for [`UiRequest::TabExpandSelectionAt`]: the (col0, col1, text)
/// triple matching the committed selection. `Err` for unknown tab,
/// missing test-mode env var, or an out-of-range coord the renderer
/// can't pin.
pub struct ExpandSelectionData {
    pub col0: u16,
    pub col1: u16,
    pub text: Option<String>,
}
type ExpandSelectionReply = tokio::sync::oneshot::Sender<Result<ExpandSelectionData, String>>;

/// Resolver-output snapshot for [`UiRequest::TabDumpResolved`]. Lives
/// in this crate (like [`SelectionData`]) so the wire layer stays
/// independent of the UI's `TerminalView`. The dispatch arm maps it
/// to the wire-format [`roost_ipc::messages::TabDumpResolvedResult`].
pub struct ResolvedCellsData {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<ResolvedCellData>,
}

/// One cell of [`ResolvedCellsData`]. Fields are normalized: `text`
/// is `" "` for blank cells, `fg`/`bg` are the post-resolver colors
/// (after bold-color, inverse swap, etc.), `has_explicit_bg`
/// distinguishes default-bg cells from SGR-bg cells.
pub struct ResolvedCellData {
    pub row: u32,
    pub col: u16,
    pub text: String,
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub has_explicit_bg: bool,
    pub bold: bool,
    pub italic: bool,
    pub inverse: bool,
}

/// One unit of work the IPC handler (a tokio worker thread) hands to the
/// UI adapter's main thread — the single seam for anything an op needs
/// to do against the UI toolkit / libghostty, which are main-thread-only.
/// The UI drains
/// one channel of these and matches; request-reply variants carry a
/// `oneshot` the main thread answers on. Adding a UI-touching op is a
/// new variant here + one arm in the UI's drain loop, instead of a
/// fresh per-op channel + handler field + setter + receiver + wiring.
pub enum UiRequest {
    /// Raise + focus the running window (#6). Fire-and-forget.
    Activate,
    /// Render the whole window (sidebar + tabs + active terminal) to a
    /// PNG.
    Screenshot { scale: u32, reply: ScreenshotReply },
    /// Read a tab's terminal viewport as text.
    Dump { tab_id: i64, reply: DumpReply },
    /// Open a command-palette root frame and reply with its state.
    /// `kind`: "" / "commands" → command palette; "launcher" → the
    /// custom-command launcher.
    PaletteOpen { kind: String, reply: PaletteReply },
    /// Reply with the current palette state (open?, frame, query, rows).
    PaletteState { reply: PaletteReply },
    /// Set the current frame's filter; reply with the filtered state.
    PaletteQuery { query: String, reply: PaletteReply },
    /// Activate the visible row with this item id — the same dispatch as
    /// its keybind — and reply with the resulting state.
    PaletteActivate { id: String, reply: PaletteReply },
    /// Dismiss any open palette; reply with the (closed) state.
    PaletteDismiss { reply: PaletteReply },
    /// Open the palette on a caller-supplied list and reply once the
    /// user picks a row or dismisses (blocking — the reply is deferred,
    /// not sent on open). Items are `(id, title, subtitle)`.
    PalettePresent {
        title: String,
        placeholder: String,
        items: Vec<(String, String, Option<String>)>,
        reply: PalettePresentReply,
    },
    /// `selection.set` — anchor a selection on a tab's terminal.
    /// Both points are viewport `(col, row)`; the UI pins each endpoint
    /// with a tracked grid ref.
    SelectionSet {
        tab_id: i64,
        anchor: (u16, u16),
        cursor: (u16, u16),
        reply: SelectionMutReply,
    },
    /// `selection.clear` — drop any active selection on this tab.
    SelectionClear {
        tab_id: i64,
        reply: SelectionMutReply,
    },
    /// `selection.dump` — read back the current selection.
    SelectionDump {
        tab_id: i64,
        reply: SelectionDumpReply,
    },
    /// `clipboard.dump` — read the host pasteboard. `target` is the
    /// normalized string from the wire ("system" or "selection") which
    /// the UI maps to the platform's CLIPBOARD / PRIMARY (Linux) or
    /// `NSPasteboard.general` / `selectionPasteboard` (Mac on the
    /// parallel implementation).
    ClipboardDump {
        target: ClipboardOp,
        reply: ClipboardDumpReply,
    },
    /// `clipboard.write` — test-only pasteboard seeding.
    ClipboardWrite { target: ClipboardOp, text: String },
    /// `tab.feed_pty_bytes` — inject bytes into a tab's PTY-output
    /// drain as if the supervisor had emitted them. The UI side
    /// rejects (`Err`) when `ROOST_TEST_MODE=1` was not set at
    /// launch.
    TabFeedPtyBytes {
        tab_id: i64,
        data: Vec<u8>,
        reply: UnitReply,
    },
    /// `tab.capture_pty_input` — read (and optionally drain) the
    /// bytes the UI has queued onto a tab's PTY-input channel.
    /// Gated like `TabFeedPtyBytes`.
    TabCapturePtyInput {
        tab_id: i64,
        drain: bool,
        reply: CapturedBytesReply,
    },
    /// `tab.dump_resolved` — return every cell on a tab's terminal
    /// viewport after the production color resolver has run. Ungated
    /// (no shadow state — same walk the real paint loop runs).
    TabDumpResolved {
        tab_id: i64,
        reply: DumpResolvedReply,
    },
    /// `tab.expand_selection_at` — run the production
    /// double-/triple-click word/line dispatch against `(col, row)`
    /// and commit the resulting span as the tab's selection. Gated
    /// like `TabFeedPtyBytes` (ROOST_TEST_MODE=1).
    TabExpandSelectionAt {
        tab_id: i64,
        col: u16,
        row: u16,
        click_count: u8,
        reply: ExpandSelectionReply,
    },
    /// `tab.feed_ime` — drive an IME preedit/commit/session-boundary
    /// event through the terminal's active keyboard route, the same
    /// production path (`ime_preedit` / `ime_commit` /
    /// `ime_session_boundary`) a real IME event takes. `action` is
    /// `"preedit" | "commit" | "clear"`. Routes by the UI's keyboard
    /// route, not directly by `tab_id`: the UI rejects (`Err`) when
    /// `tab_id` doesn't match the tab currently holding the route.
    /// Gated like `TabFeedPtyBytes` (ROOST_TEST_MODE=1).
    TabFeedIme {
        tab_id: i64,
        action: String,
        text: String,
        cursor: Option<Range<usize>>,
        reply: UnitReply,
    },
    /// `app.window_metrics` — read window size + sidebar pane width +
    /// collapsed flag (logical points). Backs the sidebar-holds-width
    /// regression suite. Ungated (read-only).
    WindowMetrics { reply: WindowMetricsReply },
    /// `app.render_stats` — read the UI's render-path counters, and
    /// zero them afterward when `reset`. Ungated. Not read-only: with
    /// `reset` it reads and then clears. The counters are the only way
    /// to measure the real draw path, which needs a live renderer no
    /// unit test can construct.
    AppRenderStats {
        reset: bool,
        reply: RenderStatsReply,
    },
    /// `app.sidebar_dump` — read the sidebar's last-rendered agent rows
    /// per project, plus the agents-visible toggle. Ungated (read-only);
    /// reads `ProjectUi::rendered_agents`, the same cache the sidebar
    /// paints from (plan 007 §3.8).
    SidebarDump { reply: SidebarDumpReply },
    /// `window.resize` — programmatically set the window's logical
    /// size. Gated for the same reason as the PTY drain ops.
    WindowResize {
        width: f64,
        height: f64,
        reply: UnitReply,
    },
    /// `sidebar.set_width` — programmatically set the projects
    /// sidebar's logical width. The UI routes it through
    /// `Workspace::set_sidebar_width`, which clamps and persists, so an
    /// out-of-band width lands at the nearest bound. Gated like
    /// `TabFeedPtyBytes` (ROOST_TEST_MODE=1); drives the sidebar-resize
    /// e2e.
    SidebarSetWidth { width: f64, reply: UnitReply },
    /// `tab.dispatch_mouse_event` — drive a synthetic mouse event
    /// into the production routing path at cell-grid coords. Same
    /// path the real GestureClick / GestureDrag / EventControllerMotion
    /// take. Gated on `ROOST_TEST_MODE=1`.
    TabDispatchMouseEvent {
        tab_id: i64,
        kind: crate::pointer::PointerAction,
        button: Option<crate::pointer::PointerButton>,
        cell_x: u32,
        cell_y: u32,
        mods: u32,
        reply: UnitReply,
    },
    /// `app.set_window_focus` — drive the focus-tracking emit path
    /// without actually changing native window focus. Targets the active tab.
    /// Gated on `ROOST_TEST_MODE=1`.
    AppSetWindowFocus { focused: bool, reply: UnitReply },
    /// `app.cursor_shape` — return the active tab's effective W3C cursor name,
    /// including a UI-owned link-hover override when present. Ungated
    /// (read-only).
    AppCursorShape {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    /// `app.active_terminal_focused` — return whether the active tab's
    /// terminal owns the UI's logical keyboard route. Ungated (read-only).
    AppActiveTerminalFocused {
        reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    /// `app.selected_tab_id` — return the active project's on-screen
    /// selected tab id (UI truth). Ungated (read-only).
    AppSelectedTabId {
        reply: tokio::sync::oneshot::Sender<Result<i64, String>>,
    },
    /// `app.dock_badge` — read the macOS Dock tile's live badge label
    /// (`None` when cleared). The UI reads AppKit rather than
    /// recomputing from its notification inbox, so the op proves the
    /// badge write actually landed. Gated like `TabFeedPtyBytes`
    /// (ROOST_TEST_MODE=1); macOS iced only — the other UIs reject.
    AppDockBadge {
        reply: tokio::sync::oneshot::Sender<Result<Option<String>, String>>,
    },
    /// `app.menu_dump` — read back the live native menu bar the macOS
    /// iced UI installed, walking `NSApp.mainMenu` itself rather than
    /// re-deriving from the keybind table. Gated + macOS-iced-only like
    /// `AppDockBadge`.
    AppMenuDump {
        reply: tokio::sync::oneshot::Sender<Result<AppMenuDumpResult, String>>,
    },
    /// `app.menu_activate` — resolve `path` through the live native
    /// menu bar by title and fire it via
    /// `performActionForItemAtIndex:`, the same dispatch a real click
    /// takes. Gated + macOS-iced-only like `AppDockBadge`.
    AppMenuActivate {
        path: Vec<String>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// `app.update_status` — read back the macOS iced UI's Sparkle
    /// updater state (framework loaded, updater started, last completed
    /// check). Gated + macOS-iced-only like `AppDockBadge`.
    AppUpdateStatus {
        reply: tokio::sync::oneshot::Sender<Result<AppUpdateStatusResult, String>>,
    },
    /// `app.update_check` — start a non-interactive
    /// `checkForUpdateInformation` on the Sparkle updater. Results land
    /// in `AppUpdateStatus`. Gated + macOS-iced-only like
    /// `AppDockBadge`.
    AppUpdateCheck {
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// `app.notification_status` — read back the macOS iced UI's
    /// `UNUserNotificationCenter` backend state (whether the delegate
    /// installed, whether the user authorized notifications). Gated +
    /// macOS-iced-only like `AppDockBadge`.
    AppNotificationStatus {
        reply: tokio::sync::oneshot::Sender<Result<AppNotificationStatusResult, String>>,
    },
}

/// Resolved clipboard target for the `clipboard.*` ops. Lives in this
/// crate so the wire-string → platform-target mapping happens at the
/// dispatcher boundary, not in the UI drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOp {
    System,
    Selection,
}

use crate::event_push::{self, PushLimits};
use crate::{AttentionSource, PtyError, PtySupervisor, Workspace, WorkspaceError};

/// How long `session.stop` lets a hung-up child live before it escalates
/// to SIGKILL. Long enough for a shell to run its exit traps and for an
/// agent to finish a write, short enough that a stuck child can't hold a
/// remote client's `roostctl session stop` open indefinitely.
pub const SESSION_STOP_SOFT_DEADLINE: Duration = Duration::from_secs(5);

/// Identity a host session answers `session.identify` with, plus the
/// session-local defaults that differ from a UI socket's.
///
/// Constructed by the daemon and installed with
/// [`IpcHandler::with_session`]. A handler without one is a UI socket:
/// `session.*` falls through to `unknown-op` and every default keeps the
/// value it has always had.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    /// RFC3339, carried as a string on the wire.
    pub started_at: String,
    pub app_version: String,
    /// What this session can encode a tab's attach payload as. Empty
    /// until HS-1b implements the attach data plane — an honest "attach
    /// unavailable" rather than a promise the session cannot keep.
    pub payload_kinds: Vec<AttachPayloadKind>,
    /// Pinned libghostty build identity, which must match exactly for
    /// [`AttachPayloadKind::GHOSTTY_SNAPSHOT`] to be negotiable. Empty
    /// in this slice for the same reason as `payload_kinds`.
    pub libghostty_build: String,
    /// `(cols, rows)` a `tab.open` that omits both falls back to. A
    /// headless session has no window to measure, so the daemon states
    /// the size rather than inheriting a UI's 80×24.
    pub default_tab_size: (u16, u16),
}

/// The process-level shutdown tail a `session.stop` runs *after* its
/// reply is on the wire — stop accepting, unlink the socket, exit.
///
/// Supplied by the daemon; boxed so this crate never learns what the
/// process does about it, and `Fn` rather than `FnOnce` because the
/// handler holds it behind a shared reference (the stop latch, not the
/// type, is what makes it run at most once).
#[derive(Clone)]
pub struct StopHandle(Arc<dyn Fn() -> StopFuture + Send + Sync>);

type StopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

impl StopHandle {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self(Arc::new(move || Box::pin(f())))
    }

    async fn run(&self) {
        (self.0)().await;
    }
}

impl std::fmt::Debug for StopHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StopHandle")
    }
}

/// Session-socket state: the identity, the stop tail, and the two
/// primitives that make `session.stop` a clean cut.
struct SessionState {
    info: SessionInfo,
    stop: StopHandle,
    /// Latched once, never cleared. Set *before* the barrier is taken,
    /// so a mutating op that has not yet acquired the barrier is
    /// guaranteed to see it.
    stopping: AtomicBool,
    /// Mutating dispatches hold this for read; `session.stop` takes it
    /// for write after latching. That makes stop wait for exactly the
    /// mutations already in flight — a `tab.open` that got past the
    /// latch completes, and its tab joins the reap set — while every
    /// later one is rejected.
    barrier: tokio::sync::RwLock<()>,
    /// Live `events.subscribe` relays, so a stop can end them.
    ///
    /// A connection that flipped to push mode never finishes on its own
    /// — nothing on it is request-shaped any more — so the stop has to
    /// reach in and abort the relay, whose dropped sender closes the
    /// connection. `None` once the stop has swept: a subscribe that
    /// raced the sweep is refused rather than registered into a list
    /// nobody will ever read again.
    pushes: std::sync::Mutex<Option<Vec<tokio::task::AbortHandle>>>,
}

impl SessionState {
    /// Register a live relay, or report that the session is already
    /// stopping. Prunes finished handles on the way through — the list
    /// is only ever walked here and at the sweep, so this is where a
    /// closed subscriber's entry goes away.
    fn register_push(&self, handle: tokio::task::AbortHandle) -> bool {
        let mut guard = lock(&self.pushes);
        let Some(pushes) = guard.as_mut() else {
            return false;
        };
        pushes.retain(|h| !h.is_finished());
        pushes.push(handle);
        true
    }

    /// End every live relay and refuse further ones.
    fn abort_pushes(&self) {
        let taken = lock(&self.pushes).take();
        for handle in taken.into_iter().flatten() {
            handle.abort();
        }
    }
}

/// Lock recovering from poisoning: a panicked holder must not be able to
/// wedge a session's shutdown, and every field behind this lock is a
/// list of abort handles — there is no invariant a panic could have
/// broken halfway.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Ops that change workspace or PTY state, and so must not run once a
/// session has latched Stopping.
///
/// Reads (`identify`, `tab.list`, `tab.dump*`, `session.identify`) stay
/// answerable throughout, so a client can still find out what happened.
/// The UI-only ops (`palette.*`, `window.*`, `app.*`, clipboard,
/// selection, and the test-mode feed ops) are not listed: they route
/// through `ui_call`, and a session socket has no UI attached, so they
/// already fail with `internal: no UI attached`.
fn is_mutating_op(op: &str) -> bool {
    matches!(
        op,
        ops::TAB_OPEN
            | ops::TAB_CLOSE
            | ops::TAB_WRITE
            | ops::TAB_RESIZE
            | ops::TAB_FOCUS
            | ops::TAB_SET_TITLE
            | ops::TAB_SET_STATE
            | ops::TAB_CLEAR_NOTIFICATION
            | ops::TAB_SET_HOOK_ACTIVE
            | ops::TAB_AGENT_REPORT
            | ops::TAB_REORDER
            | ops::PROJECT_CREATE
            | ops::PROJECT_RENAME
            | ops::PROJECT_DELETE
            | ops::PROJECT_REORDER
            | ops::NOTIFICATION_CREATE
    )
}

fn shutting_down() -> HandlerError {
    HandlerError::new("shutting-down", "session is shutting down")
}

/// Glue between the JSON IPC server and the in-process workspace +
/// PTY supervisor.
pub struct IpcHandler {
    pub workspace: Arc<Workspace>,
    pub supervisor: Arc<PtySupervisor>,
    /// Absolute path to the IPC socket. Echoed in `identify` and
    /// injected as `ROOST_SOCKET` into spawned shells.
    pub socket_path: PathBuf,
    /// App label / app id pair from the active bundle profile.
    pub app_label: String,
    pub app_id: String,
    /// Set by the running UI: ops that must touch the UI toolkit /
    /// libghostty (activate, screenshot, dump) forward a [`UiRequest`] here for the
    /// main thread to service. `None` in headless contexts (tests), so
    /// those ops no-op (activate) or error `internal` (screenshot/dump).
    ui_tx: Option<tokio::sync::mpsc::UnboundedSender<UiRequest>>,
    /// Set by the host-session daemon. `None` on every UI socket, which
    /// is what makes `session.*` an `unknown-op` there.
    session: Option<Arc<SessionState>>,
    /// Bounds on one `events.subscribe` subscriber's delivery. Only a
    /// session socket ever serves that op, so this is inert on a UI
    /// socket.
    push_limits: PushLimits,
}

impl IpcHandler {
    pub fn new(
        workspace: Arc<Workspace>,
        supervisor: Arc<PtySupervisor>,
        socket_path: PathBuf,
        app_label: impl Into<String>,
        app_id: impl Into<String>,
    ) -> Self {
        Self {
            workspace,
            supervisor,
            socket_path,
            app_label: app_label.into(),
            app_id: app_id.into(),
            ui_tx: None,
            session: None,
            push_limits: PushLimits::default(),
        }
    }

    /// Wire the UI request channel so main-thread-only ops (activate,
    /// screenshot, dump) can reach the UI toolkit / libghostty. The UI
    /// installs the sender; the matching receiver is drained on the UI
    /// adapter's main thread.
    pub fn with_ui(mut self, tx: tokio::sync::mpsc::UnboundedSender<UiRequest>) -> Self {
        self.ui_tx = Some(tx);
        self
    }

    /// Promote this handler to a host-session socket: `session.identify`
    /// and `session.stop` start answering, `tab.open`'s size fallback
    /// comes from `session.default_tab_size`, and every mutating op
    /// becomes gated on the stop latch.
    ///
    /// A handler built without this is a UI socket and is wire-identical
    /// to what it has always been.
    #[must_use]
    pub fn with_session(mut self, session: SessionInfo, stop: StopHandle) -> Self {
        self.session = Some(Arc::new(SessionState {
            info: session,
            stop,
            stopping: AtomicBool::new(false),
            barrier: tokio::sync::RwLock::new(()),
            pushes: std::sync::Mutex::new(Some(Vec::new())),
        }));
        self
    }

    /// Narrow the bounds on an `events.subscribe` subscriber's delivery.
    ///
    /// A test seam: forcing the overflow branch means a queue a test can
    /// fill and a stall budget it can outwait, neither of which the
    /// shipped defaults are. Production leaves this alone.
    #[must_use]
    pub fn with_push_limits(mut self, limits: PushLimits) -> Self {
        self.push_limits = limits;
        self
    }

    /// Hand a request-reply [`UiRequest`] to the UI adapter's main thread
    /// and await its answer. The outer `Result` reports channel/UI health
    /// (no UI attached, UI gone, reply dropped); the inner `Result` is
    /// the op's own outcome, which the caller maps to the right error
    /// code (e.g. `not-found` for a missing tab). Shared by the
    /// screenshot + dump arms so the oneshot plumbing lives in one place.
    async fn ui_call<T>(
        &self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> UiRequest,
    ) -> Result<Result<T, String>, HandlerError> {
        let tx = self
            .ui_tx
            .as_ref()
            .ok_or_else(|| HandlerError::new("internal", "no UI attached"))?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(make(reply_tx))
            .map_err(|_| HandlerError::new("internal", "UI gone"))?;
        reply_rx
            .await
            .map_err(|_| HandlerError::new("internal", "UI dropped reply"))
    }
}

impl Handler for IpcHandler {
    fn handle<'a>(
        &'a self,
        op: &'a str,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        Box::pin(async move { dispatch_outcome(self, op, params).await })
    }
}

/// The session layer wrapped around the op dispatcher: `session.*`, the
/// stop latch, and the mutation barrier. Without a [`SessionState`] this
/// is a straight pass-through, so a UI socket's wire behavior is exactly
/// what it was before host sessions existed.
async fn dispatch_outcome(
    h: &IpcHandler,
    op: &str,
    params: serde_json::Value,
) -> Result<HandlerOutcome, HandlerError> {
    let Some(session) = h.session.as_ref() else {
        return dispatch(h, op, params).await.map(HandlerOutcome::Reply);
    };

    match op {
        ops::SESSION_IDENTIFY => {
            let _p: SessionIdentifyParams = decode(params)?;
            let result = SessionIdentify {
                app_version: session.info.app_version.clone(),
                session_protocol: SESSION_PROTOCOL_VERSION,
                payload_kinds: session.info.payload_kinds.clone(),
                libghostty_build: session.info.libghostty_build.clone(),
                session_id: session.info.session_id.clone(),
                started_at: session.info.started_at.clone(),
            };
            return encode(&result).map(HandlerOutcome::Reply);
        }
        ops::SESSION_STOP => {
            let _p: SessionStopParams = decode(params)?;
            return session_stop(h, session).await;
        }
        ops::EVENTS_SUBSCRIBE => {
            let p: EventsSubscribeParams = decode(params)?;
            return events_subscribe(h, session, &p);
        }
        _ => {}
    }

    if !is_mutating_op(op) {
        return dispatch(h, op, params).await.map(HandlerOutcome::Reply);
    }

    if session.stopping.load(Ordering::Acquire) {
        return Err(shutting_down());
    }
    let _admitted = session.barrier.read().await;
    // Re-check under the barrier. The latch is set before `session.stop`
    // asks for the write guard, so an op that read `false` above but only
    // acquired the read guard after stop released it would otherwise
    // mutate a session that has already flushed and reaped.
    if session.stopping.load(Ordering::Acquire) {
        return Err(shutting_down());
    }
    dispatch(h, op, params).await.map(HandlerOutcome::Reply)
}

/// `events.subscribe` on a session socket: ack with the fence, then push.
///
/// Provisional (plan 035 D4). HS-1b makes the stream lease-gated, which
/// is a breaking change to this op; today only Roost's own tests consume
/// it, which is what makes shipping the unleased form acceptable.
///
/// Not a mutating op — it changes no workspace state — but it does
/// establish a resource, so it is refused once the session has latched:
/// a stream handed out after the stop swept the registry would be one
/// nobody can end. [`SessionState::register_push`] closes the race by
/// making the registration itself the check.
fn events_subscribe(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    params: &EventsSubscribeParams,
) -> Result<HandlerOutcome, HandlerError> {
    if params.tab_id_filter != 0 {
        return Err(HandlerError::invalid_param(format!(
            "tab_id_filter is not implemented (got {}); subscribe unfiltered and filter \
             client-side until HS-2 adds it",
            params.tab_id_filter
        )));
    }
    if session.stopping.load(Ordering::Acquire) {
        return Err(shutting_down());
    }
    let (revision, source, handle) = event_push::spawn(&h.workspace, h.push_limits);
    if !session.register_push(handle.clone()) {
        // Lost the race with the stop's sweep. Abort what we just
        // started rather than leaking a relay the stop will never see.
        handle.abort();
        return Err(shutting_down());
    }
    Ok(HandlerOutcome::ReplyThen {
        reply: encode(&EventsSubscribeResult { revision })?,
        then: ConnAction::StartPush(source),
    })
}

/// `session.stop`: latch, barrier, flush, reap, reply, *then* finalize.
///
/// The reply is the reap report and it goes out before the process-level
/// tail runs — that ordering is why the finalizer travels back as a
/// [`ConnAction::FinalizeStop`] instead of being awaited here.
async fn session_stop(
    h: &IpcHandler,
    session: &Arc<SessionState>,
) -> Result<HandlerOutcome, HandlerError> {
    // Idempotent-reject: the first caller owns the shutdown, a second
    // gets the same answer any other post-latch op gets.
    if session.stopping.swap(true, Ordering::AcqRel) {
        return Err(shutting_down());
    }

    // After the latch, before the barrier. A push connection answers no
    // requests, so it is not something the barrier can wait out — it has
    // to be cut. The plain close is the client's notification: the same
    // signal it already handles as "resync", and the only one available
    // on a connection that stopped being request/response.
    session.abort_pushes();

    // Waits out exactly the mutations that got past the latch.
    let _drained = session.barrier.write().await;

    h.workspace.flush();
    let report = h.supervisor.shutdown_all(SESSION_STOP_SOFT_DEADLINE).await;
    let reply = encode(&SessionStopResult {
        reaped: report.reaped,
        killed: report.killed,
        abandoned: report.abandoned,
    })?;

    let stop = session.stop.clone();
    Ok(HandlerOutcome::ReplyThen {
        reply,
        then: ConnAction::FinalizeStop(StopFinalizer::new(move || async move { stop.run().await })),
    })
}

async fn dispatch(
    h: &IpcHandler,
    op: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, HandlerError> {
    match op {
        ops::IDENTIFY => {
            let _p: IdentifyParams = decode(params)?;
            let (active_project_id, active_tab_id) = h.workspace.active();
            let result = IdentifyResult {
                socket_path: h.socket_path.to_string_lossy().into(),
                pid: std::process::id() as i32,
                active_project_id,
                active_tab_id,
                app_label: h.app_label.clone(),
                app_id: h.app_id.clone(),
                ui_version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: roost_ipc::PROTOCOL_VERSION,
            };
            encode(&result)
        }
        ops::TAB_OPEN => {
            let p: TabOpenParams = decode(params)?;
            let project_id = if p.project_id == 0 {
                h.workspace.ensure_default_project(&p.cwd)
            } else {
                p.project_id
            };
            let tab = h
                .workspace
                .open_tab(project_id, &p.cwd, &p.title)
                .map_err(ws_err)?;
            // Spawn the PTY. Use the tab's cwd, the requested argv,
            // and a sensible default winsize when the caller doesn't
            // provide one. Reject out-of-range cols/rows with
            // `invalid-param` instead of silently truncating —
            // CR-flagged on PR #78.
            let (default_cols, default_rows) = h
                .session
                .as_ref()
                .map_or((80u16, 24u16), |s| s.info.default_tab_size);
            let cols = if p.cols == 0 {
                default_cols
            } else {
                u16::try_from(p.cols)
                    .map_err(|_| HandlerError::invalid_param("cols out of u16 range"))?
            };
            let rows = if p.rows == 0 {
                default_rows
            } else {
                u16::try_from(p.rows)
                    .map_err(|_| HandlerError::invalid_param("rows out of u16 range"))?
            };
            if let Err(err) =
                h.supervisor
                    .spawn(tab.id, &tab.cwd, &p.argv, cols, rows, &h.socket_path)
            {
                // PTY spawn failed — roll back the tab so the
                // workspace doesn't carry a phantom.
                let _ = h.workspace.close_tab(tab.id);
                // A `Cancelled` here means the user (or another
                // caller) closed the same tab id between our
                // workspace insert and the supervisor's promote.
                // Surface that as `not-found` so the client sees
                // the same code as any other "tab gone" path
                // rather than misclassifying it as a server fault.
                if let Some(PtyError::Cancelled(_)) = err.downcast_ref::<PtyError>() {
                    return Err(HandlerError::not_found(err.to_string()));
                }
                return Err(HandlerError::new(
                    "internal",
                    format!("pty spawn failed: {err}"),
                ));
            }
            encode(&TabOpenResult { tab })
        }
        ops::TAB_CLOSE => {
            let p: TabCloseParams = decode(params)?;
            h.supervisor.close(p.tab_id);
            h.workspace.close_tab(p.tab_id).map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_LIST => {
            // Read under the snapshot's own lock: a separate revision
            // read would race a commit and hand back a fence that does
            // not describe the projects next to it. It rides along only
            // where it means something — a session socket, which also
            // serves the event stream it fences.
            let (revision, projects) = h.workspace.snapshot_with_revision();
            encode(&TabListResult {
                projects,
                revision: h.session.is_some().then_some(revision),
            })
        }
        ops::TAB_WRITE => {
            let p: TabWriteParams = decode(params)?;
            h.supervisor
                .write(p.tab_id, p.data)
                .await
                .map_err(pty_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_RESIZE => {
            let p: TabResizeParams = decode(params)?;
            let cols = u16::try_from(p.cols)
                .map_err(|_| HandlerError::invalid_param("cols out of u16 range"))?;
            let rows = u16::try_from(p.rows)
                .map_err(|_| HandlerError::invalid_param("rows out of u16 range"))?;
            h.supervisor
                .resize(p.tab_id, cols, rows)
                .await
                .map_err(pty_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_DUMP => {
            let p: TabDumpParams = decode(params)?;
            let data = h
                .ui_call(|reply| UiRequest::Dump {
                    tab_id: p.tab_id,
                    reply,
                })
                .await?
                .map_err(HandlerError::not_found)?;
            encode(&TabDumpResult {
                cols: data.cols,
                rows: data.rows,
                cursor: data
                    .cursor
                    .map(|(row, col, visible)| TabDumpCursor { row, col, visible }),
                rows_text: data.rows_text,
            })
        }
        ops::PROJECT_CREATE => {
            let p: ProjectCreateParams = decode(params)?;
            let project = h
                .workspace
                .create_project(&p.name, &p.cwd)
                .map_err(ws_err)?;
            encode(&ProjectCreateResult { project })
        }
        ops::PROJECT_RENAME => {
            let p: ProjectRenameParams = decode(params)?;
            h.workspace
                .rename_project(p.project_id, &p.name)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::PROJECT_DELETE => {
            let p: ProjectDeleteParams = decode(params)?;
            let cascaded = h.workspace.delete_project(p.project_id).map_err(ws_err)?;
            for tab_id in cascaded {
                h.supervisor.close(tab_id);
            }
            Ok(serde_json::json!({}))
        }
        ops::TAB_REORDER => {
            let p: TabReorderParams = decode(params)?;
            h.workspace
                .reorder_tabs(p.project_id, &p.tab_ids)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::PROJECT_REORDER => {
            let p: ProjectReorderParams = decode(params)?;
            h.workspace
                .reorder_projects(&p.project_ids)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_FOCUS => {
            let p: TabFocusParams = decode(params)?;
            let (previous_project_id, previous_tab_id) =
                h.workspace.focus_tab(p.tab_id).map_err(ws_err)?;
            encode(&TabFocusResult {
                previous_project_id,
                previous_tab_id,
            })
        }
        ops::TAB_SET_TITLE => {
            let p: TabSetTitleParams = decode(params)?;
            h.workspace
                .set_tab_title(p.tab_id, &p.title)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_SET_STATE => {
            let p: TabSetStateParams = decode(params)?;
            h.workspace
                .set_tab_state(p.tab_id, p.state)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_CLEAR_NOTIFICATION => {
            let p: TabClearNotificationParams = decode(params)?;
            h.workspace
                .set_tab_has_notification(p.tab_id, false)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_SET_HOOK_ACTIVE => {
            // Deprecated alias for `tab.agent_report` — claim/release as
            // `legacy` with an empty session id (plan 002 §3.6).
            let p: TabSetHookActiveParams = decode(params)?;
            h.workspace
                .set_tab_hook_active(p.tab_id, p.active)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_AGENT_REPORT => {
            let p: TabAgentReportParams = decode(params)?;
            // Shape first, so a malformed report is rejected before it
            // can touch ownership.
            agent::validate_report(&p).map_err(|e| HandlerError::invalid_param(e.to_string()))?;
            let (accepted, tab) = h.workspace.agent_report(&p).map_err(ws_err)?;
            encode(&TabAgentReportResult { accepted, tab })
        }
        ops::NOTIFICATION_CREATE => {
            let p: NotificationCreateParams = decode(params)?;
            // One transaction: pending bit + the event the UI turns into
            // a banner and an inbox row. Two separate commits let a
            // concurrent `tab.clear_notification` land between them and
            // leave an inbox row with `has_notification = false`.
            //
            // `Structured` is never gated on agent ownership (plan
            // §3.4); focus may still drop it, which is not an error.
            h.workspace
                .raise_attention(p.tab_id, &p.title, &p.body, AttentionSource::Structured)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::APP_ACTIVATE => {
            // Validate the envelope like every other op (rejects
            // unknown fields) rather than ACK-ing arbitrary payloads.
            let _p: AppActivateParams = decode(params)?;
            // Second-launch window raise (#6). Best-effort: forward to
            // the UI adapter's main thread if wired. A dropped receiver
            // (window gone) or a headless handler is a no-op.
            if let Some(tx) = &h.ui_tx {
                let _ = tx.send(UiRequest::Activate);
            }
            Ok(serde_json::json!({}))
        }
        ops::SCREENSHOT => {
            let p: ScreenshotParams = decode(params)?;
            if !(1..=2).contains(&p.scale) {
                return Err(HandlerError::invalid_param(format!(
                    "scale must be 1 or 2, got {}",
                    p.scale
                )));
            }
            let (png, width, height) = h
                .ui_call(|reply| UiRequest::Screenshot {
                    scale: p.scale,
                    reply,
                })
                .await?
                .map_err(|m| HandlerError::new("internal", m))?;
            // Preflight the 16 MiB IPC frame cap: the response rides one
            // newline-delimited JSON frame, and `png` dominates it once
            // base64-expanded (~4/3). Fail with a structured error here
            // rather than letting the oversized frame fail late during
            // transport (`frame-too-large` on the wire).
            screenshot_frame_guard(png.len())?;
            encode(&ScreenshotResult {
                png,
                width,
                height,
                scale: p.scale,
            })
        }
        ops::WINDOW_METRICS => {
            let _p: WindowMetricsParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::WindowMetrics { reply })
                .await?
                .map_err(|m| HandlerError::new("internal", m))?;
            encode(&result)
        }
        ops::APP_RENDER_STATS => {
            let p: AppRenderStatsParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::AppRenderStats {
                    reset: p.reset,
                    reply,
                })
                .await?
                .map_err(|m| HandlerError::new("internal", m))?;
            encode(&result)
        }
        ops::SIDEBAR_DUMP => {
            let _p: SidebarDumpParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::SidebarDump { reply })
                .await?
                .map_err(|m| HandlerError::new("internal", m))?;
            encode(&result)
        }
        ops::PALETTE_OPEN => {
            let p: PaletteOpenParams = decode(params)?;
            if !matches!(
                p.kind.as_str(),
                "" | "commands" | "launcher" | "custom" | "agents"
            ) {
                return Err(HandlerError::invalid_param(format!(
                    "unknown palette kind {:?} (want \"commands\", \"launcher\", \"custom\", or \"agents\")",
                    p.kind
                )));
            }
            let state = h
                .ui_call(|reply| UiRequest::PaletteOpen {
                    kind: p.kind,
                    reply,
                })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_STATE => {
            // Nullary, but still validate the envelope (reject stray
            // fields) like every other op — matches the Mac handler.
            let _p: PaletteStateParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteState { reply })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_QUERY => {
            let p: PaletteQueryParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteQuery {
                    query: p.query,
                    reply,
                })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_ACTIVATE => {
            let p: PaletteActivateParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteActivate { id: p.id, reply })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_DISMISS => {
            let _p: PaletteDismissParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteDismiss { reply })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_PRESENT => {
            let p: PalettePresentParams = decode(params)?;
            if p.items.is_empty() {
                return Err(HandlerError::invalid_param(
                    "palette.present requires a non-empty items list",
                ));
            }
            let items = p
                .items
                .into_iter()
                .map(|it| (it.id, it.title, it.subtitle))
                .collect::<Vec<_>>();
            let result = h
                .ui_call(|reply| UiRequest::PalettePresent {
                    title: p.title,
                    placeholder: p.placeholder,
                    items,
                    reply,
                })
                .await?
                .map_err(palette_err)?;
            encode(&result)
        }
        ops::SELECTION_SET => {
            let p: SelectionSetParams = decode(params)?;
            h.ui_call(|reply| UiRequest::SelectionSet {
                tab_id: p.tab_id,
                anchor: (p.anchor.col, p.anchor.row),
                cursor: (p.cursor.col, p.cursor.row),
                reply,
            })
            .await?
            .map_err(HandlerError::not_found)?;
            Ok(serde_json::json!({}))
        }
        ops::SELECTION_CLEAR => {
            let p: SelectionClearParams = decode(params)?;
            h.ui_call(|reply| UiRequest::SelectionClear {
                tab_id: p.tab_id,
                reply,
            })
            .await?
            .map_err(HandlerError::not_found)?;
            Ok(serde_json::json!({}))
        }
        ops::SELECTION_DUMP => {
            let p: SelectionDumpParams = decode(params)?;
            let dump = h
                .ui_call(|reply| UiRequest::SelectionDump {
                    tab_id: p.tab_id,
                    reply,
                })
                .await?
                .map_err(HandlerError::not_found)?;
            let result = match dump {
                Some(d) => SelectionDumpResult {
                    text: d.text,
                    anchor_visible: d.anchor_visible,
                    cursor_visible: d.cursor_visible,
                },
                None => SelectionDumpResult::default(),
            };
            encode(&result)
        }
        ops::CLIPBOARD_DUMP => {
            let p: ClipboardDumpParams = decode(params)?;
            let target = parse_clipboard_op(&p.target)?;
            let text = h
                .ui_call(|reply| UiRequest::ClipboardDump { target, reply })
                .await?
                .map_err(|e| HandlerError::new("internal", e))?;
            encode(&ClipboardDumpResult { text })
        }
        ops::CLIPBOARD_WRITE => {
            let p: ClipboardWriteParams = decode(params)?;
            let target = parse_clipboard_op(&p.target)?;
            // Fire-and-forget — matches the `app.activate` pattern.
            // Headless handler / dropped receiver: no-op.
            if let Some(tx) = &h.ui_tx {
                let _ = tx.send(UiRequest::ClipboardWrite {
                    target,
                    text: p.text,
                });
            }
            Ok(serde_json::json!({}))
        }
        ops::TAB_FEED_PTY_BYTES => {
            let p: TabFeedPtyBytesParams = decode(params)?;
            h.ui_call(|reply| UiRequest::TabFeedPtyBytes {
                tab_id: p.tab_id,
                data: p.data,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_CAPTURE_PTY_INPUT => {
            let p: TabCapturePtyInputParams = decode(params)?;
            let data = h
                .ui_call(|reply| UiRequest::TabCapturePtyInput {
                    tab_id: p.tab_id,
                    drain: p.drain,
                    reply,
                })
                .await?
                .map_err(map_test_op_err)?;
            encode(&TabCapturePtyInputResult { data })
        }
        ops::TAB_EXPAND_SELECTION_AT => {
            let p: TabExpandSelectionAtParams = decode(params)?;
            if p.click_count < 2 {
                return Err(HandlerError::new(
                    "invalid-param",
                    format!("click_count must be >= 2 (got {})", p.click_count),
                ));
            }
            let data = h
                .ui_call(|reply| UiRequest::TabExpandSelectionAt {
                    tab_id: p.tab_id,
                    col: p.col,
                    row: p.row,
                    click_count: p.click_count,
                    reply,
                })
                .await?
                .map_err(map_test_op_err)?;
            encode(&TabExpandSelectionAtResult {
                col0: data.col0,
                col1: data.col1,
                text: data.text,
            })
        }
        ops::TAB_FEED_IME => {
            let p: TabFeedImeParams = decode(params)?;
            if !matches!(p.action.as_str(), "preedit" | "commit" | "clear") {
                return Err(HandlerError::invalid_param(format!(
                    "action must be one of preedit/commit/clear (got {:?})",
                    p.action
                )));
            }
            let cursor = match (p.cursor_start, p.cursor_end) {
                (Some(start), Some(end)) => {
                    if start > end {
                        return Err(HandlerError::invalid_param(format!(
                            "cursor_start must be <= cursor_end (got {start}..{end})"
                        )));
                    }
                    Some(start..end)
                }
                (None, None) => None,
                _ => {
                    return Err(HandlerError::invalid_param(
                        "cursor_start and cursor_end must be given together",
                    ));
                }
            };
            h.ui_call(|reply| UiRequest::TabFeedIme {
                tab_id: p.tab_id,
                action: p.action,
                text: p.text,
                cursor,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::WINDOW_RESIZE => {
            let p: WindowResizeParams = decode(params)?;
            if !(p.width.is_finite() && p.height.is_finite() && p.width > 0.0 && p.height > 0.0) {
                return Err(HandlerError::invalid_param(format!(
                    "width and height must be positive and finite (got {} x {})",
                    p.width, p.height
                )));
            }
            h.ui_call(|reply| UiRequest::WindowResize {
                width: p.width,
                height: p.height,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::SIDEBAR_SET_WIDTH => {
            let p: SidebarSetWidthParams = decode(params)?;
            if !(p.width.is_finite() && p.width > 0.0) {
                return Err(HandlerError::invalid_param(format!(
                    "width must be positive and finite (got {})",
                    p.width
                )));
            }
            h.ui_call(|reply| UiRequest::SidebarSetWidth {
                width: p.width,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_DUMP_RESOLVED => {
            let p: TabDumpResolvedParams = decode(params)?;
            let dump = h
                .ui_call(|reply| UiRequest::TabDumpResolved {
                    tab_id: p.tab_id,
                    reply,
                })
                .await?
                .map_err(HandlerError::not_found)?;
            let cells = dump
                .cells
                .into_iter()
                .map(|c| ResolvedCell {
                    row: c.row,
                    col: c.col,
                    text: c.text,
                    fg: rgb_hex(c.fg),
                    bg: rgb_hex(c.bg),
                    has_explicit_bg: c.has_explicit_bg,
                    bold: c.bold,
                    italic: c.italic,
                    inverse: c.inverse,
                })
                .collect();
            encode(&TabDumpResolvedResult {
                cols: dump.cols,
                rows: dump.rows,
                cells,
            })
        }
        ops::TAB_DISPATCH_MOUSE_EVENT => {
            let p: TabDispatchMouseEventParams = decode(params)?;
            let kind = match p.kind.as_str() {
                "press" => crate::pointer::PointerAction::Press,
                "release" => crate::pointer::PointerAction::Release,
                "motion" => crate::pointer::PointerAction::Motion,
                other => {
                    return Err(HandlerError::invalid_param(format!(
                        "kind must be one of press|release|motion (got {other})"
                    )));
                }
            };
            let button = match p.button.as_str() {
                "left" => Some(crate::pointer::PointerButton::Left),
                "right" => Some(crate::pointer::PointerButton::Right),
                "middle" => Some(crate::pointer::PointerButton::Middle),
                "wheel_up" => Some(crate::pointer::PointerButton::Four),
                "wheel_down" => Some(crate::pointer::PointerButton::Five),
                "none" => None,
                other => {
                    return Err(HandlerError::invalid_param(format!(
                        "button must be one of left|right|middle|wheel_up|wheel_down|none (got {other})"
                    )));
                }
            };
            h.ui_call(|reply| UiRequest::TabDispatchMouseEvent {
                tab_id: p.tab_id,
                kind,
                button,
                cell_x: p.cell_x,
                cell_y: p.cell_y,
                mods: p.mods,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::APP_SET_WINDOW_FOCUS => {
            let p: AppSetWindowFocusParams = decode(params)?;
            h.ui_call(|reply| UiRequest::AppSetWindowFocus {
                focused: p.focus,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::APP_CURSOR_SHAPE => {
            let _: AppCursorShapeParams = decode(params)?;
            let shape = h
                .ui_call(|reply| UiRequest::AppCursorShape { reply })
                .await?
                .map_err(|e| HandlerError::new("internal", e))?;
            encode(&AppCursorShapeResult { shape })
        }
        ops::APP_ACTIVE_TERMINAL_FOCUSED => {
            let _: AppActiveTerminalFocusedParams = decode(params)?;
            let focused = h
                .ui_call(|reply| UiRequest::AppActiveTerminalFocused { reply })
                .await?
                .map_err(|e| HandlerError::new("internal", e))?;
            encode(&AppActiveTerminalFocusedResult { focused })
        }
        ops::APP_SELECTED_TAB_ID => {
            let _: AppSelectedTabIdParams = decode(params)?;
            let tab_id = h
                .ui_call(|reply| UiRequest::AppSelectedTabId { reply })
                .await?
                .map_err(|e| HandlerError::new("internal", e))?;
            encode(&AppSelectedTabIdResult { tab_id })
        }
        ops::APP_DOCK_BADGE => {
            let _: AppDockBadgeParams = decode(params)?;
            let label = h
                .ui_call(|reply| UiRequest::AppDockBadge { reply })
                .await?
                .map_err(map_test_op_err)?;
            encode(&AppDockBadgeResult { label })
        }
        ops::APP_MENU_DUMP => {
            let _: AppMenuDumpParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::AppMenuDump { reply })
                .await?
                .map_err(map_test_op_err)?;
            encode(&result)
        }
        ops::APP_MENU_ACTIVATE => {
            let p: AppMenuActivateParams = decode(params)?;
            h.ui_call(|reply| UiRequest::AppMenuActivate {
                path: p.path,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::APP_UPDATE_STATUS => {
            let _: AppUpdateStatusParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::AppUpdateStatus { reply })
                .await?
                .map_err(map_test_op_err)?;
            encode(&result)
        }
        ops::APP_UPDATE_CHECK => {
            let _: AppUpdateCheckParams = decode(params)?;
            h.ui_call(|reply| UiRequest::AppUpdateCheck { reply })
                .await?
                .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::APP_NOTIFICATION_STATUS => {
            let _: AppNotificationStatusParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::AppNotificationStatus { reply })
                .await?
                .map_err(map_test_op_err)?;
            encode(&result)
        }
        ops::EVENTS_SUBSCRIBE => {
            // Only reachable on a UI socket: a session socket handles
            // this op in `dispatch_outcome`, above the dispatcher.
            //
            // Honest failure rather than a false ACK: a UI process
            // pushes nothing on the connection, so a client that
            // "subscribed" would wait forever. Surface not-implemented
            // so it can fall back (e.g. poll `tab.list`). A UI-side
            // stream lands with its first consumer — the planned
            // `roostctl watch` (#9).
            Err(HandlerError::new(
                "not-implemented",
                "events.subscribe is not yet implemented",
            ))
        }
        other => Err(HandlerError::unknown_op(other)),
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, HandlerError> {
    serde_json::from_value(value).map_err(|e| {
        // Drop the field key out of the error message for users;
        // `serde_json::Error::Display` already includes a useful
        // "missing field `foo` at line ..." form.
        let msg = e.to_string();
        if msg.contains("unknown field") {
            HandlerError::new("unknown-field", msg)
        } else if msg.contains("missing field") {
            HandlerError::new("missing-param", msg)
        } else {
            HandlerError::invalid_param(msg)
        }
    })
}

fn encode<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, HandlerError> {
    serde_json::to_value(value).map_err(|e| HandlerError::new("internal", e.to_string()))
}

/// Format an (r,g,b) triple as `#RRGGBB` for the
/// `tab.dump_resolved` wire format. Kept human-readable so test
/// assertions can match on the literal string.
fn rgb_hex(c: (u8, u8, u8)) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// Map an error from a gated test-mode op back to a wire-friendly
/// [`HandlerError`]. Failure modes the UI distinguishes by message
/// text:
///   * env var missing → `not-enabled`
///   * unknown tab id, or `tab.expand_selection_at` falling through
///     (whitespace double-click → no span) → `not-found`. The Mac
///     handler returns `not-found` for the no-span case too — the
///     `no word/line span` substring keeps both UIs symmetric.
///   * `tab.feed_ime`'s `tab_id` not matching the tab that currently
///     holds the keyboard route → `invalid-param` — the caller asked
///     to feed the wrong tab, not a server failure.
///   * `app.menu_activate`'s path resolution failing (unknown path,
///     ambiguous title, or a disabled item) → `invalid-param` — the
///     caller asked for a path the live menu bar doesn't support.
///   * an op a UI hasn't wired up yet (`tab.feed_ime` off Mac, still
///     iced-only), or one that is structurally unavailable there
///     (`app.dock_badge` off macOS — there is no Dock) →
///     `not-implemented`, mirroring `events.subscribe`.
///   * anything else (capture buffer poisoned, feed channel closed,
///     the native menu bar not installed yet) → `internal`, so a real
///     failure surfaces clearly rather than being mistaken for a
///     missing tab.
///
/// The substring contract is the simplest seam between the UI and
/// the dispatcher while the surface stays small; bumping to a typed
/// error is the right move when the arms keep growing.
fn map_test_op_err(err: String) -> HandlerError {
    if err.contains("ROOST_TEST_MODE") {
        HandlerError::new("not-enabled", err)
    } else if err.contains("has no live terminal") || err.contains("no word/line span") {
        HandlerError::not_found(err)
    } else if err.contains("is not the active terminal")
        || err.contains("no menu item")
        || err.contains("ambiguous menu")
        || err.contains("is disabled")
        || err.contains("has no submenu to descend into")
        || err.contains("must not be empty")
    {
        HandlerError::invalid_param(err)
    } else if err.contains("not supported on this UI") {
        HandlerError::new("not-implemented", err)
    } else {
        HandlerError::new("internal", err)
    }
}

/// Reject a screenshot whose base64-encoded PNG would overflow the IPC
/// frame cap. base64 expands by 4/3 (`ceil(n/3)*4`); a small margin
/// covers the JSON envelope (`id` / `ok` / `result` / dims).
fn screenshot_frame_guard(png_len: usize) -> Result<(), HandlerError> {
    const ENVELOPE_MARGIN: usize = 1024;
    let encoded = png_len.div_ceil(3) * 4;
    if encoded + ENVELOPE_MARGIN > roost_ipc::MAX_FRAME_BYTES {
        return Err(HandlerError::new(
            "internal",
            format!(
                "screenshot too large: {encoded} base64 bytes exceeds the {} byte IPC frame cap (try --scale 1)",
                roost_ipc::MAX_FRAME_BYTES
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)] // `Result::map_err` adapter owns its error.
fn ws_err(e: WorkspaceError) -> HandlerError {
    match e {
        WorkspaceError::ProjectNotFound(_) | WorkspaceError::TabNotFound(_) => {
            HandlerError::not_found(e.to_string())
        }
        WorkspaceError::TabProjectMismatch { .. } => HandlerError::invalid_param(e.to_string()),
        WorkspaceError::Io(_) | WorkspaceError::Json(_) => {
            HandlerError::new("internal", e.to_string())
        }
    }
}

#[allow(clippy::needless_pass_by_value)] // `Result::map_err` adapter owns its error.
fn pty_err(e: PtyError) -> HandlerError {
    match e {
        PtyError::NotFound(_) | PtyError::Closed(_) | PtyError::Cancelled(_) => {
            HandlerError::not_found(e.to_string())
        }
        PtyError::DuplicateTab(_) => HandlerError::invalid_param(e.to_string()),
        PtyError::ShuttingDown(_) => HandlerError::new("shutting-down", e.to_string()),
    }
}

/// Map a `palette.activate` failure to the wire. Both cases — no palette
/// open, or no visible row with the requested id — are "act on something
/// that isn't there", i.e. `not-found`.
fn palette_err(msg: String) -> HandlerError {
    HandlerError::not_found(msg)
}

/// Map the wire-format `target` string (`"system"` / `"selection"`) to
/// the typed `ClipboardOp` the UI drain consumes. Unknown values are
/// `invalid-param` so a typo doesn't silently fall through to the
/// system clipboard.
fn parse_clipboard_op(s: &str) -> Result<ClipboardOp, HandlerError> {
    match s {
        "system" => Ok(ClipboardOp::System),
        "selection" => Ok(ClipboardOp::Selection),
        other => Err(HandlerError::invalid_param(format!(
            "clipboard target must be \"system\" or \"selection\" (got {other:?})"
        ))),
    }
}

/// The push registry's two operations. Both are `SessionState`
/// internals, so they are exercised here rather than through the
/// socket: what a wire test can see is only the *effect* of a stop, not
/// whether a hung-up subscriber's entry was ever cleaned up.
#[cfg(test)]
mod tests {
    use super::*;

    fn session_state() -> SessionState {
        SessionState {
            info: SessionInfo {
                session_id: "test".into(),
                started_at: "2026-08-27T14:03:11Z".into(),
                app_version: "9.9.9".into(),
                payload_kinds: Vec::new(),
                libghostty_build: String::new(),
                default_tab_size: (120, 40),
            },
            stop: StopHandle::new(|| async {}),
            stopping: AtomicBool::new(false),
            barrier: tokio::sync::RwLock::new(()),
            pushes: std::sync::Mutex::new(Some(Vec::new())),
        }
    }

    fn live_pushes(state: &SessionState) -> usize {
        lock(&state.pushes).as_ref().map_or(0, Vec::len)
    }

    /// A relay that ended on its own — the normal close — must not stay
    /// in the registry. Every subscribe/disconnect cycle would otherwise
    /// add one entry that nothing ever removes.
    #[tokio::test]
    async fn a_finished_relay_is_pruned_on_the_next_register() {
        let state = session_state();

        let finished = tokio::spawn(async {});
        let stale = finished.abort_handle();
        finished.await.expect("the task completes");
        assert!(state.register_push(stale));

        let parked = tokio::spawn(std::future::pending::<()>());
        assert!(state.register_push(parked.abort_handle()));
        assert_eq!(
            live_pushes(&state),
            1,
            "the finished relay must be swept, leaving only the live one"
        );
        parked.abort();
    }

    /// The stop sweep ends every live relay and closes the registry, so
    /// a subscribe that raced it cannot register into a list nobody will
    /// read again.
    #[tokio::test]
    async fn the_stop_sweep_aborts_live_relays_and_then_refuses() {
        let state = session_state();
        let parked = tokio::spawn(std::future::pending::<()>());
        assert!(state.register_push(parked.abort_handle()));

        state.abort_pushes();
        assert!(
            parked.await.expect_err("aborted").is_cancelled(),
            "the sweep must actually end the relay"
        );

        let late = tokio::spawn(std::future::pending::<()>());
        assert!(
            !state.register_push(late.abort_handle()),
            "a subscribe after the sweep must be refused"
        );
        late.abort();
    }
}
