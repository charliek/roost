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
    ops, AgentHooksOutcome, AgentSetHooksAgents, AgentSetHooksParams, AgentSetHooksResult,
    AppActivateParams, AppActiveTerminalFocusedParams, AppActiveTerminalFocusedResult,
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
    ProjectCreateResult, ProjectDeleteParams, ProjectEnsureParams, ProjectEnsureResult,
    ProjectRenameParams, ProjectReorderParams, ResolvedCell, ScreenshotParams, ScreenshotResult,
    SelectionClearParams, SelectionDumpParams, SelectionDumpResult, SelectionSetParams,
    SessionIdentify, SessionIdentifyParams, SessionPutFileParams, SessionPutFileResult,
    SessionSetAgentHooksParams, SessionSetThemeParams, SessionStopParams, SessionStopResult,
    SidebarDumpParams, SidebarDumpResult, SidebarSetWidthParams, TabAgentReportResult,
    TabCapturePtyInputParams, TabCapturePtyInputResult, TabClearNotificationParams,
    TabClearNotificationResult, TabCloseParams, TabDispatchMouseEventParams, TabDumpCursor,
    TabDumpParams, TabDumpResolvedParams, TabDumpResolvedResult, TabDumpResult,
    TabExpandSelectionAtParams, TabExpandSelectionAtResult, TabFeedImeParams,
    TabFeedPtyBytesParams, TabFocusParams, TabFocusResult, TabListResult, TabOpenParams,
    TabOpenResult, TabReorderParams, TabResizeParams, TabSendFileParams, TabSendFileResult,
    TabSetHookActiveParams, TabSetStateParams, TabSetTitleParams, TabWriteParams,
    WindowMetricsParams, WindowMetricsResult, WindowResizeParams, WireProjectRef, WireTabRef,
    MAX_DUMP_SCROLLBACK, MAX_PUT_FILE_BYTES, SESSION_PROTOCOL_VERSION,
};
#[cfg(feature = "server-vt")]
use roost_ipc::messages::{AttachHandshake, SessionSetThemeResult};
use roost_ipc::{
    CloseReason, ConnAction, ConnCloser, ConnCtx, Handler, HandlerError, HandlerOutcome,
    LocalBackendCell, LocalBackendMode, LocalRoute, StopFinalizer,
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
    /// is `"confirm" | "cancel"`, or `"toggle:<agent>"` for one of the
    /// agent-hooks card's switches. Gated like `AppDialogDump`.
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
    /// `agent.set_hooks` — set *this* machine's own `agent-hooks` key
    /// and raise every connected non-localhost host to at least the
    /// same allow-list (plan 064 §3.4).
    ///
    /// The engine decodes the op and answers with what the app reports;
    /// it never touches the install engine itself, for
    /// [`AgentHooksHandle`]'s reason — this crate is linked into the UI
    /// processes, and a UI has no business carrying a dotfile writer.
    ///
    /// Like [`UiRequest::HostTabReorder`] this cannot be answered inside
    /// `update`: the install is file I/O under an advisory lock and the
    /// host raises are round trips. The reply travels with the work and
    /// is answered from wherever it ends.
    AgentSetHooks {
        agents: AgentSetHooksAgents,
        reply: HostOpReply<AgentSetHooksResult>,
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
    /// One whole request, put to *the slot* and answered from the
    /// slot's own reply (plan 063 §D10).
    ///
    /// The `op` is a bare-id workspace op this socket would otherwise
    /// have answered against its own workspace — which under
    /// `local-backend = session` is the one the window does not draw.
    /// Answering it here would not merely land in the wrong workspace:
    /// `tab.open` begins with `ensure_default_project`, so it would
    /// *create* a project in there to land in. `params` cross verbatim:
    /// bare ids mean the slot's ids, so there is nothing to translate,
    /// and `tab.open`'s `project_id: 0` gets the slot's default project
    /// exactly as a client on the session socket would.
    ///
    /// Like [`UiRequest::HostTabReorder`] this cannot be answered inside
    /// `update` — the app has to await a session — so the reply travels
    /// with the dispatch and is answered from its completion.
    LocalSessionForward {
        op: String,
        params: serde_json::Value,
        reply: HostOpReply<serde_json::Value>,
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
    /// ↻ Reconnect, as an op. It displaces nobody, and it may start a
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
// Only the attach path names a geometry, and that whole path is
// `server-vt`: a default build (a UI binary with no `roost-session` in
// its graph) compiles neither the admission nor the forwarder.
#[cfg(feature = "server-vt")]
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
    /// (`tab.feed_pty_bytes`, `tab.capture_pty_input`).
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
/// decodes the op, and the *daemon* — which is the process that links
/// `roost-agent-install` and owns the `$HOME` being written — supplies
/// the doing. `roost-engine` is linked into the UI processes too, and a
/// UI has no business carrying a dotfile writer.
///
/// A handler built without one answers `not-supported`, which is the
/// honest answer for any socket that is not a host session's.
#[derive(Clone)]
pub struct AgentHooksHandle(Arc<dyn Fn(AgentHooksRequest) -> AgentHooksFuture + Send + Sync>);

/// The op's params, as the daemon receives them.
///
/// `agents` is the allow-list the client's own `agent-hooks` key names —
/// what this host's `agent-hooks` key is raised (unioned) to, never
/// lowered (plan 064 §3.3). There is no `off` here: a client whose own
/// key is `off` or unconfigured has nothing to raise the host with, so
/// it never sends this op at all.
///
/// Unvalidated: the agent set lives in the install engine, which this
/// crate deliberately does not link, so the handle's far side refuses a
/// malformed list with [`AgentHooksError::InvalidParam`].
#[derive(Debug, Clone)]
pub struct AgentHooksRequest {
    pub agents: Vec<String>,
    /// How the asking client names itself, for the host's state record.
    pub client: String,
}

/// Why a session could not run an install at all.
#[derive(Debug)]
pub enum AgentHooksError {
    /// The request itself is malformed — an empty `agents`, or a name no
    /// agent answers to — and nothing was written.
    ///
    /// Its own variant rather than a [`Self::Failed`] because the two
    /// instruct differently, and under protocol equality a name this
    /// host does not know can only be a bug in the client: `invalid-param`
    /// says "fix the request", `internal` says "something on the host
    /// broke", and a client told the second would go hunting on the wrong
    /// machine.
    InvalidParam(String),
    /// A whole-run failure: no `$HOME`, an unwritable state record, a lock
    /// another writer never released. A *per-agent* failure is not one —
    /// it rides back in the reply's `errors`.
    Failed(String),
}

impl std::fmt::Display for AgentHooksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentHooksError::InvalidParam(error) | AgentHooksError::Failed(error) => {
                f.write_str(error)
            }
        }
    }
}

type AgentHooksFuture =
    Pin<Box<dyn Future<Output = Result<AgentHooksOutcome, AgentHooksError>> + Send>>;

impl AgentHooksHandle {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn(AgentHooksRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<AgentHooksOutcome, AgentHooksError>> + Send + 'static,
    {
        Self(Arc::new(move |request| Box::pin(f(request))))
    }

    async fn run(&self, request: AgentHooksRequest) -> Result<AgentHooksOutcome, AgentHooksError> {
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
/// decodes `session.put_file`, and the *daemon* — the process that owns
/// the host's cache directory and sweeps it — supplies the root. A
/// handler built without one answers `not-supported`, the honest answer
/// for any socket that is not a host session's.
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
    /// Every connection this session is tracking. One lock, so a stop's
    /// sweep and a registration that raced it cannot both win.
    conns: std::sync::Mutex<Connections>,
}

/// How many live data connections one session serves, across every tab.
///
/// The only bound on the attach registry: the handshake negotiates on
/// the data connection itself, so there is no earlier credential to
/// count. One buggy same-UID client can hold all 32 and lock the others
/// out; that is accepted under the same-UID boundary this socket
/// already draws, and a per-connection share would only move the
/// failure without removing it.
///
/// 32 is far above anything healthy: a UI holds one data connection per
/// tab it is showing, and a tab is shown by one client at a time.
pub const MAX_DATA_CONNS_PER_SESSION: usize = 32;

/// One live `events.subscribe` subscriber.
///
/// Subscribers are registered **here and never under [`Connections::controls`]**
/// (plan 049 §3.7), because a stop treats the two kinds differently: it
/// closes a subscriber and only then aborts its relay. Keeping them in
/// separate lists is what makes that structural instead of a branch
/// somebody has to remember.
struct Subscriber {
    conn_id: u64,
    closer: ConnCloser,
    /// Ends the relay; its dropped sender is what EOFs the peer.
    relay: tokio::task::AbortHandle,
}

impl Subscriber {
    /// Still worth keeping a record for: the relay is running and the
    /// connection it writes to is open. A subscriber that ended on its own
    /// satisfies neither, which is what the prunes retain on.
    fn is_live(&self) -> bool {
        !self.relay.is_finished() && !self.closer.is_closed()
    }
}

/// Every connection this session is tracking: one entry per live
/// control connection, one per live event subscriber, and up to
/// [`MAX_DATA_CONNS_PER_SESSION`] live data connections, keyed by tab.
///
/// A tab admits as many data connections as clients dial (plan 057,
/// R15); what bounds them is [`MAX_DATA_CONNS_PER_SESSION`] live ones
/// session-wide, and the tab task's `MAX_CONCURRENT_SNAPSHOTS`
/// simultaneous fences (named rather than linked: that module is
/// `server-vt`-gated and this one is not). Over time the count is open.
/// That is affordable because nothing is shared between forwarders:
/// each takes its own broadcast receiver, fence and budgets, so a
/// reader that falls behind is cut on its own lag and takes nobody with
/// it.
struct Connections {
    /// Every live control connection, keyed by conn id — **the
    /// authority for closing**. Every connection that sends a single op
    /// on this socket is in here, because a stop owes each of them the
    /// labeled `shutting-down` close rather than a bare EOF.
    controls: std::collections::HashMap<u64, ConnCloser>,
    /// Every live data connection, by tab id. Kept so a stop can close
    /// them and so a forwarder unwinding can drop its own entry — no
    /// supersede, no bound: a tab serves as many attaches as clients
    /// dial.
    data_conns: std::collections::HashMap<i64, Vec<(u64, ConnCloser)>>,
    /// Every live event subscriber. `None` once a stop has swept them: a
    /// subscribe that raced the sweep is refused rather than registered
    /// into a list nobody will read again.
    ///
    /// A connection that flipped to push mode never finishes on its own
    /// — nothing on it is request-shaped any more — so a stop has to
    /// reach in, close it (which writes the terminal envelope) and only
    /// then abort its relay.
    subscribers: Option<Vec<Subscriber>>,
}

impl Default for Connections {
    fn default() -> Self {
        Self {
            controls: std::collections::HashMap::new(),
            data_conns: std::collections::HashMap::new(),
            subscribers: Some(Vec::new()),
        }
    }
}

/// What an admission settled, handed to the forwarder.
#[cfg(feature = "server-vt")]
#[derive(Debug, Clone)]
pub(crate) struct AdmittedAttach {
    pub(crate) tab_id: i64,
    pub(crate) tab_generation: u64,
    /// The negotiated payload kind — the encode and the handshake reply
    /// both have to name it.
    pub(crate) kind: AttachPayloadKind,
    /// The client's declared geometry: what the tab is resized to on a
    /// focused attach, and what every `INPUT` frame from this connection
    /// claims (plan 057, R15). A `RESIZE` frame moves it.
    pub(crate) geometry: Geometry,
    /// The tab task the forwarder still owes the focused resize, or
    /// `None` when it owes none.
    ///
    /// The channel is the one the admission read `tab_generation` off,
    /// under the same lock, and never a fresh lookup: a respawn between
    /// the two would send the geometry to the **replacement** tab — and
    /// the generation check that follows then refuses the attach, having
    /// resized somebody else's terminal on the way out. A rejected
    /// operation leaves no side effect.
    pub(crate) resize_first: Option<tokio::sync::mpsc::Sender<crate::tab_task::TabCmd>>,
}

impl Connections {
    /// Register one event subscriber. `false` once a stop has swept.
    fn register_subscriber(&mut self, ctx: &ConnCtx, relay: tokio::task::AbortHandle) -> bool {
        let Some(subscribers) = self.subscribers.as_mut() else {
            return false;
        };
        // Pruned here, as `forget_connection` prunes the controls: this
        // list is only ever walked here and at a stop, so a subscriber
        // that finished on its own goes away on somebody else's
        // subscribe.
        subscribers.retain(Subscriber::is_live);
        subscribers.push(Subscriber {
            conn_id: ctx.conn_id,
            closer: ctx.closer.clone(),
            relay,
        });
        true
    }

    /// End every live relay and refuse further ones. Called *after*
    /// [`Self::close_all`], never before: a relay aborted first would
    /// drop its sender, the push loop's source would end, and the peer
    /// would get an unlabeled EOF instead of `session.stopping`.
    fn abort_subscribers(&mut self) {
        for subscriber in self.subscribers.take().into_iter().flatten() {
            subscriber.relay.abort();
        }
    }

    /// Track a control connection's closer.
    ///
    /// Closed peers are pruned on the way in, the way this list is
    /// walked: only on a registration, a close, and a stop.
    fn register_control(&mut self, ctx: &ConnCtx) {
        self.controls.retain(|_, closer| !closer.is_closed());
        self.controls.insert(ctx.conn_id, ctx.closer.clone());
    }

    /// Forget one connection.
    ///
    /// Closed peers are pruned on the way through: a client that dropped
    /// two connections at once must not leave the second one standing.
    fn forget_connection(&mut self, conn_id: u64) {
        if let Some(subscribers) = self.subscribers.as_mut() {
            subscribers.retain(|subscriber| subscriber.conn_id != conn_id && subscriber.is_live());
        }
        self.controls.remove(&conn_id);
        self.controls.retain(|_, closer| !closer.is_closed());
    }

    /// Close every registered connection, and stop tracking them.
    fn close_all(&mut self, reason: CloseReason) {
        for (_, closer) in self.controls.drain() {
            closer.close(reason);
        }
        // Walked in its own right: a data connection never appears in
        // `controls`, so each list is owed the same labeled close.
        for (_, conns) in self.data_conns.drain() {
            for (_, closer) in conns {
                closer.close(reason);
            }
        }
        // The records stay — [`Self::abort_subscribers`] takes them, after
        // every closer above has fired.
        for subscriber in self.subscribers.iter().flatten() {
            subscriber.closer.close(reason);
        }
    }

    /// The stop latch, the session bound and the registration — the one
    /// step every admission ends in.
    ///
    /// The stop latch is read first and the bound second: a session on
    /// its way out has nothing to serve whatever its occupancy is, and
    /// "we are full" would send that client retrying against a socket
    /// about to be unlinked. Checked under this lock rather than before
    /// it: the stop latches first and sweeps this registry second, so a
    /// data connection admitted past the latch but registered after the
    /// sweep would be one no closer can reach.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn register_data_conn(
        &mut self,
        tab_id: i64,
        ctx: &ConnCtx,
        stopping: bool,
    ) -> Result<(), HandlerError> {
        if stopping {
            return Err(shutting_down());
        }
        // Pruned on the way in, the way every list behind this lock is:
        // a peer that dropped without unwinding its forwarder must not
        // hold a slot against the next client.
        self.data_conns.retain(|_, conns| {
            conns.retain(|(_, closer)| !closer.is_closed());
            !conns.is_empty()
        });
        if self.data_conn_count() >= MAX_DATA_CONNS_PER_SESSION {
            return Err(HandlerError::new(
                "too-many-attaches",
                format!(
                    "this session already serves {MAX_DATA_CONNS_PER_SESSION} data \
                     connections; detach one before attaching again"
                ),
            ));
        }
        let conns = self.data_conns.entry(tab_id).or_default();
        conns.retain(|(id, _)| *id != ctx.conn_id);
        conns.push((ctx.conn_id, ctx.closer.clone()));
        Ok(())
    }

    /// How many data connections are registered right now, across every
    /// tab — the number [`MAX_DATA_CONNS_PER_SESSION`] bounds, and what
    /// a release-on-reject test reads back.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn data_conn_count(&self) -> usize {
        self.data_conns.values().map(Vec::len).sum()
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

impl SessionState {
    /// Register a live event subscriber, or report that the session is
    /// already stopping.
    ///
    /// The check and the registration share the registry lock the stop
    /// sweep takes, which is what makes them atomic: a subscriber handed out
    /// after the sweep would be one no closer can reach and no abort can
    /// end.
    fn register_subscriber(&self, ctx: &ConnCtx, relay: tokio::task::AbortHandle) -> bool {
        let mut guard = lock(&self.conns);
        if self.stopping.load(Ordering::Acquire) {
            return false;
        }
        guard.register_subscriber(ctx, relay)
    }

    /// End every live relay and refuse further ones.
    fn abort_subscribers(&self) {
        lock(&self.conns).abort_subscribers();
    }

    /// Note this connection as a live control connection, whatever it is
    /// about to ask for.
    ///
    /// The one choke point, called from [`Handler::handle`] before any
    /// dispatch: a client that only attaches, writes and lists would
    /// otherwise appear in none of `controls`, `data_conns` or
    /// `subscribers` — so a stop could only give it a bare EOF, and a client
    /// that distinguishes "the session stopped" from "the wire died"
    /// would re-dial a socket being unlinked.
    ///
    /// Registration is refused after the stop sweep: an entry added past
    /// it is one no closer will ever reach.
    fn register_control(&self, ctx: &ConnCtx) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        lock(&self.conns).register_control(ctx);
    }

    /// One connection has ended.
    fn forget_connection(&self, conn_id: u64) {
        lock(&self.conns).forget_connection(conn_id);
    }

    /// Tell every registered connection why it is going away.
    fn close_clients(&self, reason: CloseReason) {
        lock(&self.conns).close_all(reason);
    }

    /// The admission point: the stop latch, the session bound and the
    /// registration, all under the one lock the stop sweep takes. A
    /// connection registered after that sweep is one no closer can
    /// reach, which is why the three are one step.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn register_data_conn(&self, tab_id: i64, ctx: &ConnCtx) -> Result<(), HandlerError> {
        let mut guard = lock(&self.conns);
        let stopping = self.stopping.load(Ordering::Acquire);
        guard.register_data_conn(tab_id, ctx, stopping)
    }

    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn release_data_conn(&self, tab_id: i64, conn_id: u64) {
        lock(&self.conns).release_data_conn(tab_id, conn_id);
    }

    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    fn data_conn_count(&self) -> usize {
        lock(&self.conns).data_conn_count()
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
/// Reads (`identify`, `tab.list`, `tab.dump*`, `session.identify`) stay
/// answerable throughout, so a client can still find out what happened.
/// The UI-only ops (`palette.*`, `window.*`, `app.*`, clipboard,
/// selection) are not listed: they route through `ui_call`, and a
/// session socket has no UI attached, so they already fail with
/// `internal: no UI attached`. `tab.feed_pty_bytes` is the exception —
/// on a session it writes into the tab task's terminal, which is a
/// mutation like any other. This whole set is consulted only on a
/// session socket, so listing it costs a UI nothing.
///
/// Public because plan 063 §D10's forward arm asks the same question of
/// the same set: a forwarded op that changes what the slot holds is the
/// one that has to register with `HostOpsInFlight` and the one a switch
/// in flight refuses. A second list would drift from this one.
pub fn is_mutating_op(op: &str) -> bool {
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
            | ops::PROJECT_ENSURE
            | ops::PROJECT_RENAME
            | ops::PROJECT_DELETE
            | ops::PROJECT_REORDER
            | ops::NOTIFICATION_CREATE
            | ops::SESSION_SET_THEME
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
    /// Set by the running UI: where its own local tabs live (plan 063
    /// §D1). A session daemon never installs one — its tabs are the
    /// session's — so `identify` on a session socket keeps answering
    /// exactly what it always has.
    local_route: Option<Arc<LocalBackendCell>>,
    /// Whether the UI driving this socket was launched with
    /// `ROOST_TEST_MODE=1`, passed in for the reason
    /// [`SessionInfo::test_mode`] is. A session reads its own from there.
    test_mode: bool,
    /// `identify.instance_id`: minted with the handler, which a UI
    /// process builds once. A session never reports it — its identity is
    /// `session_id`.
    instance_id: String,
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
            local_route: None,
            test_mode: false,
            instance_id: mint_instance_id(),
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

    /// Share the UI's local-backend route (plan 063 §D1). The UI writes
    /// the cell on every mode/selection change; this handler only reads
    /// it.
    #[must_use]
    pub fn with_local_route(mut self, cell: Arc<LocalBackendCell>) -> Self {
        self.local_route = Some(cell);
        self
    }

    /// Tell a UI socket whether its app was launched with
    /// `ROOST_TEST_MODE=1`, which decides whether `identify.ops` lists
    /// the gated test seams.
    #[must_use]
    pub fn with_test_mode(mut self, test_mode: bool) -> Self {
        self.test_mode = test_mode;
        self
    }

    /// The current local-backend snapshot, defaulted to in-process
    /// wherever no UI installed a cell.
    fn local_route(&self) -> Arc<LocalRoute> {
        self.local_route
            .as_ref()
            .map(|cell| cell.load())
            .unwrap_or_default()
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
            conns: std::sync::Mutex::new(Connections::default()),
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

    /// `identify.ops` / `session.identify.ops`: [`served_ops`] for this
    /// socket under `route`.
    fn serves(&self, route: &LocalRoute) -> Vec<String> {
        let (socket, test_mode) = match &self.session {
            Some(session) => (SocketKind::Session, session.info.test_mode),
            None => (SocketKind::Ui(route.mode), self.test_mode),
        };
        served_ops(socket, test_mode, self.has_ui())
            .into_iter()
            .map(str::to_string)
            .collect()
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
            // [`SessionState::register_control`] for why every
            // connection has to be tracked.
            if let Some(session) = self.session.as_ref() {
                session.register_control(ctx);
            }
            dispatch_outcome(self, ctx, op, params).await
        })
    }

    /// Retire everything keyed to one connection: its control entry and
    /// its subscriber slot. A UI socket has no session registry and
    /// does nothing here.
    fn connection_ended(&self, conn_id: u64) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        session.forget_connection(conn_id);
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

/// The registry operations the data plane needs. They live on the
/// handler rather than on `SessionState` because the forwarder lives in
/// another module and the registry is this one's private business.
#[cfg(feature = "server-vt")]
impl IpcHandler {
    /// Steps 3–8 of the admission, in the order the codes instruct in:
    /// each earlier failure names a different thing for the client to
    /// fix, so a later one must never mask it. The awaited resize, the
    /// fence and the snapshot are the forwarder's (steps 10–13) —
    /// deliberately outside this function, because nothing after the
    /// registration may run under the registry lock.
    ///
    /// The stop latch is read **twice**: once here, before the tab
    /// lookup, and again under the registry lock at step 8. The second
    /// is what closes the race with the stop sweep and cannot move. The
    /// first is what makes the answer usable: a stop reaps the tabs, so
    /// a latch read only at step 8 would answer a post-stop attach
    /// `not-found`, which a client can reasonably read as "that tab was
    /// deleted" and act on by dropping the tab from its UI — where
    /// `shutting-down` says the whole session went away.
    pub(crate) fn admit_attach(
        &self,
        handshake: &AttachHandshake,
        ctx: &ConnCtx,
    ) -> Result<AdmittedAttach, HandlerError> {
        let session = self.session.as_ref().ok_or_else(|| {
            HandlerError::new(
                "not-supported",
                "this socket does not serve attach data connections",
            )
        })?;
        let attach = handshake.attach.as_str();
        let terms = &handshake.terms;

        // 3. The session the client negotiated with. Without this a
        // dial released from the client's queue after a drop could land
        // on a REPLACEMENT session listening at the same socket path
        // and be served a tab nobody asked for.
        if terms.session_id != session.info.session_id {
            return Err(HandlerError::new(
                "session-mismatch",
                format!(
                    "this session is {:?}; the client attached to {:?}",
                    session.info.session_id, terms.session_id
                ),
            ));
        }

        // 3'. The stop latch, early — see this function's doc.
        if session.stopping.load(Ordering::Acquire) {
            return Err(shutting_down());
        }

        // 4. The tab, as `string_int64` like every other id on this
        // wire. Channel and generation come out of one lookup and are
        // both carried forward: everything this attach does to the tab
        // goes to the pipeline whose generation was admitted, not to
        // whatever the id names by the time it runs.
        let tab_id: i64 = attach
            .parse()
            .map_err(|_| HandlerError::invalid_param(format!("{attach:?} is not a tab id")))?;
        let (commands, tab_generation) =
            self.supervisor.tab_task_handle(tab_id).ok_or_else(|| {
                HandlerError::not_found(format!("tab {tab_id} has no live terminal to attach"))
            })?;

        // 5 + 6. Servable, then eligible.
        let kind = negotiate_kind(&session.info, &terms.kinds, &terms.libghostty_build)?;

        // 7. The grid.
        let geometry = attach_geometry(terms.cols, terms.rows, terms.cell_w_px, terms.cell_h_px)?;

        // 8. The latch, the bound and the registration together.
        session.register_data_conn(tab_id, ctx)?;
        Ok(AdmittedAttach {
            tab_id,
            tab_generation,
            kind,
            geometry,
            resize_first: terms.focus.then_some(commands),
        })
    }

    pub(crate) fn release_data_conn(&self, tab_id: i64, conn_id: u64) {
        if let Some(session) = self.session.as_ref() {
            session.release_data_conn(tab_id, conn_id);
        }
    }

    /// How many data connections this session has registered. Test-only
    /// today: it is the number a rejection after registration has to
    /// return to zero.
    pub fn data_conn_count(&self) -> usize {
        self.session.as_ref().map_or(0, |s| s.data_conn_count())
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
pub(crate) fn tab_gone(tab_id: i64) -> HandlerError {
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
pub(crate) fn attach_resize_refusal(
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
                libghostty_build: session.info.libghostty_build.clone(),
                session_id: session.info.session_id.clone(),
                started_at: session.info.started_at.clone(),
                persist_error: h.workspace.persist_error(),
                ops: Some(h.serves(&h.local_route())),
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
        //
        // `agent.set_hooks` is refused here for the same reason from the
        // other direction: the op sets a machine's own key *and* raises
        // every host that machine is connected to, and a session has no
        // connections of its own. `session.set_agent_hooks` is what a
        // client puts to a session; answering both here would give a
        // host two spellings of one act, one of which cannot keep half
        // its promise.
        ops::HOST_ADD
        | ops::HOST_REMOVE
        | ops::HOST_LIST
        | ops::HOST_CONNECT
        | ops::HOST_DISCONNECT
        | ops::HOST_STATUS
        | ops::AGENT_SET_HOOKS => {
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

    if op == ops::SESSION_SET_THEME {
        let p: SessionSetThemeParams = decode(params)?;
        return session_set_theme(h, p).await.map(HandlerOutcome::Reply);
    }

    if op == ops::SESSION_SET_AGENT_HOOKS {
        let p: SessionSetAgentHooksParams = decode(params)?;
        return session_set_agent_hooks(h, p)
            .await
            .map(HandlerOutcome::Reply);
    }

    // Guarded before the decode: see [`put_file_size_guard`].
    if op == ops::SESSION_PUT_FILE {
        put_file_size_guard(&params)?;
        let p: SessionPutFileParams = decode(params)?;
        return session_put_file(h, p).await.map(HandlerOutcome::Reply);
    }

    dispatch(h, op, params).await.map(HandlerOutcome::Reply)
}

/// The grid an attaching client declared, refused when it is not a grid.
///
/// Zero cell pixels are legal — a headless client has no cell metrics
/// to report — but a zero-sized grid is not a grid. Checked for an
/// unfocused attach too: it is still that connection's declared
/// geometry, which its first INPUT or RESIZE frame applies.
#[cfg(feature = "server-vt")]
fn attach_geometry(
    cols: u16,
    rows: u16,
    cell_w_px: u16,
    cell_h_px: u16,
) -> Result<Geometry, HandlerError> {
    if cols == 0 || rows == 0 {
        return Err(HandlerError::invalid_param(format!(
            "cols and rows must both be non-zero (got {cols}x{rows})"
        )));
    }
    Ok(Geometry {
        cols,
        rows,
        cell_w: u32::from(cell_w_px),
        cell_h: u32::from(cell_h_px),
    })
}

/// Give a tab the geometry an attaching client claimed, and wait for
/// both halves — the server terminal and the child — to take it.
///
/// Awaited, never fired and forgotten: the ack is what reports a
/// geometry either half refused, and everything the attach does next is
/// encoded from the grid this settles. A `Resize` still sitting on the
/// command channel would let that encode run at exactly the geometry
/// the attach exists to replace.
#[cfg(feature = "server-vt")]
pub(crate) async fn await_attach_resize(
    commands: &tokio::sync::mpsc::Sender<crate::tab_task::TabCmd>,
    tab_id: i64,
    geometry: Geometry,
) -> Result<(), HandlerError> {
    let (resized_tx, resized_rx) = tokio::sync::oneshot::channel();
    commands
        .send(crate::tab_task::TabCmd::Resize {
            geometry,
            ack: Some(resized_tx),
        })
        .await
        .map_err(|_| tab_gone(tab_id))?;
    resized_rx
        .await
        // The task dropped the ack without answering, which only
        // happens when the task itself is going away.
        .map_err(|_| tab_gone(tab_id))?
        .map_err(|error| attach_resize_refusal(tab_id, geometry.cols, geometry.rows, &error))
}

/// The payload kind a client's preference order settles on.
///
/// A list mixing kinds this build has never heard of with ones it
/// serves is fine — the client states a preference order and the first
/// entry that is both *servable* and *eligible* wins.
///
/// Servable is what `session.identify` ADVERTISED (`payload_kinds`):
/// the advertisement is the contract a client negotiated against, so a
/// kind absent from it must not be accepted even when the code could
/// produce it. Eligible is the kind's own requirement, which only
/// GHOSTSNP has — it is libghostty's binary state, so both ends must be
/// the same build.
///
/// The two refusals stay separate because they instruct differently.
/// Nothing servable at all is "offer something else"; servable but
/// ineligible is "the two builds disagree", which is the answer a
/// pre-`vt` client's whole restart flow hangs off. Splitting the walk
/// in two is what keeps them apart: a client offering
/// `[ghostty-snapshot, vt]` across a skew must land on `vt` rather than
/// on either refusal.
#[cfg(feature = "server-vt")]
fn negotiate_kind(
    info: &SessionInfo,
    kinds: &[AttachPayloadKind],
    libghostty_build: &str,
) -> Result<AttachPayloadKind, HandlerError> {
    let servable: Vec<&AttachPayloadKind> = kinds
        .iter()
        .filter(|kind| info.payload_kinds.contains(kind))
        .collect();
    if servable.is_empty() {
        return Err(HandlerError::new(
            "unsupported-kind",
            format!(
                "this session serves {:?}; the client offered {kinds:?}",
                info.payload_kinds
            ),
        ));
    }
    let builds_match = libghostty_build == info.libghostty_build;
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
    eligible.ok_or_else(|| {
        HandlerError::new(
            "build-mismatch",
            format!(
                "this session is {:?}; the client is {libghostty_build:?}",
                info.libghostty_build
            ),
        )
    })
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
/// with and the server takes it. Two clients racing are last-writer-wins
/// by construction — there is one stored seed and the last `set_theme`
/// to reach the tab task is the one its terminal ends on.
#[cfg(feature = "server-vt")]
async fn session_set_theme(
    h: &IpcHandler,
    p: SessionSetThemeParams,
) -> Result<serde_json::Value, HandlerError> {
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
/// recolor.
#[cfg(not(feature = "server-vt"))]
#[allow(clippy::unused_async)]
async fn session_set_theme(
    _h: &IpcHandler,
    _p: SessionSetThemeParams,
) -> Result<serde_json::Value, HandlerError> {
    Err(no_server_vt())
}

/// `session.set_agent_hooks`: raise the host's `agent-hooks` key to at
/// least the connected client's own allow-list (plan 046 §3.4, plan 064
/// §3.3).
///
/// Everything this function does is admission. The work — reading and
/// rewriting five agents' config files under the session user's `$HOME`
/// — belongs to the daemon, which is the only process here that links
/// the install engine; this crate only decides *whether* it may run.
///
/// In [`is_mutating_op`] because a session that has latched
/// `session.stop` has already flushed and reaped: entries pointing at a
/// socket about to be unlinked are worse than no entries at all.
/// Otherwise every same-UID connection may state it, and every raise
/// only ever widens what the key allows (plan 064 §3.3) — two
/// connections naming different agents both win.
///
/// A per-agent install failure is a *reported* failure, never an error
/// frame: the reply's `errors` list carries it, so a client hears which
/// agent broke and still keeps the session it just attached to. Only a
/// whole-run failure — no `$HOME`, an unwritable record, a lock another
/// writer never released — is an error frame, and a malformed request is
/// a different one ([`AgentHooksError::InvalidParam`]).
async fn session_set_agent_hooks(
    h: &IpcHandler,
    p: SessionSetAgentHooksParams,
) -> Result<serde_json::Value, HandlerError> {
    let handle = h.agent_hooks.as_ref().ok_or_else(|| {
        HandlerError::new(
            "not-supported",
            "this session cannot wire agent hooks: it was built without an install backend",
        )
    })?;
    let result = handle
        .run(AgentHooksRequest {
            agents: p.agents,
            client: p.client,
        })
        .await
        .map_err(|error| match error {
            AgentHooksError::InvalidParam(message) => HandlerError::new("invalid-param", message),
            AgentHooksError::Failed(message) => HandlerError::new("internal", message),
        })?;
    encode(&result)
}

/// `session.put_file`: land one client-supplied file on the host and
/// answer with the path a shell can be told to read (plan 047 §3.1).
///
/// Available to every same-UID connection: uploads land in separate
/// private directories and coexist. In [`is_mutating_op`] because a
/// session that has latched `session.stop` is about to sweep the very
/// directory this writes into — a slow write therefore holds the
/// mutation barrier and a racing stop waits for it, the price of never
/// handing back a path that is already gone.
async fn session_put_file(
    h: &IpcHandler,
    p: SessionPutFileParams,
) -> Result<serde_json::Value, HandlerError> {
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
/// Every subscriber sees everything the session publishes — workspace
/// facts, `notification.fired`, and `tab.effect` alike. There is no
/// classification and no projection: which client a bell or an OSC 52
/// write is *for* is the viewing client's question, answered where the
/// tab is on screen, not here.
///
/// Not a mutating op — it changes no workspace state — but it does
/// establish a resource, so it is refused once the session has latched:
/// a subscriber handed out after the stop swept the registry would be one
/// nobody can end. [`SessionState::register_subscriber`] closes the race by
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
    let subscription = event_push::spawn(cut, h.push_limits);
    if !session.register_subscriber(ctx, subscription.abort.clone()) {
        // Lost the race with the stop's sweep. Abort what we just
        // started rather than leaking a relay the stop will never see.
        subscription.abort.abort();
        return Err(shutting_down());
    }
    Ok(HandlerOutcome::ReplyThen {
        reply: encode(&EventsSubscribeResult {
            revision: subscription.revision,
            session_id: session.info.session_id.clone(),
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
    session.abort_subscribers();

    // Waits out exactly the mutations that got past the latch.
    let _drained = session.barrier.write().await;

    // Reported, not swallowed: a stop that could not write the layout is
    // the last chance anyone has to learn the tabs are not coming back.
    if let Err(error) = h.workspace.flush() {
        tracing::error!(%error, "session.stop could not flush the workspace layout");
    }
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

/// Where the local host session listens, for a client that wants the
/// events a UI socket cannot serve. Only under `session` — in-process
/// has no slot to point at.
fn local_session_socket(mode: LocalBackendMode) -> Option<String> {
    (mode == LocalBackendMode::Session).then(roost_ipc::session_socket_path)?
}

/// An op meant for the slot could not be put to it — see
/// [`roost_ipc::local_route::SLOT_UNAVAILABLE_CODE`] for why this is not
/// a code of its own.
fn local_session_unavailable() -> HandlerError {
    HandlerError::new(
        roost_ipc::local_route::SLOT_UNAVAILABLE_CODE,
        roost_ipc::local_route::SLOT_UNAVAILABLE,
    )
}

/// Plan 063 §D10: hand one whole request to the slot and answer with
/// its reply — [`UiRequest::LocalSessionForward`] is where the request
/// and its rationale are defined.
async fn forward_to_local_session(
    h: &IpcHandler,
    op: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, HandlerError> {
    let mut reply = h
        .ui_call(|reply| UiRequest::LocalSessionForward {
            op: op.to_string(),
            params,
            reply,
        })
        .await??;
    strip_ui_socket_fence(op, &mut reply);
    Ok(reply)
}

/// Take the session's `revision` back off a forwarded reply.
///
/// The `tab.list` arm rides its fence only on a socket that also serves
/// the event stream it fences. A forwarded reply carries the
/// *session's* fence, and this socket still cannot serve that stream —
/// `events.subscribe` is `not-implemented` here — so a client holding
/// one could not do anything with it but believe it had a fence. One
/// that wants it dials `identify.local_session_socket`.
fn strip_ui_socket_fence(op: &str, reply: &mut serde_json::Value) {
    if op != ops::TAB_LIST {
        return;
    }
    if let Some(object) = reply.as_object_mut() {
        object.remove("revision");
    }
}

async fn dispatch(
    h: &IpcHandler,
    op: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, HandlerError> {
    // Plan 063 §D10: under `session` a bare id names a tab on the slot,
    // not a row in this socket's own workspace — see
    // [`UiRequest::LocalSessionForward`] for what answering one here
    // would cost.
    //
    // Neither branch touches a ref that already names a host: the
    // rewrite is bare-only, so an explicit `h<n>.<id>` reaches the op's
    // own route parser below exactly as it does under `in-process`.
    // That is why this runs per-op off the table rather than as one
    // "session mode ⇒ forward" test.
    let route = h.local_route();
    let params = if route.mode == LocalBackendMode::Session {
        match roost_ipc::local_route::classify(op) {
            Some(roost_ipc::OpClass::Forward) => {
                return forward_to_local_session(h, op, params).await;
            }
            Some(roost_ipc::OpClass::Rewrite(ids)) => {
                let mut params = params;
                // The *reference* decides, not the mode: a request that
                // already names a host has never needed the local
                // backend, and refusing it for want of a slot would be a
                // regression against `in-process`, where the same
                // request works.
                roost_ipc::local_route::rewrite_slot_ids(ids, &mut params, route.slot_host)
                    .map_err(|roost_ipc::SlotRequired| local_session_unavailable())?;
                params
            }
            _ => params,
        }
    } else {
        params
    };
    match op {
        ops::IDENTIFY => {
            let _p: IdentifyParams = decode(params)?;
            let route = h.local_route();
            // Under `session` this socket's own workspace is empty, so
            // `workspace.active()` would answer `0` and `roostctl` with
            // no `--tab` would have nothing to act on. A bare id means
            // the slot's id there (plan 063 §D10), and so does this.
            let (active_project_id, active_tab_id) = match route.mode {
                LocalBackendMode::Session => route.slot_active.unwrap_or((0, 0)),
                LocalBackendMode::InProcess => h.workspace.active(),
            };
            let result = IdentifyResult {
                socket_path: h.socket_path.to_string_lossy().into(),
                pid: std::process::id() as i32,
                active_project_id,
                active_tab_id,
                app_label: h.app_label.clone(),
                app_id: h.app_id.clone(),
                ui_version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: roost_ipc::PROTOCOL_VERSION,
                local_backend: route.mode,
                local_session_socket: local_session_socket(route.mode),
                local_backend_switch: route.switch.map(str::to_string),
                persist_error: h.workspace.persist_error(),
                ops: Some(h.serves(&route)),
                instance_id: h.session.is_none().then(|| h.instance_id.clone()),
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
            // Resolved here, not stored empty and left to `tab.open`'s
            // own fallback: the project row and its seed tab must agree
            // on where the project "is", and a caller creating on a
            // remote host has no `$HOME` of its own to offer (D4).
            let cwd = if p.cwd.is_empty() {
                crate::home_dir()
            } else {
                p.cwd
            };
            let project = h.workspace.create_project(&p.name, &cwd).map_err(ws_err)?;
            encode(&ProjectCreateResult { project })
        }
        ops::PROJECT_ENSURE => {
            let p: ProjectEnsureParams = decode(params)?;
            let (project, created) = h
                .workspace
                .ensure_project(&p.name, p.cwd.as_deref().unwrap_or_default())
                .map_err(ws_err)?;
            encode(&ProjectEnsureResult { project, created })
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
            let cleared = h
                .workspace
                .clear_notification(p.tab_id, p.generation)
                .map_err(ws_err)?;
            encode(&TabClearNotificationResult { cleared })
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
            // `toggle:<agent>` is the agent-hooks card's third answer
            // (plan 064 §3.5); which agents exist is the card's to say,
            // so the shape is checked here and the name over there.
            if !matches!(p.action.as_str(), "confirm" | "cancel")
                && p.action.strip_prefix("toggle:").is_none_or(str::is_empty)
            {
                return Err(HandlerError::invalid_param(format!(
                    "action must be confirm, cancel or toggle:<agent> (got {:?})",
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
        // Answered by the app alone, with no headless fallback: the
        // reply names what every *connected* host did, and the
        // connection set is the app's. A socket with no window behind
        // it therefore gets `ui_call`'s `no UI attached`.
        ops::AGENT_SET_HOOKS => {
            let p: AgentSetHooksParams = decode(params)?;
            let result = h
                .ui_call(|reply| UiRequest::AgentSetHooks {
                    agents: p.agents,
                    reply,
                })
                .await??;
            encode(&result)
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

/// Which socket a [`served_ops`] answer describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketKind {
    /// A UI socket, running its own local tabs on this backend.
    Ui(LocalBackendMode),
    /// A host session's socket.
    Session,
}

/// Why a dispatched op is left out of `identify.ops`, named for what the
/// socket answers instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Withheld {
    /// `unknown-op` on a UI socket: a host session's own op.
    SessionOnly,
    /// `unknown-op` on a session: the host registry and this machine's
    /// own `agent-hooks` key are client state a session does not keep.
    UiSocketOnly,
    /// `not-implemented` on a UI socket running its tabs in-process.
    NotImplementedInProcess,
    /// `internal: no UI attached` with no app behind the socket — or,
    /// for the fire-and-forget `app.activate` and `clipboard.write`,
    /// nothing at all.
    NeedsUi,
    /// [`Self::NeedsUi`], except on a session built with `server-vt`,
    /// which answers from the tab's own server terminal.
    NeedsATerminal,
    /// `not-enabled` unless the process was launched with
    /// `ROOST_TEST_MODE=1`.
    TestMode,
    /// `not-implemented` off macOS: the Dock, the native menu bar,
    /// Sparkle and `UNUserNotificationCenter` are macOS iced's alone.
    MacosOnly,
    /// `unsupported-kind` on a session built without `server-vt`: there
    /// is no server terminal to recolor.
    ServerVt,
}

impl Withheld {
    fn applies(self, socket: SocketKind, test_mode: bool, has_ui: bool) -> bool {
        match self {
            Self::SessionOnly => matches!(socket, SocketKind::Ui(_)),
            Self::UiSocketOnly => socket == SocketKind::Session,
            Self::NotImplementedInProcess => socket == SocketKind::Ui(LocalBackendMode::InProcess),
            Self::NeedsUi => !has_ui,
            Self::NeedsATerminal => {
                !has_ui && !(socket == SocketKind::Session && cfg!(feature = "server-vt"))
            }
            Self::TestMode => !test_mode,
            Self::MacosOnly => !cfg!(target_os = "macos"),
            Self::ServerVt => !cfg!(feature = "server-vt"),
        }
    }
}

/// One row per op `dispatch_outcome` and `dispatch` have an arm for, with
/// what keeps it out of `identify.ops` and where; an empty list is an op
/// every socket serves. A test parses both dispatchers' arms against it.
const DISPATCHED_OPS: &[(&str, &[Withheld])] = {
    use Withheld::{
        MacosOnly, NeedsATerminal, NeedsUi, NotImplementedInProcess, ServerVt, SessionOnly,
        TestMode, UiSocketOnly,
    };
    &[
        (ops::IDENTIFY, &[]),
        (ops::TAB_OPEN, &[]),
        (ops::TAB_CLOSE, &[]),
        (ops::TAB_LIST, &[]),
        (ops::TAB_WRITE, &[]),
        (ops::TAB_RESIZE, &[]),
        (ops::TAB_DUMP, &[NeedsATerminal]),
        (ops::PROJECT_CREATE, &[]),
        (ops::PROJECT_ENSURE, &[]),
        (ops::PROJECT_RENAME, &[]),
        (ops::PROJECT_DELETE, &[]),
        (ops::TAB_REORDER, &[]),
        (ops::PROJECT_REORDER, &[]),
        (ops::TAB_FOCUS, &[]),
        (ops::TAB_SEND_FILE, &[NeedsUi]),
        (ops::TAB_SET_TITLE, &[]),
        (ops::TAB_SET_STATE, &[]),
        (ops::TAB_CLEAR_NOTIFICATION, &[]),
        (ops::TAB_SET_HOOK_ACTIVE, &[]),
        (ops::TAB_AGENT_REPORT, &[]),
        (ops::NOTIFICATION_CREATE, &[]),
        (ops::APP_ACTIVATE, &[NeedsUi]),
        (ops::SCREENSHOT, &[NeedsUi]),
        (ops::WINDOW_METRICS, &[NeedsUi]),
        (ops::APP_RENDER_STATS, &[NeedsUi]),
        (ops::SIDEBAR_DUMP, &[NeedsUi]),
        (ops::PALETTE_OPEN, &[NeedsUi]),
        (ops::PALETTE_STATE, &[NeedsUi]),
        (ops::PALETTE_QUERY, &[NeedsUi]),
        (ops::PALETTE_ACTIVATE, &[NeedsUi]),
        (ops::PALETTE_DISMISS, &[NeedsUi]),
        (ops::PALETTE_PRESENT, &[NeedsUi]),
        (ops::SELECTION_SET, &[NeedsUi]),
        (ops::SELECTION_CLEAR, &[NeedsUi]),
        (ops::SELECTION_DUMP, &[NeedsUi]),
        (ops::CLIPBOARD_DUMP, &[NeedsUi]),
        (ops::CLIPBOARD_WRITE, &[NeedsUi]),
        (ops::TAB_FEED_PTY_BYTES, &[NeedsATerminal, TestMode]),
        (ops::TAB_CAPTURE_PTY_INPUT, &[NeedsATerminal, TestMode]),
        (ops::TAB_EXPAND_SELECTION_AT, &[NeedsUi, TestMode]),
        (ops::TAB_FEED_IME, &[NeedsUi, TestMode]),
        (ops::WINDOW_RESIZE, &[NeedsUi, TestMode]),
        (ops::SIDEBAR_SET_WIDTH, &[NeedsUi, TestMode]),
        (ops::TAB_DUMP_RESOLVED, &[NeedsATerminal]),
        (ops::TAB_DISPATCH_MOUSE_EVENT, &[NeedsUi, TestMode]),
        (ops::APP_SET_WINDOW_FOCUS, &[NeedsUi, TestMode]),
        (ops::APP_CURSOR_SHAPE, &[NeedsUi]),
        (ops::APP_ACTIVE_TERMINAL_FOCUSED, &[NeedsUi]),
        (ops::APP_SELECTED_TAB_ID, &[NeedsUi]),
        (ops::APP_DOCK_BADGE, &[NeedsUi, TestMode, MacosOnly]),
        (ops::APP_MENU_DUMP, &[NeedsUi, TestMode, MacosOnly]),
        (ops::APP_MENU_ACTIVATE, &[NeedsUi, TestMode, MacosOnly]),
        (ops::APP_DIALOG_DUMP, &[NeedsUi, TestMode]),
        (ops::APP_DIALOG_ANSWER, &[NeedsUi, TestMode]),
        (ops::APP_KEYBIND_DISPATCH, &[NeedsUi, TestMode]),
        (ops::APP_UPDATE_STATUS, &[NeedsUi, TestMode, MacosOnly]),
        (ops::APP_UPDATE_CHECK, &[NeedsUi, TestMode, MacosOnly]),
        (
            ops::APP_NOTIFICATION_STATUS,
            &[NeedsUi, TestMode, MacosOnly],
        ),
        (ops::EVENTS_SUBSCRIBE, &[NotImplementedInProcess]),
        (ops::AGENT_SET_HOOKS, &[UiSocketOnly, NeedsUi]),
        (ops::HOST_ADD, &[UiSocketOnly]),
        (ops::HOST_REMOVE, &[UiSocketOnly]),
        (ops::HOST_CONNECT, &[UiSocketOnly, NeedsUi]),
        (ops::HOST_DISCONNECT, &[UiSocketOnly, NeedsUi]),
        (ops::HOST_LIST, &[UiSocketOnly]),
        (ops::HOST_STATUS, &[UiSocketOnly, NeedsUi]),
        (ops::SESSION_IDENTIFY, &[SessionOnly]),
        (ops::SESSION_STOP, &[SessionOnly]),
        (ops::SESSION_SET_THEME, &[SessionOnly, ServerVt]),
        (ops::SESSION_SET_AGENT_HOOKS, &[SessionOnly]),
        (ops::SESSION_PUT_FILE, &[SessionOnly]),
    ]
};

/// The ops a socket would dispatch right now (plan 066 §3.1).
///
/// A capability that can never succeed is not one, so an op that could
/// only answer `internal: no UI attached` is withheld as surely as one
/// answering `unknown-op`. Pure — no handler, no I/O — so what a socket
/// claims can be tested without executing anything.
fn served_ops(socket: SocketKind, test_mode: bool, has_ui: bool) -> Vec<&'static str> {
    DISPATCHED_OPS
        .iter()
        .filter(|(op, withheld)| {
            !withheld
                .iter()
                .any(|reason| reason.applies(socket, test_mode, has_ui))
                && !(socket == SocketKind::Ui(LocalBackendMode::Session)
                    && withheld_from_the_slot(op, has_ui))
        })
        .map(|(op, _)| *op)
        .collect()
}

/// A UI socket under `local-backend = session` refuses what plan 063
/// §D10 classes `Unsupported`, and reaches the slot for a `Forward` or
/// `Rewrite` op only through the app.
fn withheld_from_the_slot(op: &str, has_ui: bool) -> bool {
    match roost_ipc::local_route::classify(op) {
        Some(roost_ipc::OpClass::Unsupported) => true,
        Some(roost_ipc::OpClass::Forward | roost_ipc::OpClass::Rewrite(_)) => !has_ui,
        _ => false,
    }
}

/// 63 random bits as 16 lowercase hex digits.
fn mint_instance_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    format!("{:016x}", u64::from_le_bytes(bytes) & (i64::MAX as u64))
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
        | WorkspaceError::HostLabelTaken(_)
        | WorkspaceError::ProjectNameBlank
        | WorkspaceError::ProjectCwdRequired(_) => HandlerError::invalid_param(e.to_string()),
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
    use roost_ipc::paths::BundleProfile;
    use std::collections::BTreeSet;

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
            conns: std::sync::Mutex::new(Connections::default()),
        }
    }

    fn live_subscribers(state: &SessionState) -> usize {
        lock(&state.conns).subscribers.as_ref().map_or(0, Vec::len)
    }

    /// A subscriber registration with a throwaway relay: what these cases
    /// are about is the *registry*, not delivery.
    fn register(state: &SessionState, conn_id: u64, relay: tokio::task::AbortHandle) -> bool {
        let (ctx, _watch) = ConnCtx::new(conn_id);
        state.register_subscriber(&ctx, relay)
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
        assert!(register(&state, 1, stale));

        let parked = tokio::spawn(std::future::pending::<()>());
        assert!(register(&state, 2, parked.abort_handle()));
        assert_eq!(
            live_subscribers(&state),
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
        assert!(register(&state, 1, parked.abort_handle()));

        state.abort_subscribers();
        assert!(
            parked.await.expect_err("aborted").is_cancelled(),
            "the sweep must actually end the relay"
        );

        let late = tokio::spawn(std::future::pending::<()>());
        assert!(
            !register(&state, 2, late.abort_handle()),
            "a subscribe after the sweep must be refused"
        );
        late.abort();
    }

    /// The other half of the same race, and the one that only the
    /// registry lock can settle: the latch is set before the sweep
    /// runs, so a subscribe admitted past the latch must still be
    /// refused — a subscriber registered after the sweep is one no closer
    /// can reach.
    #[tokio::test]
    async fn a_subscribe_that_raced_the_stop_latch_is_refused() {
        let state = session_state();
        state.stopping.store(true, Ordering::Release);

        let late = tokio::spawn(std::future::pending::<()>());
        assert!(
            !register(&state, 1, late.abort_handle()),
            "the latch is checked under the sweep's own lock"
        );
        assert_eq!(live_subscribers(&state), 0);
        late.abort();
    }

    /// A connection that ends takes its subscriber record with it, so a
    /// subscriber that went away leaves nothing for the stop sweep to
    /// walk.
    #[tokio::test]
    async fn a_subscriber_is_pruned_when_its_connection_ends() {
        let state = session_state();
        let parked = tokio::spawn(std::future::pending::<()>());
        assert!(register(&state, 7, parked.abort_handle()));

        state.forget_connection(7);
        assert_eq!(live_subscribers(&state), 0);
        parked.abort();
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

    /// A focused attach that could not size the tab says whose problem
    /// it was (review F2).
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

    // ----- identify's local-backend fields (plan 063 §D1) ------------

    /// A handler over a workspace holding one project and one tab, so
    /// `workspace.active()` is something a slot override can differ
    /// from.
    fn identify_handler(dir: &std::path::Path) -> IpcHandler {
        let workspace = Arc::new(Workspace::open(dir.join("state.json")));
        let project = workspace.create_project("p", "/tmp").expect("project");
        workspace
            .open_tab(project.id, "/tmp", "t")
            .expect("open a tab");
        assert_ne!(workspace.active(), (0, 0), "the fixture needs a selection");
        IpcHandler::new(
            workspace,
            Arc::new(PtySupervisor::new()),
            dir.join("roost.sock"),
            "Roost-test",
            "ai.stridelabs.Roost.test",
        )
    }

    async fn identify_of(h: &IpcHandler) -> IdentifyResult {
        let value = dispatch(h, ops::IDENTIFY, serde_json::json!({}))
            .await
            .expect("identify");
        serde_json::from_value(value).expect("decode identify")
    }

    #[tokio::test]
    async fn identify_reports_the_in_process_backend_when_no_route_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let h = identify_handler(dir.path());
        let id = identify_of(&h).await;

        assert_eq!(id.local_backend, LocalBackendMode::InProcess);
        assert_eq!(id.local_session_socket, None);
        assert_eq!(
            (id.active_project_id, id.active_tab_id),
            h.workspace.active()
        );
    }

    /// Under `session` the ids on the wire are the slot's UI-selected
    /// ones, not this socket's own (empty) workspace — and the answer
    /// tracks the cell, which is how a mode change reaches `identify`
    /// at all.
    #[tokio::test]
    async fn identify_under_session_follows_the_route_cell() {
        let dir = tempfile::tempdir().unwrap();
        let cell = Arc::new(LocalBackendCell::default());
        let h = identify_handler(dir.path()).with_local_route(Arc::clone(&cell));
        let local = h.workspace.active();

        // Installed but still in-process: unchanged from above.
        let id = identify_of(&h).await;
        assert_eq!((id.active_project_id, id.active_tab_id), local);
        assert_eq!(id.local_backend, LocalBackendMode::InProcess);

        cell.store(LocalRoute {
            mode: LocalBackendMode::Session,
            slot_socket: Some("/ignored/by/identify.sock".into()),
            slot_host: Some(3),
            slot_active: Some((41, 42)),
            switch: None,
        });
        let id = identify_of(&h).await;
        assert_eq!(id.local_backend, LocalBackendMode::Session);
        assert_eq!(id.local_backend_switch, None, "nothing is switching");
        assert_eq!((id.active_project_id, id.active_tab_id), (41, 42));
        assert_ne!((id.active_project_id, id.active_tab_id), local);
        // Profile-derived, not the cell's `slot_socket`.
        let expected = BundleProfile::session().unwrap().socket_path;
        assert_eq!(
            id.local_session_socket.as_deref(),
            Some(expected.to_string_lossy().as_ref())
        );

        // No slot selection yet reports the same "nothing selected" the
        // local path does.
        cell.store(LocalRoute {
            mode: LocalBackendMode::Session,
            slot_socket: None,
            slot_host: None,
            slot_active: None,
            switch: Some("replaying"),
        });
        let id = identify_of(&h).await;
        assert_eq!((id.active_project_id, id.active_tab_id), (0, 0));
        // The one thing outside the UI process that can see a switch
        // (plan 063 §D8a), and the reason a mutation was refused.
        assert_eq!(id.local_backend_switch.as_deref(), Some("replaying"));
    }

    /// A session daemon installs no route cell, so its `identify` is
    /// byte-for-byte what it was before the fields existed: its own
    /// workspace's selection, in-process, no session socket.
    #[tokio::test]
    async fn a_session_sockets_identify_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let h = identify_handler(dir.path())
            .with_session(session_state().info, StopHandle::new(|| async {}));
        let id = identify_of(&h).await;

        assert_eq!(id.local_backend, LocalBackendMode::InProcess);
        assert_eq!(id.local_session_socket, None);
        assert_eq!(
            (id.active_project_id, id.active_tab_id),
            h.workspace.active()
        );
    }

    // ── plan 063 §D10: the classification, against this dispatcher ──

    /// Every `ops::NAME` this file names, read out of its own source.
    ///
    /// The companion to `roost_ipc::local_route`'s parse of the
    /// *declarations*: that one catches a constant nobody classified,
    /// this one catches a **dispatch arm** added for an op this crate
    /// reaches by some other spelling. Reading the source rather than
    /// the table is the whole point — a walk of `OP_CLASSES` would
    /// agree with itself about anything.
    fn ops_this_dispatcher_names() -> Vec<String> {
        let source = include_str!("ipc.rs");
        let mut found: Vec<String> = Vec::new();
        for (index, _) in source.match_indices("ops::") {
            let name: String = source[index + 5..]
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if !name.is_empty() && !found.contains(&name) {
                found.push(name);
            }
        }
        assert!(
            found.len() > 40,
            "only {} `ops::` names scanned out of ipc.rs - the scan has \
             drifted and would pass vacuously",
            found.len()
        );
        found
    }

    #[test]
    fn every_dispatched_op_is_classified() {
        // The scan finds identifiers; the table is keyed by the wire
        // strings, so the bridge is `messages.rs`'s own declarations —
        // read the same way, for the same reason. Scoped to the `ops`
        // module's own body: every name this scan looks up came from an
        // `ops::` path, so its declaration lives there too, and a
        // whole-file search would resolve to whichever same-named
        // constant happens to appear first — true since plan 065 §3.4
        // gave `TabEffect` its own `CLIPBOARD_WRITE` with a different
        // wire spelling than `ops::CLIPBOARD_WRITE`.
        let declared = include_str!("../../roost-ipc/src/messages.rs");
        let ops_mod_start = declared
            .find("pub mod ops {")
            .expect("messages.rs declares `pub mod ops`");
        let declared = &declared[ops_mod_start..];
        let unclassified: Vec<_> = ops_this_dispatcher_names()
            .into_iter()
            .filter_map(|name| {
                let marker = format!("pub const {name}: &str = \"");
                let at = declared.find(&marker)?;
                let rest = &declared[at + marker.len()..];
                let value = &rest[..rest.find('"')?];
                roost_ipc::local_route::classify(value)
                    .is_none()
                    .then(|| format!("ops::{name} ({value:?})"))
            })
            .collect();
        assert!(
            unclassified.is_empty(),
            "this dispatcher serves ops plan 063 §D10's table does not \
             classify: {}",
            unclassified.join(", ")
        );
    }

    /// A handler with a session-mode route and no UI: the forward has
    /// nowhere to go, which is exactly what makes the *routing* visible
    /// without a running app.
    fn forwarding_handler(dir: &Path, slot: Option<u32>) -> IpcHandler {
        let h = identify_handler(dir);
        let cell = Arc::new(LocalBackendCell::new(LocalRoute {
            mode: LocalBackendMode::Session,
            slot_socket: None,
            slot_host: slot,
            slot_active: None,
            switch: None,
        }));
        h.with_local_route(cell)
    }

    /// The defect §D10 exists to close, driven through the dispatcher:
    /// a bare `tab.open` under `session` leaves through the forward.
    #[tokio::test]
    async fn a_bare_tab_open_under_session_never_touches_the_local_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let h = forwarding_handler(dir.path(), Some(2));
        let before = h.workspace.snapshot().len();

        let error = dispatch(&h, ops::TAB_OPEN, serde_json::json!({"project_id": "0"}))
            .await
            .expect_err("there is no UI to forward to");
        // It left through the forward, not through the local arm.
        assert_eq!(error.code, "internal", "{error:?}");
        assert_eq!(error.message, "no UI attached");
        assert_eq!(
            h.workspace.snapshot().len(),
            before,
            "the forward must not have created a project here"
        );
    }

    /// `project.create`'s twin, and the same assertion: nothing lands in
    /// the hidden workspace.
    #[tokio::test]
    async fn a_bare_project_create_under_session_never_touches_the_local_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let h = forwarding_handler(dir.path(), Some(2));
        let before = h.workspace.snapshot().len();
        let error = dispatch(
            &h,
            ops::PROJECT_CREATE,
            serde_json::json!({"name": "ghost", "cwd": "/tmp"}),
        )
        .await
        .expect_err("there is no UI to forward to");
        assert_eq!(error.code, "internal", "{error:?}");
        assert_eq!(h.workspace.snapshot().len(), before);
        assert!(!h
            .workspace
            .snapshot()
            .iter()
            .any(|project| project.name == "ghost"));
    }

    /// Under `in-process` the very same request is answered here, which
    /// is what makes the two assertions above about the *mode* rather
    /// than about a handler with no UI.
    #[tokio::test]
    async fn the_same_request_under_in_process_is_answered_locally() {
        let dir = tempfile::tempdir().unwrap();
        let h = identify_handler(dir.path());
        let before = h.workspace.snapshot().len();
        dispatch(
            &h,
            ops::PROJECT_CREATE,
            serde_json::json!({"name": "ghost", "cwd": "/tmp"}),
        )
        .await
        .expect("in-process serves it from the local workspace");
        assert_eq!(h.workspace.snapshot().len(), before + 1);
    }

    /// `project.ensure` forwards both of its outcomes: the hidden
    /// workspace already holds a `p`, so a find answered here would
    /// succeed, and a create answered here would land a `ghost`.
    #[tokio::test]
    async fn a_bare_project_ensure_under_session_never_touches_the_local_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let h = forwarding_handler(dir.path(), Some(2));
        let before = h.workspace.snapshot();
        for name in ["p", "ghost"] {
            let error = dispatch(
                &h,
                ops::PROJECT_ENSURE,
                serde_json::json!({"name": name, "cwd": "/tmp"}),
            )
            .await
            .expect_err("there is no UI to forward to");
            assert_eq!(error.code, "internal", "{name}: {error:?}");
        }
        assert_eq!(h.workspace.snapshot(), before);
    }

    #[tokio::test]
    async fn project_ensure_on_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let h = identify_handler(dir.path());
        let active = h.workspace.active();
        let ensure = |params: serde_json::Value| dispatch(&h, ops::PROJECT_ENSURE, params);

        for params in [
            serde_json::json!({"name": " ", "cwd": "/tmp"}),
            serde_json::json!({"name": "new"}),
            serde_json::json!({"name": "new", "cwd": ""}),
        ] {
            let error = ensure(params.clone()).await.expect_err("refused");
            assert_eq!(error.code, "invalid-param", "{params}: {error:?}");
        }
        let error = ensure(serde_json::json!({"name": "p", "activate": true}))
            .await
            .expect_err("strict params");
        assert_eq!(error.code, "unknown-field", "{error:?}");

        let found: ProjectEnsureResult =
            serde_json::from_value(ensure(serde_json::json!({"name": "p"})).await.unwrap())
                .unwrap();
        assert!(!found.created);
        assert_eq!(found.project.cwd, "/tmp");
        assert_eq!(found.project.tabs.len(), 1, "a find carries its tabs");

        let made: ProjectEnsureResult = serde_json::from_value(
            ensure(serde_json::json!({"name": "new", "cwd": "/var"}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(made.created);
        assert_eq!(made.project.cwd, "/var");
        assert_eq!(h.workspace.active(), active);
    }

    /// §D10's ordering clause. A host-qualified reorder must reach the
    /// op's own route parser, which refuses it here for want of a UI —
    /// *not* be re-addressed to the slot.
    #[tokio::test]
    async fn a_host_qualified_op_is_not_re_addressed_to_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let h = forwarding_handler(dir.path(), Some(2));
        let error = dispatch(
            &h,
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "h9.1", "tab_ids": ["h9.7"]}),
        )
        .await
        .expect_err("no UI to route a host-qualified reorder through");
        assert_eq!(error.code, "invalid-param", "{error:?}");
        assert!(
            error.message.contains("host-qualified tab.reorder"),
            "it left through the host route parser, naming host 9: {error:?}"
        );
    }

    /// A rewrite row with no slot to rewrite to answers the one
    /// sentence, rather than falling through to the hidden workspace and
    /// reporting `not-found` about a tab that exists.
    #[tokio::test]
    async fn a_rewrite_op_with_no_connected_slot_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let h = forwarding_handler(dir.path(), None);
        let error = dispatch(&h, ops::TAB_DUMP, serde_json::json!({"tab_id": "7"}))
            .await
            .expect_err("no slot");
        assert_eq!(error.code, roost_ipc::local_route::SLOT_UNAVAILABLE_CODE);
        assert_eq!(error.message, roost_ipc::local_route::SLOT_UNAVAILABLE);
    }

    /// A host-qualified request has never needed the local backend, so a
    /// slot that is down must not refuse it.
    ///
    /// The pair is the point: the *same op* with a bare id is
    /// `host-unavailable` (that tab lives on a slot that is not there),
    /// and with `h9.7` it reaches the op's own route parser — which
    /// here, with no UI, refuses it for the reason it always has. A
    /// slot-availability gate placed ahead of the reference would have
    /// answered both the same way, which is a straight regression
    /// against `in-process`.
    #[tokio::test]
    async fn a_host_qualified_op_does_not_need_a_connected_slot() {
        let dir = tempfile::tempdir().unwrap();
        let h = forwarding_handler(dir.path(), None);

        let bare = dispatch(&h, ops::TAB_DUMP, serde_json::json!({"tab_id": "7"}))
            .await
            .expect_err("a bare id names a slot that is not there");
        assert_eq!(bare.code, roost_ipc::local_route::SLOT_UNAVAILABLE_CODE);

        let qualified = dispatch(&h, ops::TAB_DUMP, serde_json::json!({"tab_id": "h9.7"}))
            .await
            .expect_err("there is no UI to read host 9's terminal");
        assert_ne!(
            qualified.code,
            roost_ipc::local_route::SLOT_UNAVAILABLE_CODE,
            "host 9 is not the local backend's business: {qualified:?}"
        );
        assert_eq!(qualified.code, "internal", "{qualified:?}");

        // The same for the ops that carry their own route parser.
        let reorder = dispatch(
            &h,
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "h9.1", "tab_ids": ["h9.7"]}),
        )
        .await
        .expect_err("no UI");
        assert_eq!(reorder.code, "invalid-param", "{reorder:?}");
        assert!(reorder.message.contains("host-qualified tab.reorder"));

        let send_file = dispatch(
            &h,
            ops::TAB_SEND_FILE,
            serde_json::json!({"tab": "h9.7", "paths": ["/tmp/x"]}),
        )
        .await
        .expect_err("no UI");
        assert_ne!(
            send_file.code,
            roost_ipc::local_route::SLOT_UNAVAILABLE_CODE,
            "{send_file:?}"
        );
    }

    /// The UI socket's `tab.list` omits `revision` by contract, and a
    /// forwarded one has to keep that promise even though the session's
    /// answer carries one.
    #[test]
    fn a_forwarded_tab_list_loses_the_sessions_fence() {
        let mut listed = serde_json::json!({"projects": [], "revision": "12"});
        strip_ui_socket_fence(ops::TAB_LIST, &mut listed);
        assert_eq!(listed, serde_json::json!({"projects": []}));

        // Only that one op, and only that one field: a forward is
        // otherwise the session's reply verbatim.
        let mut dumped = serde_json::json!({"rows_text": [], "revision": "12"});
        strip_ui_socket_fence(ops::TAB_DUMP, &mut dumped);
        assert_eq!(
            dumped,
            serde_json::json!({"rows_text": [], "revision": "12"})
        );
        let mut without = serde_json::json!({"projects": []});
        strip_ui_socket_fence(ops::TAB_LIST, &mut without);
        assert_eq!(without, serde_json::json!({"projects": []}));
    }

    // ── plan 066 §3.1: what a socket says it serves ─────────────────

    /// A UI socket with an app behind it, outside test mode — in-process
    /// and under `local-backend = session` alike, for as long as
    /// `events.subscribe` is served in neither.
    const UI: &[&str] = &[
        "agent.set_hooks",
        "app.activate",
        "app.active_terminal_focused",
        "app.cursor_shape",
        "app.render_stats",
        "app.screenshot",
        "app.selected_tab_id",
        "app.sidebar_dump",
        "app.window_metrics",
        "clipboard.dump",
        "clipboard.write",
        "host.add",
        "host.connect",
        "host.disconnect",
        "host.list",
        "host.remove",
        "host.status",
        "identify",
        "notification.create",
        "palette.activate",
        "palette.dismiss",
        "palette.open",
        "palette.present",
        "palette.query",
        "palette.state",
        "project.create",
        "project.delete",
        "project.ensure",
        "project.rename",
        "project.reorder",
        "selection.clear",
        "selection.dump",
        "selection.set",
        "tab.agent_report",
        "tab.clear_notification",
        "tab.close",
        "tab.dump",
        "tab.dump_resolved",
        "tab.focus",
        "tab.list",
        "tab.open",
        "tab.reorder",
        "tab.resize",
        "tab.send_file",
        "tab.set_hook_active",
        "tab.set_state",
        "tab.set_title",
        "tab.write",
    ];

    /// What `ROOST_TEST_MODE=1` adds to [`UI`].
    const UI_TEST_SEAMS: &[&str] = &[
        "app.dialog_answer",
        "app.dialog_dump",
        "app.keybind_dispatch",
        "app.set_window_focus",
        "sidebar.set_width",
        "tab.capture_pty_input",
        "tab.dispatch_mouse_event",
        "tab.expand_selection_at",
        "tab.feed_ime",
        "tab.feed_pty_bytes",
        "window.resize",
    ];

    /// What test mode adds on macOS only.
    const MACOS_TEST_SEAMS: &[&str] = &[
        "app.dock_badge",
        "app.menu_activate",
        "app.menu_dump",
        "app.notification_status",
        "app.update_check",
        "app.update_status",
    ];

    /// A headless session outside test mode.
    const SESSION: &[&str] = &[
        "events.subscribe",
        "identify",
        "notification.create",
        "project.create",
        "project.delete",
        "project.ensure",
        "project.rename",
        "project.reorder",
        "session.identify",
        "session.put_file",
        "session.set_agent_hooks",
        "session.stop",
        "tab.agent_report",
        "tab.clear_notification",
        "tab.close",
        "tab.focus",
        "tab.list",
        "tab.open",
        "tab.reorder",
        "tab.resize",
        "tab.set_hook_active",
        "tab.set_state",
        "tab.set_title",
        "tab.write",
    ];

    /// What a session built with `server-vt` — every shipped one — adds.
    const SESSION_SERVER_VT: &[&str] = &["session.set_theme", "tab.dump", "tab.dump_resolved"];

    fn on_this_platform(seams: &'static [&'static str]) -> &'static [&'static str] {
        if cfg!(target_os = "macos") {
            seams
        } else {
            &[]
        }
    }

    fn with_server_vt(extra: &'static [&'static str]) -> &'static [&'static str] {
        if cfg!(feature = "server-vt") {
            extra
        } else {
            &[]
        }
    }

    fn sorted(parts: &[&[&str]]) -> Vec<String> {
        let mut ops: Vec<String> = parts.concat().into_iter().map(str::to_string).collect();
        ops.sort_unstable();
        ops
    }

    fn every_configuration() -> impl Iterator<Item = (SocketKind, bool, bool)> {
        [
            SocketKind::Ui(LocalBackendMode::InProcess),
            SocketKind::Ui(LocalBackendMode::Session),
            SocketKind::Session,
        ]
        .into_iter()
        .flat_map(|socket| {
            [(false, false), (false, true), (true, false), (true, true)]
                .map(|(test_mode, has_ui)| (socket, test_mode, has_ui))
        })
    }

    /// The op constants `dispatch_outcome` and `dispatch` match on, read
    /// out of this file: `ops::X =>` arms, their `|` alternations on one
    /// line or several, and the `if op == ops::X` checks
    /// `dispatch_outcome` makes under the barrier.
    fn dispatcher_arm_names() -> Vec<String> {
        let source = include_str!("ipc.rs");
        let mut names: Vec<String> = Vec::new();
        for signature in ["\nasync fn dispatch_outcome(", "\nasync fn dispatch("] {
            let start = source
                .find(signature)
                .unwrap_or_else(|| panic!("ipc.rs declares {signature:?}"));
            let body = &source[start + 1..];
            let body = &body[..body.find("\n}\n").expect("the dispatcher closes")];
            for line in body.lines().map(str::trim) {
                let pattern = match line.strip_prefix("if op == ") {
                    Some(guard) => guard.trim_end_matches(" {"),
                    None => {
                        let line = line.trim_start_matches("| ");
                        line.split_once(" =>").map_or(line, |(pattern, _)| pattern)
                    }
                };
                let alternatives: Option<Vec<&str>> = pattern
                    .split(" | ")
                    .map(|alternative| {
                        alternative.strip_prefix("ops::").filter(|name| {
                            !name.is_empty()
                                && name.chars().all(|c| {
                                    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
                                })
                        })
                    })
                    .collect();
                for name in alternatives.into_iter().flatten() {
                    if !names.iter().any(|seen| seen == name) {
                        names.push(name.to_string());
                    }
                }
            }
        }
        assert!(
            names.len() > 60,
            "only {} dispatcher arms parsed out of ipc.rs - the scan has \
             drifted and would pass vacuously",
            names.len()
        );
        names
    }

    /// Every `(NAME, "value")` in `messages.rs`'s `ops` module.
    fn ops_declared() -> Vec<(String, String)> {
        let source = include_str!("../../roost-ipc/src/messages.rs");
        let start = source
            .find("\npub mod ops {\n")
            .expect("messages.rs declares `pub mod ops`");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("the ops module closes")];
        body.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("pub const ")?;
                let (name, rest) = rest.split_once(": &str = ")?;
                let value = rest.strip_prefix('"')?.strip_suffix("\";")?;
                Some((name.to_string(), value.to_string()))
            })
            .collect()
    }

    /// Parsed rather than walked: a walk of [`DISPATCHED_OPS`] would agree
    /// with itself about an arm somebody added without a row.
    #[test]
    fn every_dispatcher_arm_is_served_somewhere_or_withheld_by_name() {
        let declared = ops_declared();
        let value_of = |name: &str| {
            declared
                .iter()
                .find(|(declared, _)| declared == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("ops::{name} is not declared in messages.rs"))
        };
        let arms: BTreeSet<String> = dispatcher_arm_names()
            .iter()
            .map(|name| value_of(name))
            .collect();
        let served_somewhere: BTreeSet<&str> = every_configuration()
            .flat_map(|(socket, test_mode, has_ui)| served_ops(socket, test_mode, has_ui))
            .collect();
        let withheld_by_name: BTreeSet<&str> = DISPATCHED_OPS
            .iter()
            .filter(|(_, withheld)| !withheld.is_empty())
            .map(|(op, _)| *op)
            .collect();
        let accounted = |op: &str| served_somewhere.contains(op) || withheld_by_name.contains(op);

        let unaccounted: Vec<_> = arms.iter().filter(|arm| !accounted(arm)).collect();
        assert!(
            unaccounted.is_empty(),
            "dispatcher arms served_ops never returns and names no reason for: \
             {unaccounted:?}. Give each a DISPATCHED_OPS row."
        );

        let rowless: Vec<_> = DISPATCHED_OPS
            .iter()
            .map(|(op, _)| *op)
            .filter(|op| !arms.contains(*op))
            .collect();
        assert!(
            rowless.is_empty(),
            "DISPATCHED_OPS rows no dispatcher arm answers: {rowless:?}"
        );
        assert_eq!(
            DISPATCHED_OPS.len(),
            arms.len(),
            "one row per arm, and no duplicates"
        );

        let undispatched: Vec<_> = declared
            .iter()
            .filter(|(_, value)| {
                roost_ipc::local_route::classify(value) != Some(roost_ipc::OpClass::Event)
                    && !accounted(value)
            })
            .map(|(name, value)| format!("ops::{name} ({value:?})"))
            .collect();
        assert!(
            undispatched.is_empty(),
            "op constants neither served nor withheld: {}",
            undispatched.join(", ")
        );
    }

    #[test]
    fn the_four_configurations_serve_their_checked_in_lists() {
        let served =
            |socket, test_mode, has_ui| sorted(&[served_ops(socket, test_mode, has_ui).as_slice()]);
        assert_eq!(
            served(SocketKind::Ui(LocalBackendMode::InProcess), false, true),
            sorted(&[UI]),
            "UI in-process"
        );
        assert_eq!(
            served(SocketKind::Ui(LocalBackendMode::Session), false, true),
            sorted(&[UI]),
            "UI under local-backend = session"
        );
        assert_eq!(
            served(SocketKind::Session, false, false),
            sorted(&[SESSION, with_server_vt(SESSION_SERVER_VT)]),
            "a headless session"
        );
        assert_eq!(
            served(SocketKind::Ui(LocalBackendMode::InProcess), true, true),
            sorted(&[UI, UI_TEST_SEAMS, on_this_platform(MACOS_TEST_SEAMS)]),
            "UI with ROOST_TEST_MODE=1"
        );
    }

    /// A handler with an app behind it: a channel nothing drains, which
    /// `identify` never sends on.
    fn with_a_ui(h: IpcHandler) -> (IpcHandler, tokio::sync::mpsc::UnboundedReceiver<UiRequest>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (h.with_ui(tx), rx)
    }

    async fn reply_of(h: &IpcHandler, op: &str) -> serde_json::Value {
        let (ctx, _watch) = ConnCtx::new(1);
        match h.handle(&ctx, op, serde_json::json!({})).await {
            Ok(HandlerOutcome::Reply(value)) => value,
            Ok(HandlerOutcome::ReplyThen { .. }) => panic!("{op} answered with an action"),
            Err(error) => panic!("{op}: {error:?}"),
        }
    }

    fn ops_in(reply: &serde_json::Value) -> Vec<String> {
        let mut ops: Vec<String> =
            serde_json::from_value(reply["ops"].clone()).expect("an ops list");
        ops.sort_unstable();
        ops
    }

    #[tokio::test]
    async fn a_ui_sockets_identify_names_what_it_serves_and_which_process_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let (h, _ui) = with_a_ui(identify_handler(dir.path()));
        let reply = reply_of(&h, ops::IDENTIFY).await;
        assert_eq!(ops_in(&reply), sorted(&[UI]));
        let instance_id = reply["instance_id"].as_str().expect("an instance_id");
        assert_eq!(instance_id.len(), 16, "{instance_id}");
        assert!(
            instance_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{instance_id}"
        );

        let dir = tempfile::tempdir().unwrap();
        let (h, _ui) = with_a_ui(identify_handler(dir.path()).with_test_mode(true));
        assert_eq!(
            ops_in(&reply_of(&h, ops::IDENTIFY).await),
            sorted(&[UI, UI_TEST_SEAMS, on_this_platform(MACOS_TEST_SEAMS)])
        );
    }

    #[tokio::test]
    async fn under_session_mode_identify_names_what_reaches_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let (h, _ui) = with_a_ui(forwarding_handler(dir.path(), Some(2)));
        let reply = reply_of(&h, ops::IDENTIFY).await;
        assert_eq!(ops_in(&reply), sorted(&[UI]));
        assert!(reply["instance_id"].is_string(), "{reply}");

        // With no app to carry them, a forward and a rewrite reach
        // nothing, where in-process the same two are answered here.
        let dir = tempfile::tempdir().unwrap();
        let headless =
            ops_in(&reply_of(&forwarding_handler(dir.path(), Some(2)), ops::IDENTIFY).await);
        let dir = tempfile::tempdir().unwrap();
        let in_process = ops_in(&reply_of(&identify_handler(dir.path()), ops::IDENTIFY).await);
        for op in [ops::TAB_OPEN, ops::TAB_FOCUS] {
            assert!(!headless.iter().any(|served| served == op), "{op}");
            assert!(in_process.iter().any(|served| served == op), "{op}");
        }
    }

    #[tokio::test]
    async fn a_session_names_what_it_serves_and_no_instance_id() {
        let dir = tempfile::tempdir().unwrap();
        let h = identify_handler(dir.path())
            .with_session(session_state().info, StopHandle::new(|| async {}));
        let session_identify = reply_of(&h, ops::SESSION_IDENTIFY).await;
        assert_eq!(
            ops_in(&session_identify),
            sorted(&[SESSION, with_server_vt(SESSION_SERVER_VT)])
        );
        assert!(session_identify.get("instance_id").is_none());

        let identify = reply_of(&h, ops::IDENTIFY).await;
        assert_eq!(ops_in(&identify), ops_in(&session_identify));
        assert!(
            identify.get("instance_id").is_none(),
            "a session's identity is its session_id: {identify}"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut info = session_state().info;
        info.test_mode = true;
        let h = identify_handler(dir.path()).with_session(info, StopHandle::new(|| async {}));
        let seams: &[&str] = &["tab.capture_pty_input", "tab.feed_pty_bytes"];
        assert_eq!(
            ops_in(&reply_of(&h, ops::SESSION_IDENTIFY).await),
            sorted(&[
                SESSION,
                with_server_vt(SESSION_SERVER_VT),
                with_server_vt(seams)
            ])
        );
    }

    #[tokio::test]
    async fn an_instance_id_is_minted_once_per_handler() {
        let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = identify_handler(dir_a.path());
        let first = reply_of(&a, ops::IDENTIFY).await["instance_id"].clone();
        assert!(first.is_string(), "{first}");
        assert_eq!(reply_of(&a, ops::IDENTIFY).await["instance_id"], first);
        let b = identify_handler(dir_b.path());
        assert_ne!(reply_of(&b, ops::IDENTIFY).await["instance_id"], first);
    }
}
