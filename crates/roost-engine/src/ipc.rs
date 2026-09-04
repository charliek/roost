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
    AppCursorShapeParams, AppCursorShapeResult, AppDialogAnswerParams, AppDialogDumpParams,
    AppDialogDumpResult, AppDockBadgeParams, AppDockBadgeResult, AppKeybindDispatchParams,
    AppMenuActivateParams, AppMenuDumpParams, AppMenuDumpResult, AppNotificationStatusParams,
    AppNotificationStatusResult, AppRenderStatsParams, AppRenderStatsResult,
    AppSelectedTabIdParams, AppSelectedTabIdResult, AppSetWindowFocusParams, AppUpdateCheckParams,
    AppUpdateStatusParams, AppUpdateStatusResult, AttachPayloadKind, ClipboardDumpParams,
    ClipboardDumpResult, ClipboardWriteParams, EventsSubscribeParams, EventsSubscribeResult, Host,
    HostAddParams, HostAddResult, HostConnectParams, HostConnectionResult, HostDisconnectParams,
    HostListParams, HostListResult, HostRemoveParams, HostStatusParams, HostStatusResult,
    IdentifyParams, IdentifyResult, NotificationCreateParams, PaletteActivateParams,
    PaletteDismissParams, PaletteOpenParams, PalettePresentParams, PalettePresentResult,
    PaletteQueryParams, PaletteStateParams, PaletteStateResult, ProjectCreateParams,
    ProjectCreateResult, ProjectDeleteParams, ProjectRenameParams, ProjectReorderParams,
    ResolvedCell, ScreenshotParams, ScreenshotResult, SelectionClearParams, SelectionDumpParams,
    SelectionDumpResult, SelectionSetParams, SessionConnectParams, SessionConnectResult,
    SessionIdentify, SessionIdentifyParams, SessionSetFocusParams, SessionSetThemeParams,
    SessionStopParams, SessionStopResult, SidebarDumpParams, SidebarDumpResult,
    SidebarSetWidthParams, TabAgentReportResult, TabAttachParams, TabCapturePtyInputParams,
    TabCapturePtyInputResult, TabClearNotificationParams, TabCloseParams,
    TabDispatchMouseEventParams, TabDumpCursor, TabDumpParams, TabDumpResolvedParams,
    TabDumpResolvedResult, TabDumpResult, TabExpandSelectionAtParams, TabExpandSelectionAtResult,
    TabFeedImeParams, TabFeedPtyBytesParams, TabFocusParams, TabFocusResult, TabListResult,
    TabOpenParams, TabOpenResult, TabReorderParams, TabResizeParams, TabSetHookActiveParams,
    TabSetStateParams, TabSetTitleParams, TabWriteParams, WindowMetricsParams, WindowMetricsResult,
    WindowResizeParams, WireProjectRef, WireTabRef, SESSION_PROTOCOL_VERSION,
};
#[cfg(feature = "server-vt")]
use roost_ipc::messages::{SessionSetThemeResult, TabAttachResult};
use roost_ipc::{
    CloseReason, ConnAction, ConnCloser, ConnCtx, Handler, HandlerError, HandlerOutcome,
    StopFinalizer,
};

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

/// Reply for the `host.*` [`UiRequest`]s (plan 037 §3.5).
///
/// The error half is the registry's own [`WorkspaceError`] rather than a
/// message, which is what lets `ws_err` mint the same wire code the
/// engine-served path does: a reserved label is `invalid-param` and an
/// unsaved id is `not-found`, whether the op was answered by the app or
/// by a headless workspace. Stringifying at the seam would flatten both
/// onto one code.
pub type HostReply<T> = tokio::sync::oneshot::Sender<Result<T, WorkspaceError>>;

/// Why a host-routed op did not happen, as the wire will say it.
///
/// The sibling of [`HostReply`]'s [`WorkspaceError`], for the ops the
/// app forwards to a *session* rather than answering from its own
/// registry (the host form of `tab.reorder` / `project.reorder`, plan
/// 044 §3.1 d6). Their failure is the session's own refusal, which
/// already carries a wire code; a `WorkspaceError` could not hold it
/// and a bare string would flatten it onto `internal`. The app maps its
/// `HostOpError` here — the session's code verbatim, or
/// `host-unavailable` for a connection that is not there — and the
/// dispatcher turns it back into a [`HandlerError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOpFailure {
    pub code: String,
    pub message: String,
}

impl HostOpFailure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl From<HostOpFailure> for HandlerError {
    fn from(failure: HostOpFailure) -> Self {
        HandlerError::new(failure.code, failure.message)
    }
}

/// Reply for a host-routed op — see [`HostOpFailure`].
pub type HostOpReply<T> = tokio::sync::oneshot::Sender<Result<T, HostOpFailure>>;

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
    /// Read a tab's terminal viewport as text. `tab_id` is the wire
    /// form: bare = a local tab, host-qualified = an attached host
    /// tab's client-side terminal — the UI resolves both against its
    /// keyed map (plan 037 §3.4).
    Dump {
        tab_id: WireTabRef,
        reply: DumpReply,
    },
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
        tab_id: WireTabRef,
        drain: bool,
        reply: CapturedBytesReply,
    },
    /// `tab.dump_resolved` — return every cell on a tab's terminal
    /// viewport after the production color resolver has run. Ungated
    /// (no shadow state — same walk the real paint loop runs).
    TabDumpResolved {
        tab_id: WireTabRef,
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
    /// `app.dialog_dump` — read which host modal is on screen and the
    /// strings it is showing. Gated like `TabFeedPtyBytes`; a test seam
    /// for the ssh bootstrap's consent card, never a surface.
    AppDialogDump {
        reply: tokio::sync::oneshot::Sender<Result<AppDialogDumpResult, String>>,
    },
    /// `app.dialog_answer` — confirm or cancel the visible host modal,
    /// through the same routes a click and Enter/Escape take. `action`
    /// is `"confirm" | "cancel"`. Gated like `AppDialogDump`.
    AppDialogAnswer {
        action: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// `app.keybind_dispatch` — run the paste accelerator through the
    /// production dispatcher, the same one a real key event or native
    /// menu click reaches. Gated like `AppDialogDump`; exists because
    /// paste (issue #376) has no other IPC seam. `action` must be
    /// `"paste"` — every other `KeybindAction` spelling is refused (see
    /// `AppKeybindDispatchParams`'s doc comment in `roost-ipc`).
    AppKeybindDispatch {
        action: String,
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
    /// `host.add` — save a host to the client-side registry (plan 037
    /// §3.5).
    ///
    /// The registry is a plain `Workspace` accessor, so the engine could
    /// serve this itself — and does, headless. It routes through the app
    /// when there is one because saving a host is only half the story:
    /// the sidebar grows a section, the launch probe may start dialing,
    /// and neither happens off a mutation nothing re-reads. A
    /// `roostctl host add` against an idle UI has to be as visible as
    /// the Add Host dialog's own save.
    HostAdd {
        label: String,
        target: String,
        reply: HostReply<Host>,
    },
    /// `host.remove` — forget a saved host. Disconnects it first if it
    /// is connected; never stops the session (roadmap D8).
    HostRemove { id: String, reply: HostReply<()> },
    /// `tab.focus` for a host-qualified ref: select that host's tab and
    /// attach it, exactly as a sidebar click does. The local form never
    /// reaches here — it mutates the workspace in the handler, headless
    /// or not — so this arm exists only because a host selection is
    /// app-owned state (plan 037 §3.4).
    HostTabFocus {
        host: u32,
        tab_id: i64,
        reply: HostReply<()>,
    },
    /// `tab.reorder` for a host-qualified project: send that host's
    /// session the whole new tab order over its op queue (plan 044
    /// §3.1 d6). The ids are already narrowed to the session's own bare
    /// id-space — the incarnation is the `host` field.
    ///
    /// Unlike every other `host.*` arm, this one cannot be answered
    /// inside `update`: the app has to await the session's reply. It
    /// moves this `reply` into that future and answers from there.
    HostTabReorder {
        host: u32,
        project_id: i64,
        tab_ids: Vec<i64>,
        reply: HostOpReply<()>,
    },
    /// `project.reorder`'s twin of [`UiRequest::HostTabReorder`].
    HostProjectReorder {
        host: u32,
        project_ids: Vec<i64>,
        reply: HostOpReply<()>,
    },
    /// `host.connect` — the palette's `Connect Host` and the sidebar's
    /// ↻ Reconnect, as an op. Unconditional takeover, and it may start a
    /// localhost session that is not running.
    HostConnect {
        id: String,
        /// `HostConnectParams::test_user_origin`, carried through
        /// verbatim — see that field's doc for what it is and why.
        test_user_origin: bool,
        reply: HostReply<HostConnectionResult>,
    },
    /// `host.disconnect` — drop the connection, leave the session
    /// running.
    HostDisconnect {
        id: String,
        reply: HostReply<HostConnectionResult>,
    },
    /// `host.status` — every saved host's connection state as the
    /// sidebar's band has it, or just the one named. A read, but an
    /// app-side one: the connection set is the app's alone.
    HostStatus {
        id: Option<String>,
        reply: HostReply<HostStatusResult>,
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
use crate::persistence::HostSnapshot;
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
    /// when the daemon runs without the server-VT pipeline — an honest
    /// "attach unavailable" rather than a promise it cannot keep.
    pub payload_kinds: Vec<AttachPayloadKind>,
    /// Pinned libghostty build identity, which must match exactly for
    /// [`AttachPayloadKind::GHOSTTY_SNAPSHOT`] to be negotiable. Empty
    /// for the same reason as `payload_kinds`.
    pub libghostty_build: String,
    /// `(cols, rows)` a `tab.open` that omits both falls back to. A
    /// headless session has no window to measure, so the daemon states
    /// the size rather than inheriting a UI's 80×24.
    pub default_tab_size: (u16, u16),
    /// Whether the daemon was launched with `ROOST_TEST_MODE=1`.
    ///
    /// Passed in rather than read from the environment here: the engine
    /// is also linked into UI processes, and a test-mode decision that
    /// depends on which process happens to be asking is one nobody can
    /// reason about. Gates the same ops a UI gates
    /// (`tab.feed_pty_bytes`, `tab.capture_pty_input`) plus the attach
    /// token's TTL override.
    pub test_mode: bool,
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
    /// Who currently holds interactive authority, and which connections
    /// they hold it on. The single linearization point for the whole
    /// admission story: connect, takeover, and every lease-gated op
    /// resolve against this one lock, so two clients racing a takeover
    /// produce one winner rather than two live leases.
    clients: std::sync::Mutex<ClientRegistry>,
}

/// How long an attach token minted by `tab.attach` stays usable.
///
/// A protocol constant, not a test wait: it is not scaled by
/// `ROOST_TEST_TIMEOUT_SCALE`, because what it bounds is how long a
/// credential a client already holds stays valid, not how long anything
/// waits. A session in test mode may shorten it (see
/// [`ATTACH_TTL_OVERRIDE_ENV`]) so the expiry case is testable in
/// seconds.
pub const ATTACH_TOKEN_TTL: Duration = Duration::from_secs(60);

/// Shortens [`ATTACH_TOKEN_TTL`], in milliseconds. Honored **only** when
/// the session was started with `ROOST_TEST_MODE=1`; a production daemon
/// ignores it entirely.
pub const ATTACH_TTL_OVERRIDE_ENV: &str = "ROOST_SESSION_ATTACH_TTL_MS";

/// How many minted-but-undialed attach tokens one session will hold.
///
/// The quota is what bounds the registry. Reaching it means a client
/// minted 16 tokens inside one TTL and dialed none of them — every
/// healthy attach consumes its token within a round trip — so the
/// answer is to refuse rather than to evict a token some other
/// connection is about to present.
pub const MAX_OUTSTANDING_TOKENS: usize = 16;

/// The lease registry. Bounded by construction: one live lease, one
/// tombstone, one entry per live connection under the lease, at most
/// [`MAX_OUTSTANDING_TOKENS`] unconsumed tokens, and one live data
/// connection per tab.
#[derive(Default)]
struct ClientRegistry {
    current: Option<Lease>,
    /// The most recently invalidated lease token, kept only so its
    /// holder gets `taken-over` instead of `connect-required` — a
    /// materially different instruction (stop retrying vs. reconnect).
    /// Exactly one: an older tombstone is a client that has already been
    /// told twice over.
    tombstone: Option<String>,
    /// Attach tickets handed out but not yet presented on a data
    /// connection.
    tokens: Vec<AttachToken>,
    /// The live data connection per tab id, so a second admitted
    /// handshake for the same tab can supersede the first rather than
    /// leaving two forwarders racing one tee.
    data_conns: std::collections::HashMap<i64, (u64, ConnCloser)>,
    /// Which connection's `session.set_focus` the workspace is currently
    /// holding, if any.
    ///
    /// A focus is a statement about a window, and it is only true while
    /// the connection that made it is still there. Tracking the *author*
    /// rather than only counting live connections is what makes the
    /// reset independent of the order two closes and a new registration
    /// happen to be noticed in — a client that re-dials on the same
    /// lease must not accidentally keep the departed one's focus alive.
    focus_conn: Option<u64>,
}

/// One single-use attach ticket. Bound to the lease that minted it and
/// to the exact tab pipeline it describes, so a takeover or a respawn
/// between `tab.attach` and the handshake cannot be papered over.
///
/// Read only by the `server-vt` attach path; a default build (a UI
/// binary built without `roost-session` in its graph) compiles the
/// registry but never consumes tickets, hence the gated allow.
#[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
struct AttachToken {
    token: String,
    lease: String,
    tab_id: i64,
    tab_generation: u64,
    expires_at: std::time::Instant,
}

/// What consuming a token admitted, handed to the forwarder.
#[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdmittedAttach {
    pub(crate) tab_id: i64,
    pub(crate) tab_generation: u64,
}

struct Lease {
    token: String,
    conns: Vec<(u64, ConnCloser)>,
}

/// What a presented lease turns out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseStatus {
    /// The live lease. The presenting connection is now registered under
    /// it.
    Current,
    /// The tombstone: this client held the lease and lost it.
    TakenOver,
    /// Absent, empty, or a token this session never issued.
    Unknown,
}

impl ClientRegistry {
    /// Mint a lease for `ctx`, or refuse.
    ///
    /// `takeover` is what makes this destructive: the previous lease is
    /// invalidated and every connection it was held on is closed —
    /// except the requester's, which is the one being answered on.
    fn connect(&mut self, takeover: bool, ctx: &ConnCtx) -> Result<String, HandlerError> {
        if self.current.is_some() && !takeover {
            // Refused even when the caller already holds the lease on
            // this very connection: a client that lost track of its own
            // lease is exactly the one that must re-establish it
            // deliberately.
            return Err(HandlerError::new(
                "already-connected",
                "another client holds the session lease; retry with takeover: true",
            ));
        }
        if let Some(previous) = self.current.take() {
            for (conn_id, closer) in previous.conns {
                if conn_id != ctx.conn_id {
                    closer.close(CloseReason::TakenOver);
                }
            }
            // Tickets minted under the displaced lease die with it —
            // `admit_attach` would refuse them at the lease re-check
            // anyway. Purged in the SAME critical section as the
            // takeover, because leaving them would let a dead client's
            // 16 outstanding tokens hold the whole quota against the new
            // holder for a full TTL.
            self.tokens.retain(|token| token.lease != previous.token);
            self.tombstone = Some(previous.token);
        }
        // The displaced client's focus dies with its authority; the
        // caller drops the workspace flag to match.
        self.focus_conn = None;
        let token = random_hex_128();
        self.current = Some(Lease {
            token: token.clone(),
            conns: vec![(ctx.conn_id, ctx.closer.clone())],
        });
        Ok(token)
    }

    /// Resolve a presented lease, registering the presenting connection
    /// when it is the live one.
    fn present(&mut self, lease: &str, ctx: &ConnCtx) -> LeaseStatus {
        if let Some(current) = self.current.as_mut() {
            if !lease.is_empty() && current.token == lease {
                // Pruned here, as in `forget_connection`: a client that
                // reconnects repeatedly on the same lease would
                // otherwise accumulate an entry per dead connection.
                current.conns.retain(|(_, closer)| !closer.is_closed());
                if !current.conns.iter().any(|(id, _)| *id == ctx.conn_id) {
                    current.conns.push((ctx.conn_id, ctx.closer.clone()));
                }
                return LeaseStatus::Current;
            }
        }
        if !lease.is_empty() && self.tombstone.as_deref() == Some(lease) {
            return LeaseStatus::TakenOver;
        }
        LeaseStatus::Unknown
    }

    /// Record who the workspace's current focus belongs to. `false`
    /// clears it: a null focus is nobody's claim to lose.
    fn claim_focus(&mut self, conn_id: u64, focused: bool) {
        self.focus_conn = focused.then_some(conn_id);
    }

    /// Drop one connection from the live lease, reporting whether the
    /// focus the workspace holds went with it.
    ///
    /// Either edge counts: the connection that *stated* the focus is
    /// gone, or the holder has no connections left at all. The first is
    /// what makes this independent of ordering; the second covers a
    /// client that stated a focus and then went away on some other
    /// connection.
    ///
    /// Closed peers are pruned on the way through, like [`Self::present`]
    /// does: a client that dropped two connections at once must not
    /// leave the second one standing in for a holder that is gone.
    fn forget_connection(&mut self, conn_id: u64) -> bool {
        let Some(current) = self.current.as_mut() else {
            return false;
        };
        let held = !current.conns.is_empty();
        current
            .conns
            .retain(|(id, closer)| *id != conn_id && !closer.is_closed());
        let lost = self.focus_conn == Some(conn_id) || (held && current.conns.is_empty());
        if lost {
            self.focus_conn = None;
        }
        lost
    }

    /// Close every registered connection, and stop tracking them. The
    /// lease itself stays: nothing after a stop is admissible anyway, and
    /// keeping it means a late op is refused as `shutting-down` rather
    /// than as a lease problem it cannot fix.
    fn close_all(&mut self, reason: CloseReason) {
        if let Some(current) = self.current.as_mut() {
            for (_, closer) in current.conns.drain(..) {
                closer.close(reason);
            }
        }
        // Their closers were in the list above, so this only drops the
        // per-tab index — but leaving it would keep an entry alive per
        // tab that ever attached.
        self.data_conns.clear();
        // The tokens deliberately stay. `admit_attach`'s stop latch is
        // what refuses them, and it can only say `shutting-down` about a
        // ticket it can still recognize; dropping them here would send a
        // client that holds a perfectly good pre-stop token hunting for
        // a bad credential instead. They are bounded at
        // [`MAX_OUTSTANDING_TOKENS`] and the process is on its way out.
    }

    /// Mint a single-use ticket for one data connection.
    ///
    /// The ticket methods are consumed only by the `server-vt` attach
    /// path; a default build compiles the registry without them being
    /// reachable, hence the gated allows here and on the two below.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn mint_token(
        &mut self,
        lease: &str,
        tab_id: i64,
        tab_generation: u64,
        ttl: Duration,
    ) -> Result<String, HandlerError> {
        let now = std::time::Instant::now();
        self.tokens.retain(|t| t.expires_at > now);
        // The lease is re-checked here because minting and the
        // `require_lease` that preceded it are two acquisitions of this
        // lock; a takeover in between must not leave a ticket behind
        // that outlives the authority it was issued under.
        match self.current.as_ref() {
            Some(current) if !lease.is_empty() && current.token == lease => {}
            _ => {
                return Err(HandlerError::new(
                    "taken-over",
                    "this lease was taken over by another client",
                ))
            }
        }
        if self.tokens.len() >= MAX_OUTSTANDING_TOKENS {
            return Err(HandlerError::new(
                "too-many-tokens",
                format!(
                    "{MAX_OUTSTANDING_TOKENS} attach tokens are already outstanding; \
                     dial the data connections you asked for"
                ),
            ));
        }
        let token = random_hex_128();
        self.tokens.push(AttachToken {
            token: token.clone(),
            lease: lease.to_string(),
            tab_id,
            tab_generation,
            expires_at: now + ttl,
        });
        Ok(token)
    }

    /// Consume a token and register `ctx` as the tab's data connection.
    ///
    /// The whole admission is one step under one lock — consume, lease
    /// re-check, stop latch, register, supersede — so two connections
    /// presenting the same token produce exactly one forwarder, and a
    /// takeover either wholly precedes this or wholly follows it.
    ///
    /// The order of the three refusals is the contract, not an accident:
    /// each names a different thing for the client to fix, so a token
    /// this session never issued must answer `invalid-token` even during
    /// a stop — telling such a client `shutting-down` would send it
    /// reconnecting with a credential that was never going to work.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn admit_attach(
        &mut self,
        token: &str,
        ctx: &ConnCtx,
        stopping: bool,
    ) -> Result<(AdmittedAttach, Option<ConnCloser>), HandlerError> {
        let now = std::time::Instant::now();
        self.tokens.retain(|t| t.expires_at > now);
        let Some(index) = self.tokens.iter().position(|t| t.token == token) else {
            return Err(HandlerError::new(
                "invalid-token",
                "unknown, expired, revoked, or already-used attach token",
            ));
        };
        let ticket = self.tokens.remove(index);
        if !self
            .current
            .as_ref()
            .is_some_and(|current| current.token == ticket.lease)
        {
            return Err(HandlerError::new(
                "taken-over",
                "the lease this attach token was minted under is no longer current",
            ));
        }
        // Checked only once the ticket is known good, and still under
        // this lock: the stop latches first and sweeps this registry
        // second, so a data connection admitted past the latch but
        // registered after the sweep would be one no closer can reach.
        if stopping {
            return Err(shutting_down());
        }
        let live = self
            .current
            .as_mut()
            .expect("the lease was just confirmed current under this lock");
        live.conns.retain(|(_, closer)| !closer.is_closed());
        if !live.conns.iter().any(|(id, _)| *id == ctx.conn_id) {
            live.conns.push((ctx.conn_id, ctx.closer.clone()));
        }
        let displaced = self
            .data_conns
            .insert(ticket.tab_id, (ctx.conn_id, ctx.closer.clone()))
            .filter(|(conn_id, _)| *conn_id != ctx.conn_id)
            .map(|(_, closer)| closer);
        Ok((
            AdmittedAttach {
                tab_id: ticket.tab_id,
                tab_generation: ticket.tab_generation,
            },
            displaced,
        ))
    }

    /// Drop a tab's data-connection entry, but only if it is still the
    /// one this connection registered — a superseded forwarder unwinding
    /// after its replacement registered must not evict it.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn release_data_conn(&mut self, tab_id: i64, conn_id: u64) {
        if self
            .data_conns
            .get(&tab_id)
            .is_some_and(|(id, _)| *id == conn_id)
        {
            self.data_conns.remove(&tab_id);
        }
    }
}

/// 128 bits of OS entropy as 32 lowercase hex characters — the shape
/// every bearer credential on a session socket takes.
///
/// Unlike the session id this one *is* a credential, so the width is the
/// point: the socket's uid check bounds who can guess at it at all, and
/// 128 bits ends the question.
fn random_hex_128() -> String {
    crate::workspace::random_hex(16)
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

    /// Mint or take over the interactive lease for `ctx`'s connection.
    fn connect(&self, takeover: bool, ctx: &ConnCtx) -> Result<String, HandlerError> {
        let mut guard = lock(&self.clients);
        // Re-checked UNDER the registry lock: the stop latches first and
        // sweeps this registry second, so a connect that was admitted
        // past the latch but reaches the registry after the sweep must
        // be refused here — a lease minted post-sweep would be authority
        // no closer can ever revoke.
        if self.stopping.load(Ordering::Acquire) {
            return Err(shutting_down());
        }
        guard.connect(takeover, ctx)
    }

    /// The gate every lease-carrying op runs first. Registers `ctx` under
    /// the lease on success; the error never echoes the presented token.
    fn require_lease(&self, lease: &str, ctx: &ConnCtx) -> Result<(), HandlerError> {
        let mut guard = lock(&self.clients);
        // Same post-sweep refusal as `connect` — registration IS the
        // resource, so the decision has to share the sweep's lock.
        if self.stopping.load(Ordering::Acquire) {
            return Err(shutting_down());
        }
        match guard.present(lease, ctx) {
            LeaseStatus::Current => Ok(()),
            LeaseStatus::TakenOver => Err(HandlerError::new(
                "taken-over",
                "this lease was taken over by another client",
            )),
            LeaseStatus::Unknown => Err(HandlerError::new(
                "connect-required",
                "run session.connect first: this op requires a session lease",
            )),
        }
    }

    /// One connection under the lease has ended. `true` when the focus
    /// the workspace is holding went away with it.
    fn forget_connection(&self, conn_id: u64) -> bool {
        lock(&self.clients).forget_connection(conn_id)
    }

    /// Remember which connection the workspace's focus came from, so its
    /// close can retire it.
    fn claim_focus(&self, ctx: &ConnCtx, focused: bool) {
        lock(&self.clients).claim_focus(ctx.conn_id, focused);
    }

    /// Tell every connection the lease holder owns why it is going away.
    fn close_clients(&self, reason: CloseReason) {
        lock(&self.clients).close_all(reason);
    }

    /// Mint one attach ticket. The caller has already passed the lease
    /// gate and the stop latch; both are re-checked under this lock,
    /// because a ticket is authority and authority minted after a sweep
    /// is authority nobody can revoke.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn mint_attach_token(
        &self,
        lease: &str,
        tab_id: i64,
        tab_generation: u64,
    ) -> Result<String, HandlerError> {
        let mut guard = lock(&self.clients);
        if self.stopping.load(Ordering::Acquire) {
            return Err(shutting_down());
        }
        guard.mint_token(lease, tab_id, tab_generation, self.attach_token_ttl())
    }

    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn attach_token_ttl(&self) -> Duration {
        if !self.info.test_mode {
            return ATTACH_TOKEN_TTL;
        }
        std::env::var(ATTACH_TTL_OVERRIDE_ENV)
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map_or(ATTACH_TOKEN_TTL, Duration::from_millis)
    }

    /// The data plane's single admission point. See
    /// [`ClientRegistry::admit_attach`], which takes the latch's value
    /// rather than reading it first, so the refusals stay in the order
    /// the client can act on.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn admit_attach(&self, token: &str, ctx: &ConnCtx) -> Result<AdmittedAttach, HandlerError> {
        let mut guard = lock(&self.clients);
        let stopping = self.stopping.load(Ordering::Acquire);
        let (admitted, displaced) = guard.admit_attach(token, ctx, stopping)?;
        // Fired under the lock so no third connection can slip between
        // "this tab's data conn is now mine" and "the old one is told".
        if let Some(closer) = displaced {
            closer.close(CloseReason::Superseded);
        }
        Ok(admitted)
    }

    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn release_data_conn(&self, tab_id: i64, conn_id: u64) {
        lock(&self.clients).release_data_conn(tab_id, conn_id);
    }
}

/// Lock recovering from poisoning: a panicked holder must not be able to
/// wedge a session's shutdown, and every field behind this lock is a
/// list of abort handles — there is no invariant a panic could have
/// broken halfway.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Ops that change workspace or PTY state — or hand out the authority to
/// change it — and so must not run once a session has latched Stopping.
///
/// `session.connect` and `tab.attach` are in the set for the second
/// reason: neither touches the workspace, but a lease or an attach token
/// minted after the latch is authority over a session that has already
/// flushed and reaped.
///
/// Reads (`identify`, `tab.list`, `tab.dump*`, `session.identify`) stay
/// answerable throughout, so a client can still find out what happened.
/// The UI-only ops (`palette.*`, `window.*`, `app.*`, clipboard,
/// selection) are not listed: they route through `ui_call`, and a
/// session socket has no UI attached, so they already fail with
/// `internal: no UI attached`. `tab.feed_pty_bytes` is the exception —
/// on a session it writes into the tab task's terminal, which is a
/// mutation like any other. This whole set is consulted only on a
/// session socket, so listing it costs a UI nothing.
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
            | ops::SESSION_CONNECT
            | ops::SESSION_SET_THEME
            | ops::SESSION_SET_FOCUS
            | ops::TAB_ATTACH
            | ops::TAB_FEED_PTY_BYTES
            | ops::HOST_ADD
            | ops::HOST_REMOVE
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
            clients: std::sync::Mutex::new(ClientRegistry::default()),
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

    /// Whether a UI adapter is driving this handler.
    ///
    /// Only the `host.*` registry ops ask: they have a working headless
    /// implementation and route through the app purely so a live UI
    /// reconciles (see [`UiRequest::HostAdd`]). Everything else is
    /// UI-only and lets `ui_call` answer `no UI attached`.
    fn has_ui(&self) -> bool {
        self.ui_tx.is_some()
    }

    /// Hand a request-reply [`UiRequest`] to the UI adapter's main thread
    /// and await its answer. The outer `Result` reports channel/UI health
    /// (no UI attached, UI gone, reply dropped); the inner `Result` is
    /// the op's own outcome, which the caller maps to the right error
    /// code (e.g. `not-found` for a missing tab). Shared by the
    /// screenshot + dump arms so the oneshot plumbing lives in one place.
    ///
    /// The error half is whatever the variant's reply channel carries:
    /// `String` for most, a [`WorkspaceError`] for the `host.*` ops so
    /// the dispatcher can mint the same wire code the headless path
    /// does, and a [`HostOpFailure`] for the ops the app forwards to a
    /// session, whose refusal already has a code of its own.
    async fn ui_call<T, E>(
        &self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, E>>) -> UiRequest,
    ) -> Result<Result<T, E>, HandlerError> {
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
        ctx: &'a ConnCtx,
        op: &'a str,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        Box::pin(async move { dispatch_outcome(self, ctx, op, params).await })
    }

    /// The other half of `session.set_focus`'s lifetime rule: a focus a
    /// client reported is only true while that client is still there.
    ///
    /// The lease deliberately outlives its connections (a reconnect is a
    /// takeover), so this cannot release the lease — but when the last
    /// connection under it goes, nobody is looking at this session any
    /// more, and leaving the flag set would mute one tab until some
    /// future client happens to move the selection. A UI socket has no
    /// lease registry and does nothing here.
    fn connection_ended(&self, conn_id: u64) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if session.forget_connection(conn_id) {
            self.workspace.release_client_focus();
        }
    }

    /// A data connection is a session's business only. Without a
    /// [`SessionState`] this is a UI socket, and the answer is the same
    /// "not-supported" the trait's default gives — restated here rather
    /// than delegated because overriding the method takes the default
    /// off the table.
    #[cfg(feature = "server-vt")]
    fn handle_data<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        handshake: roost_ipc::messages::AttachHandshake,
        conn: roost_ipc::DataConn,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if self.session.is_none() {
                crate::attach::refuse(
                    conn,
                    "not-supported",
                    "this socket does not serve attach data connections",
                )
                .await;
                return;
            }
            crate::attach::serve_attach(self, ctx, handshake, conn).await;
        })
    }
}

/// The two registry operations the data plane needs. They live on the
/// handler rather than on `SessionState` because the forwarder lives in
/// another module and the registry is this one's private business.
#[cfg(feature = "server-vt")]
impl IpcHandler {
    pub(crate) fn admit_attach(
        &self,
        token: &str,
        ctx: &ConnCtx,
    ) -> Result<AdmittedAttach, HandlerError> {
        self.session
            .as_ref()
            .ok_or_else(|| {
                HandlerError::new(
                    "not-supported",
                    "this socket does not serve attach data connections",
                )
            })?
            .admit_attach(token, ctx)
    }

    pub(crate) fn release_data_conn(&self, tab_id: i64, conn_id: u64) {
        if let Some(session) = self.session.as_ref() {
            session.release_data_conn(tab_id, conn_id);
        }
    }
}

/// The tab task's command channel for a session-served op, or the error
/// a client gets when the tab has no live terminal.
#[cfg(feature = "server-vt")]
fn tab_commands(
    h: &IpcHandler,
    tab_id: i64,
) -> Result<tokio::sync::mpsc::Sender<crate::tab_task::TabCmd>, HandlerError> {
    h.supervisor
        .tab_commands(tab_id)
        .ok_or_else(|| HandlerError::not_found(format!("tab {tab_id} has no live terminal")))
}

/// The one answer for a tab whose task stopped listening, whichever half
/// of a round trip noticed — the same "the tab is gone" a UI socket
/// gives for a dead tab.
#[cfg(feature = "server-vt")]
fn tab_gone(tab_id: i64) -> HandlerError {
    HandlerError::not_found(format!("tab {tab_id} is gone"))
}

/// The one answer for a data-plane op on a session that cannot serve
/// one — either the feature was compiled out or `enable_server_vt` was
/// never called. A client cannot act on the difference, and the text is
/// worded to be true of both.
fn no_server_vt() -> HandlerError {
    HandlerError::new(
        "unsupported-kind",
        "this session has no server-VT data plane",
    )
}

/// Round-trip one command through a tab task.
#[cfg(feature = "server-vt")]
async fn tab_ask<T>(
    h: &IpcHandler,
    tab_id: i64,
    make: impl FnOnce(
        tokio::sync::oneshot::Sender<Result<T, crate::tab_task::TabError>>,
    ) -> crate::tab_task::TabCmd,
) -> Result<T, HandlerError> {
    let commands = tab_commands(h, tab_id)?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    commands
        .send(make(reply_tx))
        .await
        .map_err(|_| tab_gone(tab_id))?;
    reply_rx
        .await
        .map_err(|_| tab_gone(tab_id))?
        .map_err(tab_err)
}

/// Map a tab-task failure onto the wire. `Gone` and a missing tab are
/// the same fact to a client; a render or encode failure is the server's
/// own problem and says so.
#[cfg(feature = "server-vt")]
#[allow(clippy::needless_pass_by_value)] // `Result::map_err` adapter owns its error.
fn tab_err(e: crate::tab_task::TabError) -> HandlerError {
    use crate::tab_task::TabError;
    match e {
        TabError::Gone | TabError::RingMiss { .. } => HandlerError::not_found(e.to_string()),
        TabError::SnapshotFailed(_) | TabError::Render(_) => {
            HandlerError::new("internal", e.to_string())
        }
    }
}

/// Whether the session serving this op was started in test mode. A UI
/// socket never reaches these arms (`dispatch` routes it to `ui_call`),
/// so `false` here means a production daemon.
#[cfg(feature = "server-vt")]
fn session_test_mode(h: &IpcHandler) -> Result<(), HandlerError> {
    if h.session.as_ref().is_some_and(|s| s.info.test_mode) {
        return Ok(());
    }
    Err(HandlerError::new(
        "not-enabled",
        "this op requires the session to have been started with ROOST_TEST_MODE=1",
    ))
}

/// A session's ids are one bare id-space by design; the `h<host>.<id>`
/// spelling names a UI's client-side tab and is refused rather than
/// silently narrowed to a number that would read (or reorder) some
/// unrelated tab.
fn bare_tab(tab: WireTabRef) -> Result<i64, HandlerError> {
    tab.local().ok_or_else(|| {
        HandlerError::invalid_param(
            "host-qualified tab refs are a UI-socket form; session tab ids are bare",
        )
    })
}

/// [`bare_tab`]'s project twin, for the reorder ops' `WireProjectRef`.
fn bare_project(project: WireProjectRef) -> Result<i64, HandlerError> {
    project.local().ok_or_else(|| {
        HandlerError::invalid_param(
            "host-qualified project refs are a UI-socket form; session project ids are bare",
        )
    })
}

/// Which instance a reorder request names: this process's own
/// workspace, or a connected host's session, reached over the app's op
/// queue (plan 044 §3.1 d6). The ids the route comes back with are
/// already narrowed to that instance's own bare id-space.
enum ReorderInstance {
    Local,
    Host(u32),
}

/// A reorder names one instance or the other, never both — it carries a
/// whole order in one id-space, and a list half in a host's numbering
/// and half in ours would reorder something nobody asked for.
fn mixed_refs(op: &str) -> HandlerError {
    HandlerError::invalid_param(format!(
        "{op}: every ref in one request must name the same instance — either all bare (local) \
         or all `h<host>.<id>` on one host"
    ))
}

/// The host form is client state: the connection whose session would
/// serve it lives in the app, so a socket with no UI has nothing to
/// route to. The `tab.focus` guard, one op over.
fn needs_a_ui(op: &str) -> HandlerError {
    HandlerError::invalid_param(format!(
        "a host-qualified {op} needs a UI: host connections are client state"
    ))
}

/// A session socket has one bare id-space and no host connections, so a
/// qualified ref is refused by name there rather than narrowed — the
/// same rule `tab.dump` applies, for the same reason.
fn is_a_session_socket(h: &IpcHandler) -> bool {
    h.session.is_some()
}

fn tab_reorder_route(
    h: &IpcHandler,
    p: TabReorderParams,
) -> Result<(ReorderInstance, i64, Vec<i64>), HandlerError> {
    let op = ops::TAB_REORDER;
    if is_a_session_socket(h) {
        let project_id = bare_project(p.project_id)?;
        let tab_ids = p
            .tab_ids
            .into_iter()
            .map(bare_tab)
            .collect::<Result<_, _>>()?;
        return Ok((ReorderInstance::Local, project_id, tab_ids));
    }
    match p.project_id {
        WireProjectRef::Local(project_id) => {
            let tab_ids = p
                .tab_ids
                .into_iter()
                .map(|tab| tab.local().ok_or_else(|| mixed_refs(op)))
                .collect::<Result<_, _>>()?;
            Ok((ReorderInstance::Local, project_id, tab_ids))
        }
        WireProjectRef::Host { host, project } => {
            let tab_ids = p
                .tab_ids
                .into_iter()
                .map(|tab| match tab {
                    WireTabRef::Host { host: named, tab } if named == host => Ok(tab),
                    _ => Err(mixed_refs(op)),
                })
                .collect::<Result<_, _>>()?;
            if !h.has_ui() {
                return Err(needs_a_ui(op));
            }
            Ok((ReorderInstance::Host(host), project, tab_ids))
        }
    }
}

fn project_reorder_route(
    h: &IpcHandler,
    p: ProjectReorderParams,
) -> Result<(ReorderInstance, Vec<i64>), HandlerError> {
    let op = ops::PROJECT_REORDER;
    if is_a_session_socket(h) {
        let project_ids = p
            .project_ids
            .into_iter()
            .map(bare_project)
            .collect::<Result<_, _>>()?;
        return Ok((ReorderInstance::Local, project_ids));
    }
    // An empty list names no instance, so it stays the local no-op it
    // has always been. So does a list whose first ref is bare: a
    // qualified one later in it is the mixed form, not a host route.
    let host = match p.project_ids.first() {
        Some(WireProjectRef::Host { host, .. }) => *host,
        _ => {
            let project_ids = p
                .project_ids
                .into_iter()
                .map(|project| project.local().ok_or_else(|| mixed_refs(op)))
                .collect::<Result<_, _>>()?;
            return Ok((ReorderInstance::Local, project_ids));
        }
    };
    let project_ids = p
        .project_ids
        .into_iter()
        .map(|project| match project {
            WireProjectRef::Host {
                host: named,
                project,
            } if named == host => Ok(project),
            _ => Err(mixed_refs(op)),
        })
        .collect::<Result<_, _>>()?;
    if !h.has_ui() {
        return Err(needs_a_ui(op));
    }
    Ok((ReorderInstance::Host(host), project_ids))
}

/// The four terminal-reading ops a session answers from its own tab
/// tasks instead of from a UI it does not have.
///
/// `None` means "not a session socket, keep the UI path" — the whole of
/// what makes these additive: a UI handler reaches `ui_call` exactly as
/// it always did, byte for byte.
#[cfg(feature = "server-vt")]
mod served {
    use super::{
        bare_tab as bare, session_test_mode, tab_ask, tab_commands, tab_gone, DumpData,
        HandlerError, IpcHandler, ResolvedCellsData, WireTabRef,
    };
    use crate::tab_task::TabCmd;

    pub(super) async fn dump(
        h: &IpcHandler,
        tab: WireTabRef,
    ) -> Option<Result<DumpData, HandlerError>> {
        h.session.as_ref()?;
        Some(match bare(tab) {
            Ok(tab_id) => tab_ask(h, tab_id, TabCmd::Dump).await,
            Err(error) => Err(error),
        })
    }

    pub(super) async fn dump_resolved(
        h: &IpcHandler,
        tab: WireTabRef,
    ) -> Option<Result<ResolvedCellsData, HandlerError>> {
        h.session.as_ref()?;
        Some(match bare(tab) {
            Ok(tab_id) => tab_ask(h, tab_id, TabCmd::DumpResolved).await,
            Err(error) => Err(error),
        })
    }

    pub(super) async fn feed_pty_bytes(
        h: &IpcHandler,
        tab_id: i64,
        data: Vec<u8>,
    ) -> Option<Result<(), HandlerError>> {
        h.session.as_ref()?;
        Some(feed(h, tab_id, data).await)
    }

    /// Injected bytes are chunked to the same granularity the real PTY
    /// reader produces, and for the same reason the reader has one: a
    /// chunk is the unit a seq is assigned to, so an unchunked megabyte
    /// would be ONE tee record — one PTY frame past the wire's 1 MiB
    /// frame cap, fatal to every attached client. Splitting here keeps a
    /// test-mode injection indistinguishable from a busy child.
    const FEED_CHUNK_BYTES: usize = 4096;

    async fn feed(h: &IpcHandler, tab_id: i64, data: Vec<u8>) -> Result<(), HandlerError> {
        session_test_mode(h)?;
        let commands = tab_commands(h, tab_id)?;
        // An empty payload sends nothing: it would take a seq and tee a
        // record with no bytes, which is a PTY frame no client accepts.
        for chunk in data.chunks(FEED_CHUNK_BYTES) {
            commands
                .send(TabCmd::FeedBytes(chunk.to_vec()))
                .await
                .map_err(|_| tab_gone(tab_id))?;
        }
        Ok(())
    }

    pub(super) async fn capture_pty_input(
        h: &IpcHandler,
        tab: WireTabRef,
        drain: bool,
    ) -> Option<Result<Vec<u8>, HandlerError>> {
        h.session.as_ref()?;
        Some(match bare(tab) {
            Ok(tab_id) => capture(h, tab_id, drain).await,
            Err(error) => Err(error),
        })
    }

    async fn capture(h: &IpcHandler, tab_id: i64, drain: bool) -> Result<Vec<u8>, HandlerError> {
        session_test_mode(h)?;
        let commands = tab_commands(h, tab_id)?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        commands
            .send(TabCmd::CaptureInput {
                drain,
                reply: reply_tx,
            })
            .await
            .map_err(|_| tab_gone(tab_id))?;
        reply_rx.await.map_err(|_| tab_gone(tab_id))
    }
}

/// Without the feature there are no tab tasks, so every op keeps the UI
/// path — which on a session answers `internal: no UI attached`, the
/// same honest failure it gave before host sessions existed.
#[cfg(not(feature = "server-vt"))]
mod served {
    // Signature parity with the served twins is the point of these, so
    // every one is an `async fn` that never awaits.
    #![allow(clippy::unused_async)]

    use super::{DumpData, HandlerError, IpcHandler, ResolvedCellsData, WireTabRef};

    pub(super) async fn dump(
        _: &IpcHandler,
        _: WireTabRef,
    ) -> Option<Result<DumpData, HandlerError>> {
        None
    }

    pub(super) async fn dump_resolved(
        _: &IpcHandler,
        _: WireTabRef,
    ) -> Option<Result<ResolvedCellsData, HandlerError>> {
        None
    }

    pub(super) async fn feed_pty_bytes(
        _: &IpcHandler,
        _: i64,
        _: Vec<u8>,
    ) -> Option<Result<(), HandlerError>> {
        None
    }

    pub(super) async fn capture_pty_input(
        _: &IpcHandler,
        _: WireTabRef,
        _: bool,
    ) -> Option<Result<Vec<u8>, HandlerError>> {
        None
    }
}

/// The session layer wrapped around the op dispatcher: `session.*`, the
/// stop latch, and the mutation barrier. Without a [`SessionState`] this
/// is a straight pass-through, so a UI socket's wire behavior is exactly
/// what it was before host sessions existed.
async fn dispatch_outcome(
    h: &IpcHandler,
    ctx: &ConnCtx,
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
            return events_subscribe(h, session, ctx, &p);
        }
        // The host registry is client-side state (D8): a UI socket's op
        // family. Letting it fall through would grow a shadow registry in
        // the daemon's own state.json that nothing ever reads.
        ops::HOST_ADD
        | ops::HOST_REMOVE
        | ops::HOST_LIST
        | ops::HOST_CONNECT
        | ops::HOST_DISCONNECT
        | ops::HOST_STATUS => {
            return Err(HandlerError::unknown_op(op));
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

    // Served here rather than in `dispatch`, which has no connection
    // identity — and a lease that nothing can be registered against is
    // not a lease.
    if op == ops::SESSION_CONNECT {
        let p: SessionConnectParams = decode(params)?;
        let lease = session.connect(p.takeover, ctx)?;
        // A new lease means a new client, and the focus the previous one
        // reported was a statement about *its* window. Carried over, it
        // would mute one tab for a client that never said anything — the
        // headless-default bug `session.set_focus` exists to fix,
        // rebuilt out of stale state. The fresh holder states its own
        // focus right after connecting.
        h.workspace.release_client_focus();
        // `snapshot_with_revision` rather than `revision`: one lock
        // acquisition means the number a client fences its first
        // `tab.list` against is a state-consistent read, not one that
        // could have moved between two.
        let (revision, _projects) = h.workspace.snapshot_with_revision();
        return encode(&SessionConnectResult { lease, revision }).map(HandlerOutcome::Reply);
    }

    // Served here rather than in `dispatch` for the same reason
    // `tab.attach` is: the lease it presents is bound to *this*
    // connection, which `dispatch` cannot see. And it is lease-gated at
    // all because it is the attached client's theme — only the client
    // driving the session gets to state it.
    if op == ops::SESSION_SET_THEME {
        let p: SessionSetThemeParams = decode(params)?;
        return session_set_theme(h, session, ctx, p)
            .await
            .map(HandlerOutcome::Reply);
    }

    // Connection-scoped for the same reason as its two neighbours: the
    // lease it presents is this connection's, and what the op states —
    // "I am looking at this tab" — is only true for as long as the
    // connection that said it holds the lease. `dispatch` can see
    // neither.
    if op == ops::SESSION_SET_FOCUS {
        let p: SessionSetFocusParams = decode(params)?;
        return session_set_focus(h, session, ctx, &p).map(HandlerOutcome::Reply);
    }

    // Same reason as `session.connect`: the token is bound to the lease
    // presented on *this* connection, which `dispatch` cannot see.
    if op == ops::TAB_ATTACH {
        let p: TabAttachParams = decode(params)?;
        return tab_attach(h, session, ctx, p)
            .await
            .map(HandlerOutcome::Reply);
    }

    dispatch(h, op, params).await.map(HandlerOutcome::Reply)
}

/// `tab.attach`: negotiate a payload kind and hand back a single-use
/// ticket for one data connection.
///
/// The validation order is pinned (D5) and each earlier failure wins,
/// because the codes instruct differently: `connect-required` means "go
/// get a lease", `not-found` means "that tab is gone", `unsupported-kind`
/// and `build-mismatch` both mean "we cannot talk", and only then does
/// geometry get looked at. Reordering would tell a client to fix the
/// wrong thing.
#[cfg(feature = "server-vt")]
async fn tab_attach(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: TabAttachParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;

    // One lookup, so the channel this resizes, the generation the token
    // is stamped with, and the task the forwarder will snapshot are the
    // same pipeline — two reads could straddle a respawn.
    let (commands, tab_generation) = h.supervisor.tab_task_handle(p.tab_id).ok_or_else(|| {
        HandlerError::not_found(format!("tab {} has no live terminal to attach", p.tab_id))
    })?;

    // A list mixing kinds this build has never heard of with one it
    // serves is fine — the client states a preference order and the
    // first servable entry wins. "Servable" is what `session.identify`
    // ADVERTISED (`payload_kinds`), intersected with what this build
    // can actually encode — the advertisement is the contract a client
    // negotiated against, so a kind absent from it must not be accepted
    // even when the code could produce it.
    let kind = p
        .kinds
        .iter()
        .find(|kind| {
            kind.as_str() == AttachPayloadKind::GHOSTTY_SNAPSHOT
                && session.info.payload_kinds.contains(kind)
        })
        .cloned()
        .ok_or_else(|| {
            HandlerError::new(
                "unsupported-kind",
                format!(
                    "this session serves {:?}; the client offered {:?}",
                    session.info.payload_kinds, p.kinds
                ),
            )
        })?;

    // Exact match, both strings named: two libghostty builds that
    // disagree cannot exchange a snapshot, and a client that sees only
    // "mismatch" cannot tell which side to upgrade.
    if p.libghostty_build != session.info.libghostty_build {
        return Err(HandlerError::new(
            "build-mismatch",
            format!(
                "this session is {:?}; the client is {:?}",
                session.info.libghostty_build, p.libghostty_build
            ),
        ));
    }

    // Zero cell pixels are legal — a headless client has no cell metrics
    // to report — but a zero-sized grid is not a grid.
    if p.cols == 0 || p.rows == 0 {
        return Err(HandlerError::invalid_param(format!(
            "cols and rows must both be non-zero (got {}x{})",
            p.cols, p.rows
        )));
    }

    // The attach geometry is the client's, so the tab takes it now
    // rather than at first frame: a snapshot encoded at the old size
    // would be re-laid-out on the client the instant it resized.
    // Detach never resizes back (roadmap D7).
    //
    // Awaited, not fired and forgotten: the ticket minted below is the
    // client's authority to snapshot this tab, and a `Resize` still
    // sitting on the command channel would let that snapshot be encoded
    // at the geometry the attach exists to replace.
    let (resized_tx, resized_rx) = tokio::sync::oneshot::channel();
    commands
        .send(crate::tab_task::TabCmd::Resize {
            cols: p.cols,
            rows: p.rows,
            cell_w: u32::from(p.cell_w_px),
            cell_h: u32::from(p.cell_h_px),
            ack: Some(resized_tx),
        })
        .await
        .map_err(|_| tab_gone(p.tab_id))?;
    resized_rx
        .await
        // The task dropped the ack without answering, which only
        // happens when the task itself is going away.
        .map_err(|_| tab_gone(p.tab_id))?
        // The terminal refused the geometry the client asked for, which
        // is the client's parameter to fix.
        .map_err(|error| {
            HandlerError::invalid_param(format!(
                "tab {} could not be resized to {}x{}: {error}",
                p.tab_id, p.cols, p.rows
            ))
        })?;

    let attach_token = session.mint_attach_token(&p.lease, p.tab_id, tab_generation)?;
    encode(&TabAttachResult {
        attach_token,
        kind,
        server_epoch: h.supervisor.server_epoch().unwrap_or_default(),
        tab_generation,
    })
}

/// Without the `server-vt` feature there is no server terminal to
/// snapshot, so there is nothing to hand a ticket for.
#[cfg(not(feature = "server-vt"))]
#[allow(clippy::unused_async)]
async fn tab_attach(
    _h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: TabAttachParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;
    Err(no_server_vt())
}

/// `session.set_theme`: seed every tab's server terminal with the
/// attached client's palette, and remember it for the tabs opened next
/// (plan 037 §3.6).
///
/// This is the op that closes the reseed gap the architecture notes
/// left open: without it, a session's terminals answer OSC 4 / 10 / 11 /
/// 12 queries with the headless white-on-black default, so a program in
/// a host tab picks its colors against a theme nobody is looking at.
///
/// Whole-theme, not a diff: the client states the palette it renders
/// with and the server takes it. Two clients racing (a takeover during
/// a theme change) are last-writer-wins by construction — there is one
/// stored seed and the last `set_theme` to reach the tab task is the
/// one its terminal ends on.
#[cfg(feature = "server-vt")]
async fn session_set_theme(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: SessionSetThemeParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;
    let seed = decode_osc_colors(&p.osc_colors)?;
    // Storing the seed and reseeding the live tabs is one supervisor
    // call — see `PtySupervisor::set_theme` for why the pair cannot be
    // split without a spawn racing between them.
    let tabs = h
        .supervisor
        .set_theme(&seed)
        .await
        .ok_or_else(no_server_vt)?;
    encode(&SessionSetThemeResult { tabs })
}

/// Without the `server-vt` feature there are no server terminals to
/// recolor — the same answer `tab.attach` gives, for the same reason.
#[cfg(not(feature = "server-vt"))]
#[allow(clippy::unused_async)]
async fn session_set_theme(
    _h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: SessionSetThemeParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;
    Err(no_server_vt())
}

/// `session.set_focus`: take the attached client's real focus (plan 038
/// §C6).
///
/// A session's workspace has no window of its own, so it defaults to
/// focused and its active tab is whatever its restored layout selected —
/// leaving `attention_suppressed_by_focus` permanently true for one tab
/// per session and muting that tab's agent entirely. The client that
/// *does* have a window is the only thing that can say otherwise, and
/// this is where it says it.
///
/// Unlike its two neighbours there is no `server-vt` twin: nothing here
/// touches a server terminal, and a featureless build's notification
/// routing is the same routing. The whole apply is one workspace
/// transaction — see [`Workspace::set_client_focus`] for why the
/// validation has to happen inside it.
fn session_set_focus(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: &SessionSetFocusParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;
    h.workspace
        .set_client_focus(p.focused_tab_id)
        .map_err(ws_err)?;
    // Only once it applied: a refused focus is not a claim, and
    // recording one would let a close retire a focus this connection
    // never established.
    session.claim_focus(ctx, p.focused_tab_id.is_some());
    Ok(serde_json::json!({}))
}

/// Wire colors → the engine's theme seed.
///
/// `#rrggbb` is the spelling `tab.dump_resolved` already answers in, so
/// a theme is readable in a wire trace and a test can state one as a
/// literal. Every failure is `invalid-param` and names the field: a
/// half-applied palette is worse than a refused one.
#[cfg(feature = "server-vt")]
fn decode_osc_colors(
    colors: &roost_ipc::messages::OscColorsParams,
) -> Result<crate::osc::OscColorSnapshot, HandlerError> {
    if colors.palette.len() != 256 {
        return Err(HandlerError::invalid_param(format!(
            "osc_colors.palette must have exactly 256 entries (got {})",
            colors.palette.len()
        )));
    }
    let mut palette = [(0u8, 0u8, 0u8); 256];
    for (index, raw) in colors.palette.iter().enumerate() {
        palette[index] = parse_rgb_hex(raw)
            .ok_or_else(|| invalid_color(&format!("osc_colors.palette[{index}]"), raw))?;
    }
    Ok(crate::osc::OscColorSnapshot::new(
        parse_rgb_hex(&colors.foreground)
            .ok_or_else(|| invalid_color("osc_colors.foreground", &colors.foreground))?,
        parse_rgb_hex(&colors.background)
            .ok_or_else(|| invalid_color("osc_colors.background", &colors.background))?,
        parse_rgb_hex(&colors.cursor)
            .ok_or_else(|| invalid_color("osc_colors.cursor", &colors.cursor))?,
        palette,
    ))
}

#[cfg(feature = "server-vt")]
fn invalid_color(field: &str, raw: &str) -> HandlerError {
    HandlerError::invalid_param(format!("{field} is not a #rrggbb color (got {raw:?})"))
}

/// The inverse of [`rgb_hex`]. Long form only — the wire is machine-
/// written, and accepting `#abc` too would mean two spellings of one
/// color for the vectors to disagree about.
#[cfg(feature = "server-vt")]
fn parse_rgb_hex(raw: &str) -> Option<(u8, u8, u8)> {
    let body = raw.strip_prefix('#')?;
    if body.len() != 6 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&body[0..2], 16).ok()?,
        u8::from_str_radix(&body[2..4], 16).ok()?,
        u8::from_str_radix(&body[4..6], 16).ok()?,
    ))
}

/// `events.subscribe` on a session socket: ack with the fence, then push.
///
/// Lease-gated (D4): the event stream is interactive authority, so a
/// caller presents the lease `session.connect` handed it and the
/// connection joins the registry under that lease — which is what lets a
/// later takeover close this stream rather than leaving two clients both
/// believing they drive the session.
///
/// Not a mutating op — it changes no workspace state — but it does
/// establish a resource, so it is refused once the session has latched:
/// a stream handed out after the stop swept the registry would be one
/// nobody can end. [`SessionState::register_push`] closes the race by
/// making the registration itself the check.
fn events_subscribe(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    params: &EventsSubscribeParams,
) -> Result<HandlerOutcome, HandlerError> {
    if params.tab_id_filter != 0 {
        return Err(HandlerError::invalid_param(format!(
            "tab_id_filter is not implemented (got {}); subscribe unfiltered and filter \
             client-side until HS-2 adds it",
            params.tab_id_filter
        )));
    }
    // `require_lease` also refuses under the stop latch, sharing the
    // registry sweep's lock — the check-then-register pair is atomic.
    session.require_lease(&params.lease, ctx)?;
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
    // to be cut.
    //
    // Order matters and is the whole of deviation #2's fix: the closers
    // fire FIRST, so every push connection observes a reason and writes
    // the terminal `session.stopping` envelope before it goes. Aborting
    // the relays first would drop their senders, `serve_push`'s source
    // would end, and the peer would get a bare EOF it cannot tell from a
    // crash. The abort still follows, as the guarantee that a relay with
    // no registered connection — or one whose peer stopped reading — ends
    // regardless.
    session.close_clients(CloseReason::ShuttingDown);
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
            let data = match served::dump(h, p.tab_id).await {
                Some(served) => served?,
                None => h
                    .ui_call(|reply| UiRequest::Dump {
                        tab_id: p.tab_id,
                        reply,
                    })
                    .await?
                    .map_err(HandlerError::not_found)?,
            };
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
            let (instance, project_id, tab_ids) = tab_reorder_route(h, p)?;
            match instance {
                ReorderInstance::Local => {
                    h.workspace
                        .reorder_tabs(project_id, &tab_ids)
                        .map_err(ws_err)?;
                }
                ReorderInstance::Host(host) => {
                    h.ui_call(|reply| UiRequest::HostTabReorder {
                        host,
                        project_id,
                        tab_ids,
                        reply,
                    })
                    .await??;
                }
            }
            Ok(serde_json::json!({}))
        }
        ops::PROJECT_REORDER => {
            let p: ProjectReorderParams = decode(params)?;
            let (instance, project_ids) = project_reorder_route(h, p)?;
            match instance {
                ReorderInstance::Local => {
                    h.workspace.reorder_projects(&project_ids).map_err(ws_err)?;
                }
                ReorderInstance::Host(host) => {
                    h.ui_call(|reply| UiRequest::HostProjectReorder {
                        host,
                        project_ids,
                        reply,
                    })
                    .await??;
                }
            }
            Ok(serde_json::json!({}))
        }
        ops::TAB_FOCUS => {
            let p: TabFocusParams = decode(params)?;
            let tab_id = match p.tab_id {
                WireTabRef::Local(tab_id) => tab_id,
                WireTabRef::Host { host, tab } => {
                    if !h.has_ui() {
                        return Err(HandlerError::invalid_param(
                            "a host-qualified tab.focus needs a UI: host selection is client state",
                        ));
                    }
                    h.ui_call(|reply| UiRequest::HostTabFocus {
                        host,
                        tab_id: tab,
                        reply,
                    })
                    .await?
                    .map_err(ws_err)?;
                    // The host's own workspace owns its active row; this
                    // client only moved its selection, so there is no
                    // local "previous" to report.
                    return encode(&TabFocusResult {
                        previous_project_id: 0,
                        previous_tab_id: 0,
                    });
                }
            };
            let (previous_project_id, previous_tab_id) =
                h.workspace.focus_tab(tab_id).map_err(ws_err)?;
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
            match served::feed_pty_bytes(h, p.tab_id, p.data.clone()).await {
                Some(served) => served?,
                None => h
                    .ui_call(|reply| UiRequest::TabFeedPtyBytes {
                        tab_id: p.tab_id,
                        data: p.data,
                        reply,
                    })
                    .await?
                    .map_err(map_test_op_err)?,
            }
            Ok(serde_json::json!({}))
        }
        ops::TAB_CAPTURE_PTY_INPUT => {
            let p: TabCapturePtyInputParams = decode(params)?;
            let data = match served::capture_pty_input(h, p.tab_id, p.drain).await {
                Some(served) => served?,
                None => h
                    .ui_call(|reply| UiRequest::TabCapturePtyInput {
                        tab_id: p.tab_id,
                        drain: p.drain,
                        reply,
                    })
                    .await?
                    .map_err(map_test_op_err)?,
            };
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
            let dump = match served::dump_resolved(h, p.tab_id).await {
                Some(served) => served?,
                None => h
                    .ui_call(|reply| UiRequest::TabDumpResolved {
                        tab_id: p.tab_id,
                        reply,
                    })
                    .await?
                    .map_err(HandlerError::not_found)?,
            };
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
        ops::APP_DIALOG_DUMP => {
            let _: AppDialogDumpParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::AppDialogDump { reply })
                .await?
                .map_err(map_test_op_err)?;
            encode(&result)
        }
        ops::APP_DIALOG_ANSWER => {
            let p: AppDialogAnswerParams = decode(params)?;
            if !matches!(p.action.as_str(), "confirm" | "cancel") {
                return Err(HandlerError::invalid_param(format!(
                    "action must be confirm or cancel (got {:?})",
                    p.action
                )));
            }
            h.ui_call(|reply| UiRequest::AppDialogAnswer {
                action: p.action,
                reply,
            })
            .await?
            .map_err(map_test_op_err)?;
            Ok(serde_json::json!({}))
        }
        ops::APP_KEYBIND_DISPATCH => {
            let p: AppKeybindDispatchParams = decode(params)?;
            if p.action != "paste" {
                return Err(HandlerError::invalid_param(format!(
                    "action must be \"paste\" (got {:?})",
                    p.action
                )));
            }
            h.ui_call(|reply| UiRequest::AppKeybindDispatch {
                action: p.action,
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
        // The four registry mutations route through the app when one is
        // attached (plan 037 §3.5): the app owns the connections and the
        // sidebar, so a `roostctl host add` has to reach it or it
        // mutates state nothing re-reads. Headless — the engine's own
        // tests, an embedder with no UI — the workspace answers
        // directly, which is also why the error type crossing the seam
        // is `WorkspaceError`: both paths mint the same wire code.
        ops::HOST_ADD => {
            let p: HostAddParams = decode(params)?;
            let host = if h.has_ui() {
                h.ui_call(|reply| UiRequest::HostAdd {
                    label: p.label,
                    target: p.target,
                    reply,
                })
                .await?
                .map_err(ws_err)?
            } else {
                h.workspace
                    .add_host(&p.label, &p.target)
                    .map_err(ws_err)?
                    .into()
            };
            encode(&HostAddResult { host })
        }
        ops::HOST_REMOVE => {
            let p: HostRemoveParams = decode(params)?;
            if h.has_ui() {
                h.ui_call(|reply| UiRequest::HostRemove { id: p.id, reply })
                    .await?
                    .map_err(ws_err)?;
            } else {
                h.workspace.remove_host(&p.id).map_err(ws_err)?;
            }
            Ok(serde_json::json!({}))
        }
        ops::HOST_CONNECT | ops::HOST_DISCONNECT => {
            // Connection state is the app's alone — there is no headless
            // fallback to give, and `no UI attached` is the honest
            // answer for a socket with no window behind it.
            let (id, test_user_origin) = if op == ops::HOST_CONNECT {
                let p = decode::<HostConnectParams>(params)?;
                (p.id, p.test_user_origin)
            } else {
                (decode::<HostDisconnectParams>(params)?.id, false)
            };
            let connect = op == ops::HOST_CONNECT;
            let result = h
                .ui_call(move |reply| {
                    if connect {
                        UiRequest::HostConnect {
                            id,
                            test_user_origin,
                            reply,
                        }
                    } else {
                        UiRequest::HostDisconnect { id, reply }
                    }
                })
                .await?
                .map_err(ws_err)?;
            encode(&result)
        }
        ops::HOST_LIST => {
            let _p: HostListParams = decode(params)?;
            let hosts = h.workspace.hosts().into_iter().map(Host::from).collect();
            encode(&HostListResult { hosts })
        }
        ops::HOST_STATUS => {
            let p: HostStatusParams = decode(params)?;
            let result = h
                .ui_call(move |reply| UiRequest::HostStatus { id: p.id, reply })
                .await?
                .map_err(ws_err)?;
            encode(&result)
        }
        other => Err(HandlerError::unknown_op(other)),
    }
}

/// `persistence::HostSnapshot` (storage) → `messages::Host` (wire).
/// `Host` is foreign to this crate, but the orphan rule still allows the
/// impl here because `HostSnapshot` — the trait's type parameter — is
/// local; `roost-engine`, the only crate that sees both types, is where
/// the mapping belongs either way.
impl From<HostSnapshot> for Host {
    fn from(host: HostSnapshot) -> Self {
        Host {
            id: host.id,
            label: host.label,
            target: host.target,
            last_connected: host.last_connected,
        }
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
        || err.contains("unknown keybind action")
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
        WorkspaceError::Io(_) | WorkspaceError::Json(_) | WorkspaceError::Inconsistent(_) => {
            HandlerError::new("internal", e.to_string())
        }
        WorkspaceError::HostNotFound(_) => HandlerError::not_found(e.to_string()),
        WorkspaceError::HostLabelEmpty
        | WorkspaceError::HostLabelReserved
        | WorkspaceError::HostLabelTaken(_) => HandlerError::invalid_param(e.to_string()),
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
                test_mode: false,
            },
            stop: StopHandle::new(|| async {}),
            stopping: AtomicBool::new(false),
            barrier: tokio::sync::RwLock::new(()),
            pushes: std::sync::Mutex::new(Some(Vec::new())),
            clients: std::sync::Mutex::new(ClientRegistry::default()),
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
