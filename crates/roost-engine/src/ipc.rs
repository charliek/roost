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
use std::path::{Path, PathBuf};
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
    ClipboardDumpResult, ClipboardWriteParams, EventEnvelope, EventsSubscribeParams,
    EventsSubscribeResult, Host, HostAddParams, HostAddResult, HostConnectParams,
    HostConnectionResult, HostDisconnectParams, HostListParams, HostListResult, HostRemoveParams,
    HostStatusParams, HostStatusResult, IdentifyParams, IdentifyResult, NotificationCreateParams,
    PaletteActivateParams, PaletteDismissParams, PaletteOpenParams, PalettePresentParams,
    PalettePresentResult, PaletteQueryParams, PaletteStateParams, PaletteStateResult,
    ProjectCreateParams, ProjectCreateResult, ProjectDeleteParams, ProjectRenameParams,
    ProjectReorderParams, ResolvedCell, ScreenshotParams, ScreenshotResult, SelectionClearParams,
    SelectionDumpParams, SelectionDumpResult, SelectionSetParams, SessionConnectParams,
    SessionConnectResult, SessionDriverChangedEvent, SessionIdentify, SessionIdentifyParams,
    SessionPutFileParams, SessionPutFileResult, SessionSetAgentHooksParams,
    SessionSetAgentHooksResult, SessionSetFocusParams, SessionSetThemeParams, SessionStopParams,
    SessionStopResult, SidebarDumpParams, SidebarDumpResult, SidebarSetWidthParams,
    TabAgentReportResult, TabAttachParams, TabCapturePtyInputParams, TabCapturePtyInputResult,
    TabClearNotificationParams, TabCloseParams, TabDispatchMouseEventParams, TabDumpCursor,
    TabDumpParams, TabDumpResolvedParams, TabDumpResolvedResult, TabDumpResult,
    TabExpandSelectionAtParams, TabExpandSelectionAtResult, TabFeedImeParams,
    TabFeedPtyBytesParams, TabFocusParams, TabFocusResult, TabListResult, TabOpenParams,
    TabOpenResult, TabReorderParams, TabResizeParams, TabSendFileParams, TabSendFileResult,
    TabSetHookActiveParams, TabSetStateParams, TabSetTitleParams, TabWriteParams,
    WindowMetricsParams, WindowMetricsResult, WindowResizeParams, WireProjectRef, WireTabRef,
    MAX_DUMP_SCROLLBACK, MAX_PUT_FILE_BYTES, SESSION_DRIVER_CHANGED_EVENT, SESSION_FEATURES,
    SESSION_PROTOCOL_VERSION,
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
    /// History rows above the viewport `rows_text` shows, and the last
    /// `min(requested, scrollback_rows)` of them. Both halves are read
    /// from one terminal state so `scrollback_text`'s final entry is the
    /// row immediately above `rows_text[0]`.
    pub scrollback_rows: u32,
    pub scrollback_text: Vec<String>,
}

/// Why a UI-served `tab.dump` produced no [`DumpData`].
///
/// Typed rather than a message because `tab.dump` is served on two
/// sockets — a session's own `tab_task` reader and the app's — and one
/// op must answer the same code for the same condition: a tab the UI
/// does not have is `not-found`, a terminal read that failed is
/// `internal` (the session path's `TabError::Render`). Flattening both
/// onto a `String` folds them onto one code and tells the client to fix
/// the wrong thing — "that tab is gone" for a tab that is right there.
pub enum DumpError {
    /// No tab with that id on this UI.
    NoTab(String),
    /// The tab is there; reading its terminal failed.
    Read(String),
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

/// Reply for a [`UiRequest::Dump`]: the viewport text on success, a
/// [`DumpError`] — which failure it was — otherwise.
type DumpReply = tokio::sync::oneshot::Sender<Result<DumpData, DumpError>>;

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
    /// keyed map (plan 037 §3.4). `scrollback` is the already-clamped
    /// count of history rows to return alongside the viewport.
    Dump {
        tab_id: WireTabRef,
        scrollback: u32,
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
    /// `clipboard.write { image_png }` — the same seeding, with a real
    /// image (plan 047 §3.5). Unlike its text sibling this one is
    /// answered: the write can fail (no test mode, Wayland, a PNG that
    /// will not decode, a display server that refuses the selection)
    /// and a harness that pressed paste against a clipboard it only
    /// believed it had seeded would blame the paste path instead.
    ///
    /// The reply lands when the platform clipboard can be read back,
    /// not when the request was queued — the whole point of the seam is
    /// that a paste issued right afterwards reads what this put there.
    ClipboardWriteImage {
        png: Vec<u8>,
        reply: HostOpReply<()>,
    },
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
    /// `tab.send_file` — put these local files into that tab, the same
    /// route a native drop takes (plan 047 §3.4).
    ///
    /// `paths` arrives exactly as the caller sent it, and unread: the
    /// app validates it non-empty and absolute — §3.4 ranks the tab and
    /// the host ahead of the paths, and only the app can answer those —
    /// and the app is the process that opens them, so the handler never
    /// touches a filesystem it might not share.
    ///
    /// Like [`UiRequest::HostTabReorder`] this cannot be answered
    /// inside `update` — the uploads and the paste are a gesture the
    /// app runs over many frames — so the reply travels with the
    /// gesture and is answered from wherever it ends.
    TabSendFile {
        tab: WireTabRef,
        paths: Vec<String>,
        reply: HostOpReply<TabSendFileResult>,
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
use crate::pty::Geometry;
use crate::{
    AttentionSource, PtyError, PtySupervisor, ResumeCut, ResumeError, Workspace, WorkspaceError,
};

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

/// What a session does when a client sends `session.set_agent_hooks`.
///
/// The dependency direction is the point (plan 046 §3.4): this crate
/// decodes the op and gates it on the lease, and the *daemon* — which is
/// the process that links `roost-agent-install` and owns the `$HOME`
/// being written — supplies the doing. `roost-engine` is linked into the
/// UI processes too, and a UI has no business carrying a dotfile writer.
///
/// A handler built without one answers `not-supported`, which is the
/// honest answer for any socket that is not a host session's.
#[derive(Clone)]
pub struct AgentHooksHandle(Arc<dyn Fn(AgentHooksRequest) -> AgentHooksFuture + Send + Sync>);

/// The op's params minus the credential. The lease is this crate's to
/// check and nobody else's to hold.
#[derive(Debug, Clone)]
pub struct AgentHooksRequest {
    pub mode: roost_ipc::messages::AgentHooksMode,
    pub skip: Vec<String>,
    /// How the asking client names itself, for the host's state record.
    pub client: String,
    /// Whether the client that asked *still* holds the session — asked
    /// again at the point of effect. See [`AgentHooksAuthority`].
    pub authority: AgentHooksAuthority,
}

/// "Is the client that asked for this still the lease holder?", callable
/// from the thread doing the work.
///
/// The lease gate at the door is not enough on its own. The install
/// engine takes a per-home `flock` and can wait behind another writer
/// for seconds; the client's own 15 s budget makes a *timed-out* client
/// reconnect while the host work carries on. In that window another
/// client can take the lease over and state the opposite policy — and
/// the displaced request would then run afterwards and rewrite the files
/// it was no longer allowed to touch. So the answer travels with the
/// request and is re-asked where it counts (`roost-agent-install`'s
/// `ensure_on_behalf` asks it under the lock).
///
/// It is a closure rather than a lease string because the registry that
/// can answer lives in this module and nothing outside it should be able
/// to read or forge a lease token.
#[derive(Clone)]
pub struct AgentHooksAuthority(Arc<dyn Fn() -> bool + Send + Sync>);

impl AgentHooksAuthority {
    pub fn new(f: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// For a caller with no authority to lose — the tests, and any
    /// future backend that is not driven by a lease.
    pub fn always() -> Self {
        Self::new(|| true)
    }

    pub fn holds(&self) -> bool {
        (self.0)()
    }
}

impl std::fmt::Debug for AgentHooksAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentHooksAuthority")
    }
}

/// Why a session could not run an install at all.
///
/// Typed rather than a string because the two answers instruct
/// differently on the wire: `Unauthorized` is `taken-over` (stop driving
/// this session), everything else is `internal` (the wiring failed, the
/// session is fine). A *per-agent* failure is neither — it rides back in
/// the reply's `errors`.
#[derive(Debug)]
pub enum AgentHooksError {
    /// The lease that asked had been taken over by the time the install
    /// could act. Nothing was written.
    Unauthorized,
    /// A whole-run failure: no `$HOME`, an unwritable state record, a
    /// lock another writer never released.
    Failed(String),
}

impl std::fmt::Display for AgentHooksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentHooksError::Unauthorized => f.write_str(
                "the lease that asked for this was taken over before the install could run; \
                 nothing was written",
            ),
            AgentHooksError::Failed(error) => f.write_str(error),
        }
    }
}

type AgentHooksFuture =
    Pin<Box<dyn Future<Output = Result<SessionSetAgentHooksResult, AgentHooksError>> + Send>>;

impl AgentHooksHandle {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn(AgentHooksRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<SessionSetAgentHooksResult, AgentHooksError>> + Send + 'static,
    {
        Self(Arc::new(move |request| Box::pin(f(request))))
    }

    async fn run(
        &self,
        request: AgentHooksRequest,
    ) -> Result<SessionSetAgentHooksResult, AgentHooksError> {
        (self.0)(request).await
    }
}

impl std::fmt::Debug for AgentHooksHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentHooksHandle")
    }
}

/// Admission cap a [`FileStore`] gets when the caller states none.
pub const FILE_STORE_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Longest `name` `session.put_file` will land.
const MAX_PUT_FILE_NAME_BYTES: usize = 128;

/// Where a session lands the files a client uploads for its tabs, and
/// the accounting that decides whether the next one fits.
///
/// The dependency direction mirrors [`AgentHooksHandle`]: this crate
/// decodes `session.put_file` and gates it on the lease, and the
/// *daemon* — the process that owns the host's cache directory and
/// sweeps it — supplies the root. A handler built without one answers
/// `not-supported`, the honest answer for any socket that is not a host
/// session's.
///
/// **Nothing here ever evicts** (plan 047 §3.1). A path this store has
/// handed out stays valid until the session stops cleanly or starts
/// again, because it may sit unsubmitted in an agent's composer for an
/// hour; a store with no room answers `store-full` and deletes nothing.
#[derive(Clone)]
pub struct FileStore(Arc<FileStoreState>);

struct FileStoreState {
    root: PathBuf,
    cap: u64,
    /// Logical bytes landed: summed once by walking `root`, maintained
    /// per write afterwards. Admission and directory allocation both
    /// happen under this lock, so two connections cannot each be told
    /// their file fits the same remaining room.
    used: std::sync::Mutex<u64>,
}

impl FileStore {
    /// A store over `root` with the default cap.
    ///
    /// `root` must already exist and be private to this user — the
    /// session creates it, because the session is also what sweeps it.
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        Self::with_cap(root, FILE_STORE_CAP_BYTES)
    }

    /// [`Self::new`] with the cap stated, so a test can fill a store
    /// without writing half a gigabyte.
    pub fn with_cap(root: PathBuf, cap: u64) -> std::io::Result<Self> {
        let used = logical_bytes(&root)?;
        Ok(Self(Arc::new(FileStoreState {
            root,
            cap,
            used: std::sync::Mutex::new(used),
        })))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.0.root
    }

    /// Land `data` at `<root>/<16 hex>/<name>`, or leave nothing behind.
    ///
    /// Called from the blocking pool: a session's cache can sit on a
    /// network mount, and a stalled write must not take the tokio worker
    /// serving other connections down with it.
    fn put(&self, name: &str, data: &[u8]) -> Result<PathBuf, HandlerError> {
        let bytes = data.len() as u64;
        let dir = self.admit(bytes)?;
        let path = dir.join(name);
        if let Err(error) = write_upload(&dir, &path, data) {
            // Nothing partial may survive under a path this op never
            // returned, and bytes that never landed are not the next
            // upload's problem — but only once they are actually gone.
            // A cleanup that fails leaves the bytes on disk, and
            // refunding them anyway is the one way this counter can
            // undercount, which is the one way the store can over-admit.
            let message = match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    let mut used = lock(&self.0.used);
                    *used = used.saturating_sub(bytes);
                    format!("could not land {}: {error}", path.display())
                }
                Err(cleanup) => format!(
                    "could not land {}: {error}; and could not remove it: {cleanup} \
                     (its bytes stay charged against the store)",
                    path.display()
                ),
            };
            return Err(HandlerError::new("internal", message));
        }
        Ok(path)
    }

    /// Charge `bytes` against the cap and claim a private directory for
    /// them, both under the one lock that makes over-admission
    /// impossible.
    fn admit(&self, bytes: u64) -> Result<PathBuf, HandlerError> {
        let mut used = lock(&self.0.used);
        let after = used
            .checked_add(bytes)
            .filter(|after| *after <= self.0.cap)
            .ok_or_else(|| {
                HandlerError::new(
                    "store-full",
                    format!(
                        "the host's file store holds {} of its {} byte cap and cannot take \
                         {bytes} more; restart the session to clear it",
                        *used, self.0.cap
                    ),
                )
            })?;
        // One attempt: the name is 64 bits of OS entropy, so a collision
        // is a real error rather than something to retry around.
        let dir = self.0.root.join(crate::workspace::random_hex(8));
        create_private_dir(&dir).map_err(|error| {
            HandlerError::new(
                "internal",
                format!(
                    "could not create the upload directory {}: {error}",
                    dir.display()
                ),
            )
        })?;
        *used = after;
        Ok(dir)
    }
}

impl std::fmt::Debug for FileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileStore")
            .field("root", &self.0.root)
            .field("cap", &self.0.cap)
            .finish_non_exhaustive()
    }
}

/// Sum the regular files under `root`. A root that does not exist holds
/// nothing; anything else that cannot be read is an error, because a
/// store that undercounts what it already holds is a store that
/// over-admits.
fn logical_bytes(root: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            // `DirEntry::metadata` does not follow symlinks, so a link
            // planted in the store counts as the link it is.
            let meta = entry.metadata()?;
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// Write, then rename: a reader must never find a half-written file
/// under the name this op is about to return.
///
/// The temp name carries a `~`, which the op's own name grammar forbids,
/// so it can never be the name being landed. No fsync — the store is
/// cache, swept at every start and every clean stop, and the client can
/// always upload again.
fn write_upload(dir: &Path, path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let temp = dir.join(".part~");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(data)?;
    drop(file);
    std::fs::rename(&temp, path)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new().mode(0o700).create(dir)
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

/// How many of those one control connection may hold at once.
///
/// Half the pool, so no single connection can exhaust it: before R15
/// minting required the lease, which meant only the foreground could
/// reach [`MAX_OUTSTANDING_TOKENS`] at all. Raw input is open now, so
/// any same-UID client can loop `tab.attach` without ever dialing — and
/// without this sub-cap one buggy agent script would answer every other
/// client's attach with `too-many-tokens` for a whole TTL.
///
/// Eight is far above anything healthy: a client consumes each ticket
/// within a round trip, so even a UI attaching several tabs at once
/// holds one or two. Half rather than a smaller share because the
/// interesting property is only that a second connection always has
/// room, and a low cap would start refusing legitimate bursts.
pub const MAX_TOKENS_PER_CONNECTION: usize = MAX_OUTSTANDING_TOKENS / 2;

/// What a takeover reports when the claimant stated no label.
///
/// Display copy, not a sentinel: `taken_by` is always a non-empty string
/// on the wire so a client never has to render "took over by ".
const UNKNOWN_CLIENT: &str = "unknown client";

/// The longest client label a session will keep, in bytes.
const MAX_CLIENT_LABEL: usize = 128;

/// Trim, de-control, and cap a claimant's self-reported label.
///
/// Display metadata, never identity (§3.9) — which is exactly why it is
/// normalized here rather than trusted: it is rendered in a banner, so a
/// label carrying a newline or a kilobyte of text is a UI problem a
/// client should not be able to hand us. Empty after normalization is
/// absent: a label nobody stated must not render as one.
fn normalize_client_label(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let mut label = String::new();
    for c in raw.trim().chars().filter(|c| !is_layout_hostile(*c)) {
        if label.len() + c.len_utf8() > MAX_CLIENT_LABEL {
            break;
        }
        label.push(c);
    }
    let label = label.trim().to_string();
    (!label.is_empty()).then_some(label)
}

/// Characters a banner must never receive, beyond `char::is_control`.
///
/// The line/paragraph separators break the banner onto a second line and
/// the bidi overrides reorder everything after them — neither is a
/// control character by Unicode's definition, so `is_control` alone
/// lets both straight through into a string this session renders.
fn is_layout_hostile(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// One live `events.subscribe` stream.
///
/// Streams are registered **here and never under [`ClientRegistry::controls`]**
/// (plan 049 §3.7), because the two kinds are handled differently at
/// both events that reach them: a stop closes a stream and only then
/// aborts its relay, and a takeover demotes a stream and tells it who
/// took over rather than touching it. Keeping them in separate lists is
/// what makes that structural instead of a branch somebody has to
/// remember. Since R15 (plan 057) a takeover touches no control or data
/// connection either.
struct Observer {
    conn_id: u64,
    closer: ConnCloser,
    /// Writes one non-batch envelope into this stream's push queue,
    /// behind whatever is already queued. Weak on purpose — see
    /// [`event_push::Subscription::inject`].
    inject: tokio::sync::mpsc::WeakSender<serde_json::Value>,
    /// Ends the relay; its dropped sender is what EOFs the peer.
    relay: tokio::task::AbortHandle,
    /// The lease this stream presented, `None` for one that presented
    /// none — and `None` again once a takeover demoted it. Delivery
    /// classifies off the same `current` under this same lock, so this
    /// view and that one cannot disagree.
    presented: Option<String>,
    /// A `session.driver_changed` the queue would not take, waiting for
    /// this stream's own relay to send it (plan 049 §3.8).
    ///
    /// The slot exists because a full queue does not mean a stalled
    /// peer. The relay reserves capacity *before* it takes the registry
    /// lock, so a stream that is draining perfectly reports `Full` to
    /// the injector for exactly as long as its relay is parked on that
    /// lock — and cutting it there would turn a healthy reader into a
    /// bare EOF. Left here instead, [`LeaseGate`] finds it under the
    /// same lock and spends its reserved permit on the notice first.
    /// A peer that genuinely stopped reading still dies, on the relay's
    /// own stall budget.
    ///
    /// One slot, not a queue: a second takeover's envelope names the
    /// current holder, which is the more useful answer than the one it
    /// replaces, and a stream that has not been told once has no order
    /// to preserve.
    notice: Option<serde_json::Value>,
}

impl Observer {
    /// Still worth keeping a record for: the relay is running and the
    /// connection it writes to is open. A stream that ended on its own
    /// satisfies neither, which is what the prunes retain on.
    fn is_live(&self) -> bool {
        !self.relay.is_finished() && !self.closer.is_closed()
    }
}

/// The client registry: one live lease, one tombstone, one entry per
/// live control connection, one entry per live event stream, at most
/// [`MAX_OUTSTANDING_TOKENS`] unconsumed tokens, and **every** live data
/// connection per tab.
///
/// Data connections are not bounded by construction any more (plan 057,
/// R15): a tab admits as many as clients dial. What bounds them is the
/// token quota — [`MAX_OUTSTANDING_TOKENS`] tickets per TTL window,
/// [`MAX_TOKENS_PER_CONNECTION`] of them per connection — and
/// the tab task's `MAX_CONCURRENT_SNAPSHOTS` simultaneous fences (named
/// rather than linked: that module is `server-vt`-gated and this one is
/// not); over time the count is open. That is affordable because nothing is
/// shared between forwarders: each takes its own broadcast receiver,
/// fence and budgets, so a reader that falls behind is cut on its own
/// lag and takes nobody with it.
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
    /// Every live control connection, keyed by conn id.
    ///
    /// **The authority for closing**, held independently of any lease.
    /// A takeover closes nothing, so the connections a displaced lease
    /// was held on outlive it — and a stop must still be able to hand
    /// each of them the labeled `shutting-down` close. [`Lease::conns`]
    /// is membership and nothing else.
    ///
    /// Every connection that sends a single op on this socket is in
    /// here, not only the ones that present a lease: since R15 a client
    /// that only attaches and writes never mints one, and it is owed the
    /// same labeled goodbye as the foreground.
    controls: std::collections::HashMap<u64, ConnCloser>,
    /// Every live data connection, by tab id. Kept so a stop can close
    /// them and so a forwarder unwinding can drop its own entry — no
    /// supersede, no bound: a tab serves as many attaches as clients
    /// dial.
    data_conns: std::collections::HashMap<i64, Vec<(u64, ConnCloser)>>,
    /// Every live event stream. `None` once a stop has swept them: a
    /// subscribe that raced the sweep is refused rather than registered
    /// into a list nobody will read again.
    ///
    /// A connection that flipped to push mode never finishes on its own
    /// — nothing on it is request-shaped any more — so a stop has to
    /// reach in, close it (which writes the terminal envelope) and only
    /// then abort its relay.
    observers: Option<Vec<Observer>>,
}

impl Default for ClientRegistry {
    fn default() -> Self {
        Self {
            current: None,
            tombstone: None,
            tokens: Vec::new(),
            controls: std::collections::HashMap::new(),
            data_conns: std::collections::HashMap::new(),
            observers: Some(Vec::new()),
        }
    }
}

/// One single-use attach ticket. Bound to the exact tab pipeline it
/// describes, so a respawn between `tab.attach` and the handshake cannot
/// be papered over, and to the connection that minted it — which is what
/// bounds the quota (see [`AttachToken::minted_by`]).
///
/// Read only by the `server-vt` attach path; a default build (a UI
/// binary built without `roost-session` in its graph) compiles the
/// registry but never consumes tickets, hence the gated allow.
#[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
struct AttachToken {
    token: String,
    /// The control connection that asked for this ticket.
    ///
    /// Two things read it, and they cover the two ways one client could
    /// hold the pool against the others. `mint_token` counts a
    /// connection's own live tickets against
    /// [`MAX_TOKENS_PER_CONNECTION`], which is what bounds a **live**
    /// client that mints and never dials. `forget_connection` purges on
    /// it, which is what releases a **vanished** one's tickets instead
    /// of leaving them to time out — the connection-scoped replacement
    /// for the lease-scoped purge a takeover used to do, since takeovers
    /// no longer invalidate tickets and an attach takes no lease.
    minted_by: u64,
    tab_id: i64,
    tab_generation: u64,
    terms: AttachTerms,
    expires_at: std::time::Instant,
}

/// What `tab.attach` settled on, carried to the forwarder.
///
/// The data connection presents only a token, so everything the control
/// op decided has to ride the ticket: re-deriving any of it on the data
/// side would let the two answers drift.
#[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct AttachTerms {
    /// The negotiated payload kind — the encode and the handshake reply
    /// both have to name it.
    pub(crate) kind: AttachPayloadKind,
    /// The client's declared geometry: what the tab was resized to on a
    /// focused attach, and what every `INPUT` frame from this connection
    /// claims (plan 057, R15). A `RESIZE` frame moves it.
    pub(crate) geometry: Geometry,
}

/// What consuming a token admitted, handed to the forwarder.
#[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct AdmittedAttach {
    pub(crate) tab_id: i64,
    pub(crate) tab_generation: u64,
    pub(crate) terms: AttachTerms,
}

struct Lease {
    token: String,
    /// Which connections have presented this lease — membership only.
    /// The closers live in [`ClientRegistry::controls`], which outlives
    /// the lease.
    conns: Vec<u64>,
    /// What the claimant said it was, normalized. Never authenticated —
    /// it exists so a deposed client's banner can name whoever took the
    /// session, and it travels no further than
    /// [`SessionDriverChangedEvent::taken_by`].
    label: Option<String>,
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
    /// `takeover` moves the **foreground** and nothing else (plan 057,
    /// R15): the previous lease is invalidated and tombstoned, so its
    /// holder's foreground ops answer `taken-over`, but no connection is
    /// closed and no attach ticket is revoked. The displaced client
    /// keeps typing, keeps attaching, keeps reading.
    ///
    /// Event streams get the one thing a takeover does emit, which is
    /// the whole of plan 049 §3.8: `session.driver_changed` naming the
    /// claimant, plus a demotion of the displaced driver's stream. Both
    /// happen here, under the registry lock delivery also classifies
    /// under — which is what makes "no `tab.effect` after
    /// `session.driver_changed` on one stream" an invariant rather than
    /// a race.
    fn connect(
        &mut self,
        takeover: bool,
        label: Option<String>,
        ctx: &ConnCtx,
    ) -> Result<String, HandlerError> {
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
        let mut displaced = None;
        if let Some(previous) = self.current.take() {
            self.tombstone = Some(previous.token.clone());
            displaced = Some((previous.token, previous.label));
        }
        let token = random_hex_128();
        let taken_by = label.clone().unwrap_or_else(|| UNKNOWN_CLIENT.to_string());
        self.register_control(ctx);
        self.current = Some(Lease {
            token: token.clone(),
            conns: vec![ctx.conn_id],
            label,
        });
        if let Some((displaced, from)) = displaced {
            // The one place both labels exist at once, and the only
            // reason a session keeps the holder's: an operator reading
            // the log wants "who lost it to whom", which no single
            // event carries. Both are normalized display metadata —
            // neither is a credential and neither is authenticated.
            tracing::info!(
                from = from.as_deref().unwrap_or(UNKNOWN_CLIENT),
                to = taken_by,
                "the session lease changed hands"
            );
            self.announce_takeover(&displaced, &taken_by);
        }
        Ok(token)
    }

    /// Demote the displaced lease's streams and tell **every** stream who
    /// took over.
    ///
    /// Every one, not just the deposed driver's: an observer that was
    /// already watching has the same question ("who drives this now?")
    /// and the same reason to want the answer. Injection order is
    /// registration order, and consecutive takeovers are serialized by
    /// this lock, so a stream reads them in the order they happened.
    ///
    /// A stream whose queue is full at this instant is **not** cut: the
    /// envelope is parked on its [`Observer::notice`] slot and its own
    /// relay sends it, ahead of whatever batch that relay was holding a
    /// permit for. Reserved capacity is not backpressure, and a takeover
    /// still never waits on anybody — a peer that has really stopped
    /// reading dies on the relay's stall budget instead, with the bare
    /// EOF that has always been event backpressure's resync signal.
    fn announce_takeover(&mut self, displaced: &str, taken_by: &str) {
        let envelope = match serde_json::to_value(SessionDriverChangedEvent {
            taken_by: taken_by.to_string(),
        })
        .and_then(|data| {
            serde_json::to_value(EventEnvelope {
                event: SESSION_DRIVER_CHANGED_EVENT.to_string(),
                data,
            })
        }) {
            Ok(value) => value,
            // Not reachable: the payload is one owned String. Ending
            // every stream over it would be a worse answer than leaving
            // them un-notified, since the client converges through the
            // prologue either way.
            Err(error) => {
                tracing::warn!(%error, "the driver-changed envelope could not be serialized");
                return;
            }
        };
        let Some(observers) = self.observers.as_mut() else {
            return;
        };
        for observer in observers.iter_mut() {
            if observer.presented.as_deref() == Some(displaced) {
                observer.presented = None;
            }
            let Some(queue) = observer.inject.upgrade() else {
                // The relay is already gone; its EOF is the signal.
                continue;
            };
            match queue.try_send(envelope.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(envelope)) => {
                    tracing::debug!(
                        conn_id = observer.conn_id,
                        "an events subscriber's queue was full; its relay delivers the takeover"
                    );
                    observer.notice = Some(envelope);
                }
                // The receiver is gone: this connection is already on
                // its way down and the relay's own `tx.closed()` arm
                // ends it.
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    /// The takeover envelope this stream's relay owes its peer, if one
    /// was parked on it. See [`Observer::notice`].
    fn take_notice(&mut self, conn_id: u64) -> Option<serde_json::Value> {
        self.observers
            .as_mut()?
            .iter_mut()
            .find(|observer| observer.conn_id == conn_id)?
            .notice
            .take()
    }

    /// Register one event stream. `false` once a stop has swept.
    fn register_stream(
        &mut self,
        lease: &str,
        ctx: &ConnCtx,
        inject: tokio::sync::mpsc::WeakSender<serde_json::Value>,
        relay: tokio::task::AbortHandle,
    ) -> bool {
        let Some(observers) = self.observers.as_mut() else {
            return false;
        };
        // Pruned here, as `present` and `forget_connection` prune the
        // lease's own connections: this list is only ever walked here
        // and at a takeover or a stop, so a subscriber that finished on
        // its own goes away on somebody else's subscribe.
        observers.retain(Observer::is_live);
        observers.push(Observer {
            conn_id: ctx.conn_id,
            closer: ctx.closer.clone(),
            inject,
            relay,
            presented: (!lease.is_empty()).then(|| lease.to_string()),
            notice: None,
        });
        true
    }

    /// Is `lease` the live one? The read a stream's delivery classifies
    /// on — no registration, no error, just the fact.
    fn is_driver(&self, lease: &str) -> bool {
        !lease.is_empty()
            && self
                .current
                .as_ref()
                .is_some_and(|current| current.token == lease)
    }

    /// End every live relay and refuse further ones. Called *after*
    /// [`Self::close_all`], never before: a relay aborted first would
    /// drop its sender, the push loop's source would end, and the peer
    /// would get an unlabeled EOF instead of `session.stopping`.
    fn abort_streams(&mut self) {
        for observer in self.observers.take().into_iter().flatten() {
            observer.relay.abort();
        }
    }

    /// Resolve a presented lease, registering the presenting connection
    /// when it is the live one.
    fn present(&mut self, lease: &str, ctx: &ConnCtx) -> LeaseStatus {
        if self
            .current
            .as_ref()
            .is_some_and(|current| !lease.is_empty() && current.token == lease)
        {
            self.register_control(ctx);
            let controls = &self.controls;
            let current = self
                .current
                .as_mut()
                .expect("the live lease was just matched under this lock");
            // Pruned here, as in `forget_connection`: a client that
            // reconnects repeatedly on the same lease would otherwise
            // accumulate a member id per dead connection.
            current.conns.retain(|id| controls.contains_key(id));
            if !current.conns.contains(&ctx.conn_id) {
                current.conns.push(ctx.conn_id);
            }
            return LeaseStatus::Current;
        }
        if !lease.is_empty() && self.tombstone.as_deref() == Some(lease) {
            return LeaseStatus::TakenOver;
        }
        LeaseStatus::Unknown
    }

    /// Track a control connection's closer, independently of whatever
    /// authority it just presented — or never presented at all.
    ///
    /// [`ClientRegistry::controls`] is the authority for closing and
    /// [`Lease::conns`] is membership, which is why the registration is
    /// here and not on the lease: a connection admitted under a lease
    /// that is later taken over keeps being closable, and a stop still
    /// owes it a labeled goodbye.
    ///
    /// Closed peers are pruned on the way in, the way this list is
    /// walked: only on a registration, a close, and a stop.
    fn register_control(&mut self, ctx: &ConnCtx) {
        self.controls.retain(|_, closer| !closer.is_closed());
        self.controls.insert(ctx.conn_id, ctx.closer.clone());
    }

    /// Forget one connection.
    ///
    /// Closed peers are pruned on the way through, like [`Self::present`]
    /// does: a client that dropped two connections at once must not
    /// leave the second one standing in for a holder that is gone.
    fn forget_connection(&mut self, conn_id: u64, reclaim_tokens: bool) {
        // Ahead of the lease's own bookkeeping and outside it: a stream,
        // a control connection or an attach ticket can exist on a
        // session that never minted a lease at all, so none of these may
        // sit under an early return that asks about one.
        if let Some(observers) = self.observers.as_mut() {
            observers.retain(|observer| observer.conn_id != conn_id && observer.is_live());
        }
        self.controls.remove(&conn_id);
        self.controls.retain(|_, closer| !closer.is_closed());
        // See [`AttachToken::minted_by`]: a client that minted the whole
        // quota and vanished must not hold it against everyone else
        // until the tickets time out.
        if reclaim_tokens {
            self.tokens.retain(|token| token.minted_by != conn_id);
        }
        let controls = &self.controls;
        if let Some(current) = self.current.as_mut() {
            current
                .conns
                .retain(|id| *id != conn_id && controls.contains_key(id));
        }
    }

    /// Close every registered connection, and stop tracking them. The
    /// lease itself stays: nothing after a stop is admissible anyway, and
    /// keeping it means a late op is refused as `shutting-down` rather
    /// than as a lease problem it cannot fix.
    fn close_all(&mut self, reason: CloseReason) {
        for (_, closer) in self.controls.drain() {
            closer.close(reason);
        }
        if let Some(current) = self.current.as_mut() {
            // Membership only; the closers were in `controls` above.
            current.conns.clear();
        }
        // Walked in its own right, not as a side effect of the lease's
        // list: a data connection is admitted by a ticket, not by a
        // lease, so a session that never minted one can still have
        // several — and each is owed the same labeled close.
        for (_, conns) in self.data_conns.drain() {
            for (_, closer) in conns {
                closer.close(reason);
            }
        }
        // Independent of the lease, for the same reason
        // `forget_connection` is: a session can have observers and have
        // never minted one. The records stay — [`Self::abort_streams`]
        // takes them, after every closer above has fired.
        for observer in self.observers.iter().flatten() {
            observer.closer.close(reason);
        }
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
        minted_by: u64,
        tab_id: i64,
        tab_generation: u64,
        terms: AttachTerms,
        ttl: Duration,
    ) -> Result<String, HandlerError> {
        let now = std::time::Instant::now();
        self.tokens.retain(|t| t.expires_at > now);
        if self.tokens.len() >= MAX_OUTSTANDING_TOKENS {
            return Err(HandlerError::new(
                "too-many-tokens",
                format!(
                    "{MAX_OUTSTANDING_TOKENS} attach tokens are already outstanding; \
                     dial the data connections you asked for"
                ),
            ));
        }
        // The per-connection share, checked second so the session-wide
        // answer stays the one a client hears when the session really is
        // full. See [`MAX_TOKENS_PER_CONNECTION`].
        if self
            .tokens
            .iter()
            .filter(|t| t.minted_by == minted_by)
            .count()
            >= MAX_TOKENS_PER_CONNECTION
        {
            return Err(HandlerError::new(
                "too-many-tokens",
                format!(
                    "this connection already holds {MAX_TOKENS_PER_CONNECTION} attach tokens; \
                     dial the data connections you asked for"
                ),
            ));
        }
        let token = random_hex_128();
        self.tokens.push(AttachToken {
            token: token.clone(),
            minted_by,
            tab_id,
            tab_generation,
            terms,
            expires_at: now + ttl,
        });
        Ok(token)
    }

    /// Consume a token and register `ctx` among the tab's data
    /// connections.
    ///
    /// The whole admission is one step under one lock — consume, stop
    /// latch, register — so two connections presenting the same token
    /// produce exactly one forwarder.
    ///
    /// The order of the two refusals is the contract, not an accident:
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
    ) -> Result<AdmittedAttach, HandlerError> {
        let now = std::time::Instant::now();
        self.tokens.retain(|t| t.expires_at > now);
        let Some(index) = self.tokens.iter().position(|t| t.token == token) else {
            return Err(HandlerError::new(
                "invalid-token",
                "unknown, expired, revoked, or already-used attach token",
            ));
        };
        let ticket = self.tokens.remove(index);
        // Checked only once the ticket is known good, and still under
        // this lock: the stop latches first and sweeps this registry
        // second, so a data connection admitted past the latch but
        // registered after the sweep would be one no closer can reach.
        if stopping {
            return Err(shutting_down());
        }
        let conns = self.data_conns.entry(ticket.tab_id).or_default();
        conns.retain(|(id, closer)| *id != ctx.conn_id && !closer.is_closed());
        conns.push((ctx.conn_id, ctx.closer.clone()));
        Ok(AdmittedAttach {
            tab_id: ticket.tab_id,
            tab_generation: ticket.tab_generation,
            terms: ticket.terms,
        })
    }

    /// Drop this connection's entry from a tab's data connections, and
    /// the tab's list with it once nothing is attached — the index must
    /// not keep an entry alive per tab that ever attached.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn release_data_conn(&mut self, tab_id: i64, conn_id: u64) {
        let std::collections::hash_map::Entry::Occupied(mut entry) = self.data_conns.entry(tab_id)
        else {
            return;
        };
        entry.get_mut().retain(|(id, _)| *id != conn_id);
        if entry.get().is_empty() {
            entry.remove();
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
    /// Register a live event stream, or report that the session is
    /// already stopping.
    ///
    /// The check and the registration share the registry lock the stop
    /// sweep takes, which is what makes them atomic: a stream handed out
    /// after the sweep would be one no closer can reach and no abort can
    /// end.
    fn register_stream(
        &self,
        lease: &str,
        ctx: &ConnCtx,
        inject: tokio::sync::mpsc::WeakSender<serde_json::Value>,
        relay: tokio::task::AbortHandle,
    ) -> bool {
        let mut guard = lock(&self.clients);
        if self.stopping.load(Ordering::Acquire) {
            return false;
        }
        guard.register_stream(lease, ctx, inject, relay)
    }

    /// End every live relay and refuse further ones.
    fn abort_streams(&self) {
        lock(&self.clients).abort_streams();
    }

    /// Mint or take over the interactive lease for `ctx`'s connection.
    fn connect(
        &self,
        takeover: bool,
        label: Option<String>,
        ctx: &ConnCtx,
    ) -> Result<String, HandlerError> {
        let mut guard = lock(&self.clients);
        // Re-checked UNDER the registry lock: the stop latches first and
        // sweeps this registry second, so a connect that was admitted
        // past the latch but reaches the registry after the sweep must
        // be refused here — a lease minted post-sweep would be authority
        // no closer can ever revoke.
        if self.stopping.load(Ordering::Acquire) {
            return Err(shutting_down());
        }
        guard.connect(takeover, label, ctx)
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

    /// Note this connection as a live control connection, whatever it is
    /// about to ask for.
    ///
    /// The one choke point, called from [`Handler::handle`] before any
    /// dispatch, because since plan 057 R15 a control connection that
    /// never presents a lease is ordinary: a client that only attaches,
    /// writes and lists is first-class and would otherwise appear in
    /// none of `controls`, `data_conns` or `observers` — so a stop could
    /// only give it a bare EOF, and a client that distinguishes "the
    /// session stopped" from "the wire died" would re-dial a socket
    /// being unlinked.
    ///
    /// Registration is refused after the stop sweep for
    /// [`Self::require_lease`]'s reason: an entry added past the sweep
    /// is one no closer will ever reach.
    fn register_control(&self, ctx: &ConnCtx) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        lock(&self.clients).register_control(ctx);
    }

    /// Is `lease` still the live one?
    ///
    /// A read, with none of [`Self::require_lease`]'s registration: it is
    /// asked from the thread doing an op's *work*, where the question is
    /// "may this still take effect", not "count me as a connection". A
    /// stop counts as a loss of authority for the same reason it refuses
    /// mutations — a session that has flushed and reaped must not gain
    /// new entries pointing at the socket it is about to unlink.
    fn holds_lease(&self, lease: &str) -> bool {
        if lease.is_empty() || self.stopping.load(Ordering::Acquire) {
            return false;
        }
        lock(&self.clients)
            .current
            .as_ref()
            .is_some_and(|current| current.token == lease)
    }

    /// One connection has ended.
    fn forget_connection(&self, conn_id: u64) {
        // The quota is not reclaimed during a stop, which is also when
        // every control connection is closed at once: `close_all` keeps
        // the tokens deliberately so a client holding a good pre-stop
        // ticket hears `shutting-down` instead of being sent hunting for
        // a bad credential, and reclaiming here would undo exactly that.
        let stopping = self.stopping.load(Ordering::Acquire);
        lock(&self.clients).forget_connection(conn_id, !stopping);
    }

    /// Tell every connection the lease holder owns why it is going away.
    fn close_clients(&self, reason: CloseReason) {
        lock(&self.clients).close_all(reason);
    }

    /// Mint one attach ticket. The caller has already passed the stop
    /// latch; it is re-checked under this lock, because a ticket is
    /// authority and authority minted after a sweep is authority nobody
    /// can revoke.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn mint_attach_token(
        &self,
        ctx: &ConnCtx,
        tab_id: i64,
        tab_generation: u64,
        terms: AttachTerms,
    ) -> Result<String, HandlerError> {
        let mut guard = lock(&self.clients);
        if self.stopping.load(Ordering::Acquire) {
            return Err(shutting_down());
        }
        guard.mint_token(
            ctx.conn_id,
            tab_id,
            tab_generation,
            terms,
            self.attach_token_ttl(),
        )
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
        guard.admit_attach(token, ctx, stopping)
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
            // Not workspace state, but authority-bearing all the same:
            // it writes hook entries into the session user's dotfiles,
            // pointing them at a `roostctl` that reports to a socket
            // this session is about to unlink. Listed so a stop latches
            // it out along with everything else that changes the world.
            | ops::SESSION_SET_AGENT_HOOKS
            // Also not workspace state, and listed for the same reason:
            // it writes a file into the host's store and hands the path
            // back to be pasted. A stop that has already swept that
            // store would leave the client holding a path to nothing.
            | ops::SESSION_PUT_FILE
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
    /// Set by the host-session daemon: what `session.set_agent_hooks`
    /// actually does. `None` everywhere else, and the op answers
    /// `not-supported` — see [`AgentHooksHandle`] for why this crate
    /// holds a callback rather than the install engine itself.
    agent_hooks: Option<AgentHooksHandle>,
    /// Set by the host-session daemon: where `session.put_file` lands
    /// what a client uploads. `None` everywhere else, and the op answers
    /// `not-supported` — the daemon owns the directory because the
    /// daemon is what sweeps it.
    files: Option<FileStore>,
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
            agent_hooks: None,
            files: None,
        }
    }

    /// Teach a session socket how to wire the host's agent hooks.
    ///
    /// Separate from [`Self::with_session`] because the two answer
    /// different questions — "is this a session?" and "can this session
    /// write the user's dotfiles?" — and only the daemon can answer the
    /// second. A session built without it serves the op as
    /// `not-supported` rather than pretending it wired anything.
    #[must_use]
    pub fn with_agent_hooks(mut self, handle: AgentHooksHandle) -> Self {
        self.agent_hooks = Some(handle);
        self
    }

    /// Give a session socket somewhere to land uploaded files.
    ///
    /// Separate from [`Self::with_session`] for the reason
    /// [`Self::with_agent_hooks`] is: only the daemon knows a directory
    /// it also sweeps, and a session built without one serves
    /// `session.put_file` as `not-supported` rather than writing
    /// somewhere nothing will ever clean up.
    #[must_use]
    pub fn with_file_store(mut self, store: FileStore) -> Self {
        self.files = Some(store);
        self
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
        Box::pin(async move {
            // Every op on a session socket passes through here, which is
            // why the registration is here: see
            // [`SessionState::register_control`] for why a leaseless
            // connection has to be tracked too.
            if let Some(session) = self.session.as_ref() {
                session.register_control(ctx);
            }
            dispatch_outcome(self, ctx, op, params).await
        })
    }

    /// The other half of `session.set_focus`'s lifetime rule: a focus a
    /// client reported is only true while that client is still there.
    /// Only this connection's statement is retired — everyone else is
    /// still looking at whatever they said they were. A UI socket has no
    /// session registry and does nothing here.
    ///
    /// Streams are pruned here too, and independently of the lease: an
    /// observer can exist on a session where no lease was ever minted —
    /// as can a control connection holding attach tickets, which this is
    /// also where the registry reclaims.
    fn connection_ended(&self, conn_id: u64) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        session.forget_connection(conn_id);
        self.workspace.forget_client_focus(conn_id);
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
        TabError::SnapshotFailed(_) | TabError::Render(_) | TabError::WinsizeFailed(_) => {
            HandlerError::new("internal", e.to_string())
        }
    }
}

/// Why a focused [`tab_attach`] could not size the tab, told apart by
/// whose problem it is.
///
/// A grid the server terminal refused is the caller's `cols`/`rows` to
/// fix, and that is the only half of a resize that is. The child's half
/// is not: [`TabError::WinsizeFailed`] covers a `TIOCSWINSZ` that
/// failed and a child that did not take the size within the budget
/// alike, and neither says anything about the parameters — calling
/// those `invalid-param` would make a wedged shell look like a
/// permanent client error and send every retry of a perfectly good
/// attach off to fix a geometry that was never wrong. [`tab_err`] has
/// always called that `internal`; the two paths answer for the same
/// error and must not disagree about it.
#[cfg(feature = "server-vt")]
fn attach_resize_refusal(
    tab_id: i64,
    cols: u16,
    rows: u16,
    error: &crate::tab_task::TabError,
) -> HandlerError {
    use crate::tab_task::TabError;
    let code = match error {
        TabError::WinsizeFailed(_) => "internal",
        _ => "invalid-param",
    };
    HandlerError::new(
        code,
        format!("tab {tab_id} could not be resized to {cols}x{rows}: {error}"),
    )
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

/// A request only an app can serve. `what` is the request as the
/// refusal names it (a whole op, or just its host-qualified form) and
/// `because` is what the app has that this socket does not.
fn needs_a_ui(what: &str, because: &str) -> HandlerError {
    HandlerError::invalid_param(format!("{what} needs a UI: {because}"))
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
                return Err(needs_a_ui(
                    &format!("a host-qualified {op}"),
                    "host connections are client state",
                ));
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
        return Err(needs_a_ui(
            &format!("a host-qualified {op}"),
            "host connections are client state",
        ));
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
        scrollback: u32,
    ) -> Option<Result<DumpData, HandlerError>> {
        h.session.as_ref()?;
        Some(match bare(tab) {
            Ok(tab_id) => tab_ask(h, tab_id, |reply| TabCmd::Dump { scrollback, reply }).await,
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
        _: u32,
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
                features: SESSION_FEATURES.iter().map(|f| (*f).to_string()).collect(),
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
        let lease = session.connect(p.takeover, normalize_client_label(p.client_label), ctx)?;
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

    // Connection-scoped like its neighbours, for the same reason: the
    // lease rides in the params and belongs to *this* connection.
    if op == ops::SESSION_SET_AGENT_HOOKS {
        let p: SessionSetAgentHooksParams = decode(params)?;
        return session_set_agent_hooks(h, session, ctx, p)
            .await
            .map(HandlerOutcome::Reply);
    }

    // Connection-scoped like its neighbours, and guarded before the
    // decode: see [`put_file_size_guard`].
    if op == ops::SESSION_PUT_FILE {
        put_file_size_guard(&params)?;
        let p: SessionPutFileParams = decode(params)?;
        return session_put_file(h, session, ctx, p)
            .await
            .map(HandlerOutcome::Reply);
    }

    // Served here rather than in `dispatch` because the ticket it mints
    // is bound to *this* connection — that is what lets the connection's
    // close revoke the tickets it holds — and `dispatch` cannot see one.
    if op == ops::TAB_ATTACH {
        let p: TabAttachParams = decode(params)?;
        return tab_attach(h, session, ctx, p)
            .await
            .map(HandlerOutcome::Reply);
    }

    // Raw input is open to every same-UID client (plan 057, R15): the
    // presented lease is accepted and ignored here exactly as it is on a
    // UI socket. What the lease still owns is the foreground — effects,
    // focus, and the session-wide settings ops — never a keystroke.
    if op == ops::TAB_WRITE {
        let p: TabWriteParams = decode(params)?;
        h.supervisor
            .write(p.tab_id, p.data)
            .await
            .map_err(|e| pty_err(&e))?;
        return Ok(HandlerOutcome::Reply(serde_json::json!({})));
    }

    dispatch(h, op, params).await.map(HandlerOutcome::Reply)
}

/// `tab.attach`: negotiate a payload kind and hand back a single-use
/// ticket for one data connection.
///
/// An attach is raw input, so it takes no lease (plan 057, R15): any
/// same-UID client may attach, and a tab serves as many data connections
/// as are dialed. A presented `lease` is accepted and ignored.
///
/// The validation order is pinned (D5) and each earlier failure wins,
/// because the codes instruct differently: `not-found` means "that tab
/// is gone", `unsupported-kind` means "offer something else",
/// `build-mismatch` means "the offer we could serve needs the same
/// libghostty on both ends", and only then does geometry get looked at.
/// Reordering would tell a client to fix the wrong thing.
#[cfg(feature = "server-vt")]
async fn tab_attach(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: TabAttachParams,
) -> Result<serde_json::Value, HandlerError> {
    // One lookup, so the channel this resizes, the generation the token
    // is stamped with, and the task the forwarder will snapshot are the
    // same pipeline — two reads could straddle a respawn.
    let (commands, tab_generation) = h.supervisor.tab_task_handle(p.tab_id).ok_or_else(|| {
        HandlerError::not_found(format!("tab {} has no live terminal to attach", p.tab_id))
    })?;

    // A list mixing kinds this build has never heard of with ones it
    // serves is fine — the client states a preference order and the
    // first entry that is both *servable* and *eligible* wins.
    //
    // Servable is what `session.identify` ADVERTISED
    // (`payload_kinds`): the advertisement is the contract a client
    // negotiated against, so a kind absent from it must not be accepted
    // even when the code could produce it. Eligible is the kind's own
    // requirement, which only GHOSTSNP has — it is libghostty's binary
    // state, so both ends must be the same build.
    //
    // The two refusals stay separate because they instruct differently.
    // Nothing servable at all is "offer something else"; servable but
    // ineligible is "the two builds disagree", which is the answer a
    // pre-`vt` client's whole restart flow hangs off. Splitting the walk
    // in two is what keeps them apart: a client offering
    // `[ghostty-snapshot, vt]` across a skew must land on `vt` rather
    // than on either refusal.
    let servable: Vec<&AttachPayloadKind> = p
        .kinds
        .iter()
        .filter(|kind| session.info.payload_kinds.contains(kind))
        .collect();
    if servable.is_empty() {
        return Err(HandlerError::new(
            "unsupported-kind",
            format!(
                "this session serves {:?}; the client offered {:?}",
                session.info.payload_kinds, p.kinds
            ),
        ));
    }
    let builds_match = p.libghostty_build == session.info.libghostty_build;
    let mut eligible = None;
    for kind in servable {
        let holds = match kind.as_str() {
            AttachPayloadKind::GHOSTTY_SNAPSHOT => builds_match,
            // `vt` is a byte stream any VT parser replays, so the build
            // it was encoded against is not this gate's business.
            AttachPayloadKind::VT => true,
            // Advertised by this session and unknown to this code, which
            // can only be a misconfigured advertisement. Refused rather
            // than waved through: "no requirement" is the answer for a
            // kind whose requirement is *known* to be none, and guessing
            // it for an unknown one is how a build-skewed client gets
            // served GHOSTSNP under another name.
            other => {
                return Err(HandlerError::new(
                    "internal",
                    format!("this session advertises {other:?}, which it cannot serve"),
                ))
            }
        };
        if holds {
            eligible = Some(kind.clone());
            break;
        }
    }
    // Exact match, both strings named: a client that sees only
    // "mismatch" cannot tell which side to upgrade.
    let kind = eligible.ok_or_else(|| {
        HandlerError::new(
            "build-mismatch",
            format!(
                "this session is {:?}; the client is {:?}",
                session.info.libghostty_build, p.libghostty_build
            ),
        )
    })?;

    // Zero cell pixels are legal — a headless client has no cell metrics
    // to report — but a zero-sized grid is not a grid. Checked for an
    // unfocused attach too: it is still this connection's declared
    // geometry, which its first INPUT or RESIZE frame applies.
    if p.cols == 0 || p.rows == 0 {
        return Err(HandlerError::invalid_param(format!(
            "cols and rows must both be non-zero (got {}x{})",
            p.cols, p.rows
        )));
    }
    let geometry = Geometry {
        cols: p.cols,
        rows: p.rows,
        cell_w: u32::from(p.cell_w_px),
        cell_h: u32::from(p.cell_h_px),
    };

    // A focused attach is a geometry-bearing interaction, so the tab
    // takes the client's size now rather than at first frame: a snapshot
    // encoded at the old size would be re-laid-out on the client the
    // instant it resized. Detach never resizes back (roadmap D7). An
    // unfocused one resizes nothing — a client that is only watching
    // must not shrink the one that is typing — and the reply tells it
    // the size the snapshot was encoded at instead.
    //
    // Awaited, not fired and forgotten: the ticket minted below is the
    // client's authority to snapshot this tab, and a `Resize` still
    // sitting on the command channel would let that snapshot be encoded
    // at the geometry the attach exists to replace.
    if p.focus {
        let (resized_tx, resized_rx) = tokio::sync::oneshot::channel();
        commands
            .send(crate::tab_task::TabCmd::Resize {
                geometry,
                ack: Some(resized_tx),
            })
            .await
            .map_err(|_| tab_gone(p.tab_id))?;
        resized_rx
            .await
            // The task dropped the ack without answering, which only
            // happens when the task itself is going away.
            .map_err(|_| tab_gone(p.tab_id))?
            .map_err(|error| attach_resize_refusal(p.tab_id, p.cols, p.rows, &error))?;
    }

    let attach_token = session.mint_attach_token(
        ctx,
        p.tab_id,
        tab_generation,
        AttachTerms {
            kind: kind.clone(),
            geometry,
        },
    )?;
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
    _session: &Arc<SessionState>,
    _ctx: &ConnCtx,
    _p: TabAttachParams,
) -> Result<serde_json::Value, HandlerError> {
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

/// `session.set_focus`: take one connected client's real focus (plan 038
/// §C6).
///
/// A session's workspace has no window of its own, so the only thing
/// that can say a tab is being looked at is a client that does — and
/// several may be, each at a different tab, which is why the statement
/// is keyed by connection ([`Workspace::set_client_focus`]).
///
/// Unlike its two neighbours there is no `server-vt` twin: nothing here
/// touches a server terminal, and a featureless build's notification
/// routing is the same routing.
fn session_set_focus(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: &SessionSetFocusParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;
    h.workspace
        .set_client_focus(ctx.conn_id, p.focused_tab_id)
        .map_err(ws_err)?;
    Ok(serde_json::json!({}))
}

/// `session.set_agent_hooks`: bring the host's agent hook entries in
/// line with the connected client's config (plan 046 §3.4).
///
/// Everything this function does is admission. The work — reading and
/// rewriting five agents' config files under the session user's `$HOME`
/// — belongs to the daemon, which is the only process here that links
/// the install engine; this crate only decides *whether* it may run.
///
/// Lease-gated because it writes authority-bearing files on the host,
/// and in [`is_mutating_op`] because a session that has latched
/// `session.stop` has already flushed and reaped: entries pointing at a
/// socket about to be unlinked are worse than no entries at all.
///
/// A per-agent install failure is a *reported* failure, never an error
/// frame: the reply's `errors` list carries it, so a client hears which
/// agent broke and still keeps the session it just attached to. Only a
/// whole-run failure — no `$HOME`, an unwritable record, a lock another
/// writer never released — is an error frame.
///
/// **The lease is checked twice, and the second one is the real one.**
/// `require_lease` here is the door; the install engine can then sit
/// behind another writer's `flock` for seconds, and neither closing the
/// client's connection nor its own 15 s timeout cancels the handler that
/// is already running. So the credential travels on as an
/// [`AgentHooksAuthority`] the backend re-asks at the point of effect —
/// once it owns the lock, before it plans. Without that, a client that
/// had *lost* the lease could still rewrite the host's files and the
/// state record afterwards, undoing the policy of whoever displaced it.
/// Two clients that each legitimately hold the lease in turn are
/// last-writer-wins by design (plan 046 §3.4); one acting after it lost
/// the lease is not.
async fn session_set_agent_hooks(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: SessionSetAgentHooksParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;
    let handle = h.agent_hooks.as_ref().ok_or_else(|| {
        HandlerError::new(
            "not-supported",
            "this session cannot wire agent hooks: it was built without an install backend",
        )
    })?;
    let authority = {
        let session = Arc::clone(session);
        let lease = p.lease.clone();
        AgentHooksAuthority::new(move || session.holds_lease(&lease))
    };
    let result = handle
        .run(AgentHooksRequest {
            mode: p.mode,
            skip: p.skip,
            client: p.client,
            authority,
        })
        .await
        .map_err(|error| match error {
            // The same code any other lease-gated op would answer this
            // client with now, so a client that hears it reacts the one
            // documented way: stop driving this session.
            AgentHooksError::Unauthorized => HandlerError::new("taken-over", error.to_string()),
            AgentHooksError::Failed(_) => HandlerError::new("internal", error.to_string()),
        })?;
    encode(&result)
}

/// `session.put_file`: land one client-supplied file on the host and
/// answer with the path a shell can be told to read (plan 047 §3.1).
///
/// Lease-gated because the file is written under the session user's
/// `$HOME` and its path is about to be typed into one of this session's
/// tabs, and in [`is_mutating_op`] because a session that has latched
/// `session.stop` is about to sweep the very directory this writes into.
/// A slow write therefore holds the mutation barrier and a racing stop
/// waits for it — the price of never handing back a path that is already
/// gone.
async fn session_put_file(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    ctx: &ConnCtx,
    p: SessionPutFileParams,
) -> Result<serde_json::Value, HandlerError> {
    session.require_lease(&p.lease, ctx)?;
    let store = h.files.clone().ok_or_else(|| {
        HandlerError::new(
            "not-supported",
            "this session cannot receive files: it was built without a file store",
        )
    })?;
    validate_put_file_name(&p.name)?;
    let bytes = p.data.len() as u64;
    // The exact check the pre-decode guard cannot make.
    if bytes > MAX_PUT_FILE_BYTES {
        return Err(too_large());
    }

    let SessionPutFileParams { name, data, .. } = p;
    let path = tokio::task::spawn_blocking(move || store.put(&name, &data))
        .await
        .map_err(|error| HandlerError::new("internal", format!("the upload failed: {error}")))??;

    encode(&SessionPutFileResult {
        // Lossy is not a repair here: the root came from a session that
        // checked it against the paste grammar, and `name` just passed
        // `validate_put_file_name`.
        path: path.to_string_lossy().into_owned(),
        bytes,
    })
}

/// Refuse an oversized upload before the typed decode allocates it.
///
/// The frame and its `serde_json::Value` already exist by the time this
/// runs; what the early check spares is the decoded `Vec<u8>`. It is
/// deliberately coarse: base64 carries three bytes per four characters,
/// so a file one or two bytes over the cap encodes to exactly the same
/// length as the cap itself and is caught by the exact check after the
/// decode instead. Both answer `too-large`.
fn put_file_size_guard(params: &serde_json::Value) -> Result<(), HandlerError> {
    // A `data` that is absent or not a string is a shape error, and
    // `decode` names it far better than this can.
    let Some(encoded) = params.get("data").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    if encoded.len() as u64 > MAX_PUT_FILE_BYTES.div_ceil(3) * 4 {
        return Err(too_large());
    }
    Ok(())
}

fn too_large() -> HandlerError {
    HandlerError::new(
        "too-large",
        format!(
            "this file is over the {} byte cap on one upload",
            MAX_PUT_FILE_BYTES
        ),
    )
}

/// The client's basename, taken or refused — never repaired.
///
/// The path this op returns has to be pasteable **bare**, which is the
/// one spelling every agent unquotes identically (plan 047 §3.1), so a
/// name that would need quoting, or that could climb out of the upload
/// directory, is not a name this store can land. `-` leading is out
/// because a bare path is also an argv word.
fn validate_put_file_name(name: &str) -> Result<(), HandlerError> {
    let refuse = |why: &str| Err(HandlerError::invalid_param(format!("name {name:?} {why}")));
    if name.is_empty() {
        return refuse("must not be empty");
    }
    if name.len() > MAX_PUT_FILE_NAME_BYTES {
        return refuse(&format!("is longer than {MAX_PUT_FILE_NAME_BYTES} bytes"));
    }
    if name == "." || name == ".." {
        return refuse("is a directory reference, not a file name");
    }
    if name.starts_with('-') {
        return refuse("must not start with '-'");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return refuse("must be a bare basename of [A-Za-z0-9._-]");
    }
    Ok(())
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

/// One subscription's classifier (plan 049 §3.7).
///
/// It holds the lease the stream *presented* — an immutable fact for the
/// life of the stream — and compares it against `current` at the instant
/// each batch is enqueued. Reclassification therefore needs no write:
/// a takeover replaces `current` with a freshly minted token, and this
/// comparison stops matching in the same critical section.
struct LeaseGate {
    session: Arc<SessionState>,
    presented: String,
    /// Which stream this is, so the gate can pick up a takeover notice
    /// the injector had to park — see [`Observer::notice`].
    conn_id: u64,
}

impl event_push::StreamGate for LeaseGate {
    fn deliver(
        &self,
        permit: tokio::sync::mpsc::Permit<'_, serde_json::Value>,
        batch: &crate::VersionedWorkspaceEvent,
    ) -> event_push::Delivery {
        // The lock is the whole point. A takeover demotes this stream
        // and injects `session.driver_changed` in this same critical
        // section, so an effect batch is either enqueued *before* the
        // envelope (correct — the reader was still the driver) or
        // classified after it and filtered (correct — it no longer is).
        // There is no third interleaving, which is what makes "no
        // tab.effect after driver_changed" an invariant.
        //
        // Nothing is awaited here: the queue slot was reserved before
        // this call, so the send cannot block.
        let mut guard = lock(&self.session.clients);
        // Ahead of the batch, always: the reservation this permit came
        // from is what made the queue look full to the injector, and
        // sending the batch first would put a driver-classified
        // `tab.effect` after the announcement on the same stream.
        if let Some(notice) = guard.take_notice(self.conn_id) {
            permit.send(notice);
            return event_push::Delivery::NoticeSentRetryBatch;
        }
        let driver = guard.is_driver(&self.presented);
        let Some(value) = event_push::batch_value(batch, driver) else {
            return event_push::Delivery::End;
        };
        permit.send(value);
        event_push::Delivery::Delivered
    }
}

/// Where this subscription starts: the current revision, or the replay
/// a `from_revision` asked for.
///
/// Every refusal is named rather than folded onto `invalid-param`,
/// because each one asks the client for something different: re-fence
/// (`session-mismatch`), snapshot (`replay-expired`), or fix a bug
/// (`revision-ahead`).
fn resume_cut(
    h: &IpcHandler,
    session: &Arc<SessionState>,
    params: &EventsSubscribeParams,
) -> Result<ResumeCut, HandlerError> {
    if let Some(named) = &params.session_id {
        if *named != session.info.session_id {
            return Err(HandlerError::new(
                "session-mismatch",
                format!(
                    "this is session {}, not {named}: revisions restart with the process, so a \
                     fence from another incarnation cannot be resumed; snapshot with tab.list and \
                     subscribe afresh",
                    session.info.session_id
                ),
            ));
        }
    }
    let Some(from_revision) = params.from_revision else {
        return Ok(h.workspace.subscribe_live());
    };
    if params.session_id.is_none() {
        return Err(HandlerError::invalid_param(
            "a resume names the session it fenced against: from_revision requires session_id, \
             which session.identify reports",
        ));
    }
    h.workspace
        .subscribe_from(from_revision)
        .map_err(|err| match err {
            ResumeError::Ahead { current } => HandlerError::new(
                "revision-ahead",
                format!(
                    "this session never produced revision {from_revision} (current: {current}): a \
                 different incarnation, or a client bug; compare session.identify.session_id, \
                 then snapshot"
                ),
            ),
            ResumeError::Expired {
                oldest_resumable_from,
                current,
            } => HandlerError::new(
                "replay-expired",
                format!(
                    "revision {from_revision} is outside the replay window (oldest resumable: \
                 {oldest_resumable_from}, current: {current}); snapshot with tab.list and \
                 subscribe afresh"
                ),
            ),
        })
}

/// `events.subscribe` on a session socket: ack with the fence, then push.
///
/// **Leaseless, and lease-classified** (plan 049 §3.7). Reading a session
/// is not authority, so the op no longer gates — but the lease still
/// means something: it decides *what* this stream sees.
///
/// * `lease` present and current → the driver stream, today's full feed,
///   `tab.effect` included (DL-18: effects belong to the client driving).
/// * absent, stale, or unknown → an observer stream: every workspace
///   batch plus `notification.fired`, with `tab.effect` filtered and its
///   revision still delivered as an empty batch.
///
/// Classification is the ordinary one on a resume too, for replayed and
/// live batches alike: a driver taken over during its gap comes back an
/// observer, and since effects are never replayed, a replay cannot put a
/// `tab.effect` after the `session.driver_changed` it missed.
///
/// Not a mutating op — it changes no workspace state — but it does
/// establish a resource, so it is refused once the session has latched:
/// a stream handed out after the stop swept the registry would be one
/// nobody can end. [`SessionState::register_stream`] closes the race by
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
    // Everything a resume can be refused for is settled before anything
    // is spawned or registered, so a refusal leaves the connection the
    // request/response connection it was and the client can simply
    // subscribe again on it.
    let cut = resume_cut(h, session, params)?;
    let gate = Arc::new(LeaseGate {
        session: Arc::clone(session),
        presented: params.lease.clone(),
        conn_id: ctx.conn_id,
    });
    let subscription = event_push::spawn(cut, h.push_limits, gate);
    if !session.register_stream(
        &params.lease,
        ctx,
        subscription.inject,
        subscription.abort.clone(),
    ) {
        // Lost the race with the stop's sweep. Abort what we just
        // started rather than leaking a relay the stop will never see.
        subscription.abort.abort();
        return Err(shutting_down());
    }
    Ok(HandlerOutcome::ReplyThen {
        reply: encode(&EventsSubscribeResult {
            revision: subscription.revision,
        })?,
        then: ConnAction::StartPush(subscription.source),
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
    session.abort_streams();

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
            crate::application::spawn_for_row(
                &h.workspace,
                &h.supervisor,
                &tab,
                &p.argv,
                cols,
                rows,
                &h.socket_path,
            )
            .map_err(|err| match err.downcast_ref::<PtyError>() {
                Some(pty) => pty_err(pty),
                None => HandlerError::new("internal", format!("pty spawn failed: {err}")),
            })?;
            encode(&TabOpenResult { tab })
        }
        ops::TAB_CLOSE => {
            let p: TabCloseParams = decode(params)?;
            crate::application::close_tab(&h.workspace, &h.supervisor, p.tab_id).map_err(ws_err)?;
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
                .map_err(|e| pty_err(&e))?;
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
                .map_err(|e| pty_err(&e))?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_DUMP => {
            let p: TabDumpParams = decode(params)?;
            // Clamped, never refused: a client asking for "everything"
            // does not know the tab's retention. Once here, before the
            // paths split, so the session and UI sockets share one
            // ceiling.
            let scrollback = p.scrollback.min(MAX_DUMP_SCROLLBACK);
            let data = match served::dump(h, p.tab_id, scrollback).await {
                Some(served) => served?,
                None => h
                    .ui_call(|reply| UiRequest::Dump {
                        tab_id: p.tab_id,
                        scrollback,
                        reply,
                    })
                    .await?
                    .map_err(dump_err)?,
            };
            encode(&TabDumpResult {
                cols: data.cols,
                rows: data.rows,
                cursor: data
                    .cursor
                    .map(|(row, col, visible)| TabDumpCursor { row, col, visible }),
                rows_text: data.rows_text,
                scrollback_rows: data.scrollback_rows,
                scrollback_text: data.scrollback_text,
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
                        return Err(needs_a_ui(
                            "a host-qualified tab.focus",
                            "host selection is client state",
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
        ops::TAB_SEND_FILE => {
            let p: TabSendFileParams = decode(params)?;
            // Only what is decidable with no client state at all: the
            // ref spelling and the fact that there is an app. The paths
            // are the app's to judge, because §3.4's precedence puts
            // `not-found` and `host-unavailable` ahead of a relative
            // path and only the app knows those two.
            let tab = WireTabRef::parse(&p.tab).ok_or_else(|| {
                HandlerError::invalid_param(format!("invalid tab reference: {}", p.tab))
            })?;
            if !h.has_ui() {
                return Err(needs_a_ui(
                    ops::TAB_SEND_FILE,
                    "reading the files and typing the paste are the app's",
                ));
            }
            let result = h
                .ui_call(|reply| UiRequest::TabSendFile {
                    tab,
                    paths: p.paths,
                    reply,
                })
                .await??;
            encode(&result)
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
            // The wire allows `text` OR `image_png` — exactly one. Both
            // at once is ambiguous, and answering it by silently
            // preferring `text` would drop an image the caller believed
            // it had written.
            if p.text.is_some() && p.image_png.is_some() {
                return Err(HandlerError::new(
                    "invalid-param",
                    "clipboard.write takes `text` or `image_png`, not both",
                ));
            }
            if let Some(png) = p.image_png {
                // PRIMARY carries text by convention and the paste path
                // never probes it for an image (`probe_wanted`), so a
                // selection image write would seed a clipboard nothing
                // reads. Refused here rather than in the UI so the
                // headless dispatcher answers it too.
                if target != ClipboardOp::System {
                    return Err(HandlerError::new(
                        "invalid-param",
                        "clipboard.write `image_png` requires target \"system\"",
                    ));
                }
                h.ui_call(|reply| UiRequest::ClipboardWriteImage { png, reply })
                    .await??;
                return Ok(serde_json::json!({}));
            }
            let text = p.text.ok_or_else(|| {
                HandlerError::new("missing-param", "clipboard.write requires `text`")
            })?;
            // Fire-and-forget — matches the `app.activate` pattern.
            // Headless handler / dropped receiver: no-op.
            if let Some(tx) = &h.ui_tx {
                let _ = tx.send(UiRequest::ClipboardWrite { target, text });
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

/// The UI half of the `tab.dump` contract, mapped onto the codes the
/// session half already gives: see [`DumpError`].
#[allow(clippy::needless_pass_by_value)] // `Result::map_err` adapter owns its error.
fn dump_err(e: DumpError) -> HandlerError {
    match e {
        DumpError::NoTab(msg) => HandlerError::not_found(msg),
        DumpError::Read(msg) => HandlerError::new("internal", msg),
    }
}

fn pty_err(e: &PtyError) -> HandlerError {
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
            clients: std::sync::Mutex::new(ClientRegistry::default()),
        }
    }

    fn live_streams(state: &SessionState) -> usize {
        lock(&state.clients).observers.as_ref().map_or(0, Vec::len)
    }

    /// A stream registration with a throwaway queue: what these cases
    /// are about is the *registry*, not delivery, so the sender is
    /// dropped immediately and only the weak handle is kept.
    fn register(
        state: &SessionState,
        conn_id: u64,
        lease: &str,
        relay: tokio::task::AbortHandle,
    ) -> bool {
        let (ctx, _watch) = ConnCtx::new(conn_id);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let weak = tx.downgrade();
        state.register_stream(lease, &ctx, weak, relay)
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
        assert!(register(&state, 1, "", stale));

        let parked = tokio::spawn(std::future::pending::<()>());
        assert!(register(&state, 2, "", parked.abort_handle()));
        assert_eq!(
            live_streams(&state),
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
        assert!(register(&state, 1, "", parked.abort_handle()));

        state.abort_streams();
        assert!(
            parked.await.expect_err("aborted").is_cancelled(),
            "the sweep must actually end the relay"
        );

        let late = tokio::spawn(std::future::pending::<()>());
        assert!(
            !register(&state, 2, "", late.abort_handle()),
            "a subscribe after the sweep must be refused"
        );
        late.abort();
    }

    /// The other half of the same race, and the one that only the
    /// registry lock can settle: the latch is set before the sweep
    /// runs, so a subscribe admitted past the latch must still be
    /// refused — a stream registered after the sweep is one no closer
    /// can reach.
    #[tokio::test]
    async fn a_subscribe_that_raced_the_stop_latch_is_refused() {
        let state = session_state();
        state.stopping.store(true, Ordering::Release);

        let late = tokio::spawn(std::future::pending::<()>());
        assert!(
            !register(&state, 1, "", late.abort_handle()),
            "the latch is checked under the sweep's own lock"
        );
        assert_eq!(live_streams(&state), 0);
        late.abort();
    }

    /// Observers are pruned independently of the lease. A session that
    /// never minted one still has to clean up after a subscriber that
    /// went away, and the lease's own early return must not sit in
    /// front of that.
    #[tokio::test]
    async fn a_stream_is_pruned_with_no_lease_ever_minted() {
        let state = session_state();
        let parked = tokio::spawn(std::future::pending::<()>());
        let (ctx, _watch) = ConnCtx::new(7);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(state.register_stream("", &ctx, tx.downgrade(), parked.abort_handle()));
        assert!(lock(&state.clients).current.is_none());

        state.forget_connection(7);
        assert_eq!(live_streams(&state), 0);
        parked.abort();
    }

    /// Normalization is the whole of the label contract (§3.9): a
    /// banner renders this, so a client cannot hand us a newline, a
    /// kilobyte, or whitespace pretending to be a name.
    #[test]
    fn a_client_label_is_trimmed_capped_and_de_controlled() {
        assert_eq!(normalize_client_label(None), None);
        assert_eq!(normalize_client_label(Some("   ".into())), None);
        assert_eq!(normalize_client_label(Some(String::new())), None);
        assert_eq!(
            normalize_client_label(Some("  pop-os  ".into())).as_deref(),
            Some("pop-os")
        );
        assert_eq!(
            normalize_client_label(Some("pop\nos\u{7}".into())).as_deref(),
            Some("popos"),
            "control characters are dropped, not escaped"
        );
        assert_eq!(
            normalize_client_label(Some("\u{7}  pop-os".into())).as_deref(),
            Some("pop-os"),
            "whitespace uncovered by a dropped control character is trimmed too"
        );
        assert_eq!(
            normalize_client_label(Some("pop\u{202e}o\u{2028}s".into())).as_deref(),
            Some("popos"),
            "a bidi override reorders the banner and a line separator splits it; \
             neither is `is_control`, so both are dropped by name"
        );
        // A label that is nothing but control characters is no label.
        assert_eq!(normalize_client_label(Some("\u{0}\u{1}".into())), None);

        let long = normalize_client_label(Some("é".repeat(200))).expect("a capped label");
        assert!(
            long.len() <= MAX_CLIENT_LABEL,
            "capped in bytes: {}",
            long.len()
        );
        assert!(
            std::str::from_utf8(long.as_bytes()).is_ok() && long.chars().all(|c| c == 'é'),
            "the cap must land on a character boundary"
        );
        assert_eq!(long.chars().count(), MAX_CLIENT_LABEL / 2);
    }

    /// Admission is atomic, not merely capped: the check and the charge
    /// happen under one lock. A split (read the counter, then lock and
    /// write) passes the two-connection wire test whenever tokio happens
    /// to serialise it; this drives real contention through a barrier
    /// and demands *exactly* `cap` winners, every time.
    #[test]
    fn admission_never_over_admits_under_contention() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().expect("tempdir");
        const CAP: u64 = 64;
        const THREADS: u64 = 4 * CAP;
        let store = FileStore::with_cap(dir.path().to_path_buf(), CAP).expect("store");
        let barrier = Arc::new(Barrier::new(THREADS as usize));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let store = store.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.admit(1).is_ok()
                })
            })
            .collect();
        // Every thread is spawned before any is joined, or the barrier
        // never releases.
        let mut admitted = 0u64;
        for handle in handles {
            if handle.join().expect("thread") {
                admitted += 1;
            }
        }

        assert_eq!(admitted, CAP, "exactly the cap may be admitted");
        assert_eq!(*lock(&store.0.used), CAP);
        let dirs = std::fs::read_dir(dir.path()).expect("read").count() as u64;
        assert_eq!(
            dirs, CAP,
            "one directory per admitted upload, none for a refusal"
        );
    }

    /// The store admits against what its root **already** holds, not
    /// against zero: a session restarted over a root it did not sweep
    /// would otherwise hand out room twice.
    #[test]
    fn a_store_counts_what_its_root_already_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("files");
        std::fs::create_dir_all(root.join("aa")).expect("seed a directory");
        std::fs::write(root.join("aa/one.bin"), [0u8; 10]).expect("seed a file");
        std::fs::write(root.join("aa/two.bin"), [0u8; 6]).expect("seed a file");

        let store = FileStore::with_cap(root, 20).expect("open the store");
        assert_eq!(*lock(&store.0.used), 16);
        store
            .put("three.bin", &[0u8; 4])
            .expect("4 more fits exactly");
        let error = store
            .put("four.bin", &[0u8; 1])
            .expect_err("and nothing after that");
        assert_eq!(error.code, "store-full");
    }

    /// A write that cannot land leaves nothing behind — no directory,
    /// and no charge against the room the next upload has to fit in.
    ///
    /// Forced with the one failure a test can arrange *after* the upload
    /// directory already exists: a final path over the OS's `PATH_MAX`,
    /// with the directory and its temp file comfortably inside it. The
    /// shorter name landing afterwards is what proves the refund.
    #[test]
    fn a_write_that_fails_leaves_no_directory_and_no_charge() {
        #[cfg(target_os = "macos")]
        const PATH_MAX: usize = 1024;
        #[cfg(not(target_os = "macos"))]
        const PATH_MAX: usize = 4096;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut root = dir.path().to_path_buf();
        let target = PATH_MAX - 60;
        while root.as_os_str().len() + 101 <= target {
            root.push("d".repeat(100));
        }
        let pad = target.saturating_sub(root.as_os_str().len() + 1);
        if pad > 0 {
            root.push("d".repeat(pad));
        }
        std::fs::create_dir_all(&root).expect("create the deep root");

        let store = FileStore::with_cap(root.clone(), 1024).expect("open the store");
        let error = store
            .put(&"n".repeat(MAX_PUT_FILE_NAME_BYTES), b"payload")
            .expect_err("the final path is over PATH_MAX");
        assert_eq!(error.code, "internal");
        assert!(
            std::fs::read_dir(&root)
                .expect("read the root")
                .next()
                .is_none(),
            "a failed write must leave no directory"
        );
        assert_eq!(*lock(&store.0.used), 0, "and no charge");

        let landed = store.put("ok.bin", b"payload").expect("a short name lands");
        assert_eq!(std::fs::read(&landed).expect("read it back"), b"payload");
        assert_eq!(*lock(&store.0.used), 7);
    }

    /// A focused `tab.attach` that could not size the tab says whose
    /// problem it was (review F2).
    ///
    /// The two halves fail for unrelated reasons and only one of them is
    /// the caller's. `WinsizeFailed` is the child's — a `TIOCSWINSZ`
    /// that failed, or a shell wedged past the ack budget — and a client
    /// told `invalid-param` there would go hunting for a fault in a
    /// `cols`/`rows` that was never wrong, then see every retry of the
    /// same legitimate attach refused the same permanent-sounding way.
    #[cfg(feature = "server-vt")]
    #[test]
    fn an_attach_resize_refusal_blames_the_geometry_only_when_the_terminal_refused_it() {
        use crate::tab_task::TabError;

        let child = attach_resize_refusal(
            7,
            80,
            24,
            &TabError::WinsizeFailed(
                "the child did not take the new size within the budget".into(),
            ),
        );
        assert_eq!(
            child.code, "internal",
            "a wedged child is not a parameter the caller can fix"
        );
        assert_eq!(
            child.code,
            tab_err(TabError::WinsizeFailed("ioctl".into())).code,
            "and the two paths that answer for this error must agree"
        );
        assert!(
            child
                .message
                .contains("tab 7 could not be resized to 80x24"),
            "the message still names what was attempted: {}",
            child.message
        );

        let refused = attach_resize_refusal(7, 0, 24, &TabError::Render("cols must be > 0".into()));
        assert_eq!(
            refused.code, "invalid-param",
            "a grid the terminal itself refused is the caller's to fix"
        );
        assert!(refused.message.contains("cols must be > 0"));
    }

    /// The screen in front of the typed decode: coarse by construction
    /// (base64 carries three bytes per four characters), and silent
    /// about anything that is not an over-long string, because `decode`
    /// names those failures far better.
    #[test]
    fn the_size_guard_screens_the_encoded_length_and_defers_the_rest() {
        let data = |len: usize| serde_json::json!({ "data": "A".repeat(len) });
        let cap = usize::try_from(MAX_PUT_FILE_BYTES).unwrap();
        let encoded_cap = cap.div_ceil(3) * 4;

        put_file_size_guard(&data(encoded_cap)).expect("the cap's own encoding fits");
        assert_eq!(
            put_file_size_guard(&data(encoded_cap + 1))
                .expect_err("one character more does not")
                .code,
            "too-large"
        );
        put_file_size_guard(&serde_json::json!({})).expect("a missing `data` is decode's to name");
        put_file_size_guard(&serde_json::json!({"data": 7}))
            .expect("and so is one of the wrong type");
    }

    /// `tab.open`'s lost-row answer, pinned here because the race it
    /// reports cannot be interposed over a socket.
    #[test]
    fn a_cancelled_spawn_is_not_found_on_the_wire() {
        assert_eq!(pty_err(&PtyError::Cancelled(7)).code, "not-found");
    }
}
