//! Wire-format types. Mirrors `docs/reference/ipc.md` 1:1.
//!
//! All identifier fields (tab/project ids) are 64-bit ints but
//! serialize as JSON strings via the [`string_int64`] helper module —
//! JSON numbers lose precision past 2^53 and the legacy proto schema
//! gives us int64 ids.
//!
//! Byte payloads use base64 via [`bytes_base64`]. Tested for binary
//! fidelity (0x00..0xff round-trip) in `tests/binary_fidelity.rs`.
//!
//! Server-side request structs carry `#[serde(deny_unknown_fields)]`
//! so unknown fields are rejected (matches the strict server policy
//! in the spec). Response and event structs do NOT carry that
//! attribute — clients see them as permissive, allowing the server
//! to add fields in a backwards-compatible way.

use serde::{Deserialize, Serialize};

// ============================================================================
// Shared types
// ============================================================================

/// `TabState` — JSON string enum. Values: `"none"`, `"running"`,
/// `"needs_input"`, `"idle"`. The legacy proto's `TAB_STATE_UNSPECIFIED`
/// is intentionally omitted; the server always picks a concrete state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabState {
    #[default]
    None,
    Running,
    NeedsInput,
    Idle,
}

/// Tab snapshot. Used in `tab.open` / `tab.list` / `tab.opened` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    #[serde(with = "string_int64")]
    pub id: i64,
    #[serde(with = "string_int64")]
    pub project_id: i64,
    pub title: String,
    pub cwd: String,
    pub state: TabState,
    pub has_notification: bool,
    pub is_active: bool,
    pub user_titled: bool,
    pub position: i32,
    pub created_at: i64,
    pub last_active: i64,
    pub hook_active: bool,
}

/// Project snapshot. Used in `project.create` / `tab.list` /
/// `project.created` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    #[serde(with = "string_int64")]
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub position: i32,
    pub created_at: i64,
    #[serde(default)]
    pub tabs: Vec<Tab>,
}

// ============================================================================
// Request envelope (untyped — the dispatcher parses the params per op)
// ============================================================================

/// Raw request envelope before per-op typing.
///
/// The server reads each frame as a `RawRequest` first, then matches on
/// `op` and re-parses `params` into the typed per-op struct below. This
/// keeps the envelope decoder generic while still letting each op's
/// param struct carry `#[serde(deny_unknown_fields)]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRequest {
    /// Client-allocated correlation id. String-wrapped int64.
    #[serde(with = "string_int64")]
    pub id: i64,
    /// Dotted-lowercase op name (e.g. `"tab.open"`).
    pub op: String,
    /// Per-op parameter object. Defaults to an empty object when the
    /// client omits the field.
    #[serde(default = "empty_object")]
    pub params: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

// ============================================================================
// Response envelope
// ============================================================================

/// Response envelope — either success (`ok: true` + `result`) or
/// failure (`ok: false` + `error`).
///
/// Permissive on the client side (unknown fields ignored) so the
/// server can extend response payloads forward-compatibly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(with = "string_int64")]
    pub id: i64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// Error body — kebab-case stable `code`, human-readable `message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

impl Response {
    /// Build a success envelope from a JSON value.
    pub fn ok(id: i64, result: serde_json::Value) -> Response {
        Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error envelope from a stable code + message.
    pub fn err(id: i64, code: impl Into<String>, message: impl Into<String>) -> Response {
        Response {
            id,
            ok: false,
            result: None,
            error: Some(ResponseError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

// ============================================================================
// Event envelope (server push)
// ============================================================================

/// Server-push event. Only delivered after the client calls
/// `events.subscribe`. (`events.subscribe` is stubbed in M0 — it
/// replies success but the server never emits events on the
/// connection. M2 wires up the type system; M3+ implement the push.)
///
/// Permissive by default (no `deny_unknown_fields`) so future
/// server-side additions to the event envelope itself don't break
/// older clients. The inner `data` is a free-form `Value` so
/// per-event additions are already forward-compatible. The
/// server-side strictness lives on the *request* path, not on the
/// event-push path which is server→client only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event: String,
    pub data: serde_json::Value,
}

// ============================================================================
// Identify
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentifyParams {
    #[serde(default)]
    pub client_name: String,
    #[serde(default)]
    pub client_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentifyResult {
    pub socket_path: String,
    pub pid: i32,
    #[serde(with = "string_int64")]
    pub active_project_id: i64,
    #[serde(with = "string_int64")]
    pub active_tab_id: i64,
    pub app_label: String,
    pub app_id: String,
    pub ui_version: String,
    pub protocol_version: u32,
}

// ============================================================================
// Tab lifecycle
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabOpenParams {
    #[serde(with = "string_int64", default)]
    pub project_id: i64,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cols: u32,
    #[serde(default)]
    pub rows: u32,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabOpenResult {
    pub tab: Tab,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabCloseParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabListResult {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabWriteParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    /// Raw bytes encoded as base64. See `bytes_base64`.
    #[serde(with = "bytes_base64")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabResizeParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub cols: u32,
    pub rows: u32,
}

// ============================================================================
// Tab content dump (terminal grid → text)
// ============================================================================

/// `tab.dump` request. Returns the tab's live terminal *viewport* as
/// text — the determinism backbone for content assertions in automated
/// tests (assert on exact text instead of OCR / pixel-matching).
/// Scrollback above the viewport is a planned follow-up; today the dump
/// is the visible grid only, so no `scrollback` param is accepted yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabDumpParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

/// Cursor position within the dumped viewport, 0-indexed from the top-left.
/// Absent when the cursor is off-viewport or hidden by the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabDumpCursor {
    pub row: u32,
    pub col: u32,
    pub visible: bool,
}

/// `tab.dump` response. `rows_text` has one entry per visible row with
/// trailing blanks trimmed, reconstructing what's on screen (a blank
/// cell renders as a space so columns line up). Permissive on the wire
/// so per-cell color / scrollback fields can be added forward-compatibly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabDumpResult {
    pub cols: u32,
    pub rows: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<TabDumpCursor>,
    pub rows_text: Vec<String>,
}

// ============================================================================
// Command palette (overlay introspection + drive)
// ============================================================================
//
// The palette is a UI overlay, not workspace state, so these ops route
// through the UI seam (a `UiRequest` on GTK / the `UiBridge` on Mac)
// rather than the workspace. They make the palette a driveable, testable
// command surface: open it, read its rows, filter, and activate a row —
// where activating dispatches the *same* command an item's keybind would
// (a command row's id IS the KeybindAction id), so a palette test is also
// a command-dispatch test. Every op replies with the resulting
// `PaletteStateResult`, so a driver asserts without a second round-trip.

/// `palette.open` params: which root frame to present. Empty or
/// `"commands"` opens the command palette; `"launcher"` opens the
/// custom-command launcher. An unknown kind is rejected `invalid-param`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteOpenParams {
    #[serde(default)]
    pub kind: String,
}

/// `palette.query` params: replace the current frame's filter text
/// (resetting selection to the top match), as if the user typed it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteQueryParams {
    pub query: String,
}

/// `palette.activate` params: confirm the visible row whose item id
/// matches — exactly as pressing Enter on it would, running its command
/// or drilling into its sub-frame. `not-found` if no visible row has
/// that id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteActivateParams {
    pub id: String,
}

/// `palette.state` / `palette.dismiss` carry no params. Declared as
/// empty + strict structs so the handler validates the envelope like
/// every other op rather than ACK-ing arbitrary payloads — same
/// rationale as [`AppActivateParams`]. Distinct types keep each op's
/// contract its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteStateParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaletteDismissParams {}

/// One visible palette row. `id` is the activation key (a KeybindAction
/// id for command rows; a theme name / notification id in sub-frames).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteItemView {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
}

/// Snapshot of the palette after an op. `open` is false when no palette
/// is up (the remaining fields are then default/empty). When open,
/// `frame` is the current frame id (`"commands"` | `"launcher"` |
/// `"themes"` | `"notifications"`), `query`/`selection` are the live
/// filter + highlight, and `items` are the filtered rows in display
/// order. Permissive on the wire for forward-compatible fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteStateResult {
    pub open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub selection: u32,
    #[serde(default)]
    pub items: Vec<PaletteItemView>,
}

// ============================================================================
// Selection + clipboard (test ops, drive selection + read/seed pasteboard)
// ============================================================================
//
// These exist so `tools/roosttest/` can drive selection state and assert on
// the host clipboard end-to-end — neither was possible with the prior op
// set (no mouse simulation; no way to read the OS clipboard from outside
// the UI process). They also make selection a first-class op-set citizen
// per CLAUDE.md's "one core, two implementations" principle.

/// (col, row) in **viewport** coordinates — what the user would see if they
/// could click the cell. Server-side the UI converts to libghostty's
/// `PointTag::Screen` so the selection survives subsequent scrolling
/// (mirrors mouseDown / drag_begin).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionPoint {
    pub col: u16,
    pub row: u16,
}

/// `selection.set` request: drop any existing selection and create a new
/// one anchored at `anchor` with the cursor at `cursor`. Both are viewport
/// (col, row); rows outside `[0, tab_rows)` are rejected `invalid-param`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionSetParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub anchor: SelectionPoint,
    pub cursor: SelectionPoint,
}

/// `selection.clear` request: drop the selection on this tab (no-op if
/// none active).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionClearParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

/// `selection.dump` request: read back the current selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionDumpParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

/// `selection.dump` response. `text` is `None` when no selection exists
/// (or when both endpoints have scrolled off-screen and copy currently
/// returns nothing — same lossy behavior as `⌘C` / Ctrl+Shift+C today).
/// `anchor_visible` / `cursor_visible` report whether each endpoint is
/// currently in the viewport — useful for asserting clip behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionDumpResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub anchor_visible: bool,
    pub cursor_visible: bool,
}

/// `clipboard.dump` request. `target`: `"system"` reads the system
/// clipboard (`NSPasteboard.general` / CLIPBOARD); `"selection"` reads
/// the per-app selection pasteboard (named `NSPasteboard` on Mac /
/// PRIMARY on Linux). Unknown values are rejected `invalid-param`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardDumpParams {
    pub target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardDumpResult {
    /// `None` when the target has no text content (PRIMARY off Linux,
    /// or an empty pasteboard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `clipboard.write` request. Test-only seeding for the inverse of
/// `clipboard.dump` — lets a test set a known pasteboard value before
/// asserting paste behavior. Not a security regression: any process on
/// the host can already write the OS clipboard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardWriteParams {
    pub target: String,
    pub text: String,
}

// ============================================================================
// Test-only ops (ROOST_TEST_MODE=1)
// ============================================================================
//
// `tab.feed_pty_bytes` + `tab.capture_pty_input` are gated by the
// `ROOST_TEST_MODE=1` env var set at UI launch. They drive PTY output
// into a live tab and observe what the UI would have written back —
// the missing rung that lets `tools/roosttest/` cover OSC drains,
// reply round-trips, and other byte-level wiring end-to-end. See
// `docs/development/test-automation.md` §5.4 for the full rationale.
//
// `tab.dump_resolved` is NOT gated: it's a richer read of the same
// render state `tab.dump` already exposes, useful to anyone debugging
// "why is this row gray." The resolver walk it pins is exactly the
// one the production paint path runs, so it doubles as the
// regression net for the bold-color resolver call site (#142).

/// `tab.feed_pty_bytes` request: inject bytes into a tab's PTY-output
/// drain as if the supervisor had emitted them. Indistinguishable
/// from real PTY output to the OSC scanner + libghostty — same
/// `TabOutput` channel, same downstream handlers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabFeedPtyBytesParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    /// Raw bytes encoded as base64. See `bytes_base64`.
    #[serde(with = "bytes_base64")]
    pub data: Vec<u8>,
}

/// `tab.capture_pty_input` request: return the bytes the UI has
/// queued onto this tab's PTY-input channel (keystrokes, paste,
/// synthesized OSC replies). `drain=true` consumes the buffer;
/// `drain=false` peeks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabCapturePtyInputParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    #[serde(default)]
    pub drain: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCapturePtyInputResult {
    /// Captured input bytes, base64-encoded on the wire.
    #[serde(with = "bytes_base64")]
    pub data: Vec<u8>,
}

/// `tab.dump_resolved` request: walk a tab's render state through
/// the same resolver the production paint path uses (including the
/// theme's bold-color override).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabDumpResolvedParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

/// `tab.dump_resolved` response: post-resolver per-cell view of the
/// terminal grid. Fg/bg are `#RRGGBB` to keep the JSON human-readable
/// for test assertions. `has_explicit_bg` tracks whether the bg came
/// from an SGR cell vs the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabDumpResolvedResult {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<ResolvedCell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCell {
    pub row: u32,
    pub col: u16,
    pub text: String,
    /// `#RRGGBB`.
    pub fg: String,
    /// `#RRGGBB`.
    pub bg: String,
    pub has_explicit_bg: bool,
    pub bold: bool,
    pub italic: bool,
    pub inverse: bool,
}

// ============================================================================
// Project lifecycle
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCreateParams {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCreateResult {
    pub project: Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRenameParams {
    #[serde(with = "string_int64")]
    pub project_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDeleteParams {
    #[serde(with = "string_int64")]
    pub project_id: i64,
}

// ============================================================================
// Reorder
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabReorderParams {
    #[serde(with = "string_int64")]
    pub project_id: i64,
    #[serde(with = "vec_string_int64")]
    pub tab_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectReorderParams {
    #[serde(with = "vec_string_int64")]
    pub project_ids: Vec<i64>,
}

// ============================================================================
// Control
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabFocusParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabFocusResult {
    #[serde(with = "string_int64")]
    pub previous_project_id: i64,
    #[serde(with = "string_int64")]
    pub previous_tab_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSetTitleParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSetStateParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub state: TabState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabClearNotificationParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSetHookActiveParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationCreateParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
}

// ============================================================================
// Events subscribe (stubbed M0..M2)
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribeParams {
    /// Restrict to a single tab. `"0"` (or absent) means all events.
    #[serde(with = "string_int64", default)]
    pub tab_id_filter: i64,
}

/// `app.activate` carries no params. Declared (empty + strict) so the
/// handler validates the envelope like every other op rather than
/// ACK-ing arbitrary payloads (#80).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppActivateParams {}

/// `app.screenshot` request. `scale` is the pixel multiplier — `1`
/// renders at logical window size (the default; already the resolution
/// a vision model consumes after its own downsample), `2` super-samples
/// for a human zooming in. The UI rejects anything outside `1..=2`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotParams {
    #[serde(default = "default_screenshot_scale")]
    pub scale: u32,
}

fn default_screenshot_scale() -> u32 {
    1
}

impl Default for ScreenshotParams {
    fn default() -> Self {
        Self {
            scale: default_screenshot_scale(),
        }
    }
}

/// `app.screenshot` response. `png` is the raw PNG bytes (base64 on the
/// wire); `width`/`height` are the pixel dimensions actually rendered
/// (== logical size × `scale`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotResult {
    #[serde(with = "bytes_base64")]
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale: u32,
}

// ============================================================================
// Event data types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabOpenedEvent {
    pub tab: Tab,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabClosedEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabStateChangedEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub state: TabState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabTitleChangedEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCwdChangedEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabNotificationEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub has_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCreatedEvent {
    pub project: Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRenamedEvent {
    #[serde(with = "string_int64")]
    pub project_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDeletedEvent {
    #[serde(with = "string_int64")]
    pub project_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveChangedEvent {
    #[serde(with = "string_int64")]
    pub project_id: i64,
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookActiveChangedEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationFiredEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
}

// ============================================================================
// Operation name constants — used by client + server dispatcher
// ============================================================================

pub mod ops {
    pub const IDENTIFY: &str = "identify";
    pub const TAB_OPEN: &str = "tab.open";
    pub const TAB_CLOSE: &str = "tab.close";
    pub const TAB_LIST: &str = "tab.list";
    pub const TAB_WRITE: &str = "tab.write";
    pub const TAB_RESIZE: &str = "tab.resize";
    pub const TAB_DUMP: &str = "tab.dump";
    pub const PROJECT_CREATE: &str = "project.create";
    pub const PROJECT_RENAME: &str = "project.rename";
    pub const PROJECT_DELETE: &str = "project.delete";
    pub const TAB_REORDER: &str = "tab.reorder";
    pub const PROJECT_REORDER: &str = "project.reorder";
    pub const TAB_FOCUS: &str = "tab.focus";
    pub const TAB_SET_TITLE: &str = "tab.set_title";
    pub const TAB_SET_STATE: &str = "tab.set_state";
    pub const TAB_CLEAR_NOTIFICATION: &str = "tab.clear_notification";
    pub const TAB_SET_HOOK_ACTIVE: &str = "tab.set_hook_active";
    pub const NOTIFICATION_CREATE: &str = "notification.create";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// Raise + focus the running UI window. Sent by a second launch
    /// that loses the single-instance flock; takes no params (#6).
    pub const APP_ACTIVATE: &str = "app.activate";
    /// Render the running UI's whole window (sidebar + tabs + active
    /// terminal) to a PNG, in-process. Returns the bytes base64-encoded
    /// so an agent can `see` the live UI without OS screen capture.
    pub const SCREENSHOT: &str = "app.screenshot";

    /// Command-palette overlay: open a root frame, read the current
    /// frame's rows, set the filter, activate a row (same dispatch as its
    /// keybind), and dismiss. Each replies with the resulting palette
    /// state. UI-only — routed through the UI seam, not the workspace.
    pub const PALETTE_OPEN: &str = "palette.open";
    pub const PALETTE_STATE: &str = "palette.state";
    pub const PALETTE_QUERY: &str = "palette.query";
    pub const PALETTE_ACTIVATE: &str = "palette.activate";
    pub const PALETTE_DISMISS: &str = "palette.dismiss";

    /// Selection + clipboard test ops — drive selection state and
    /// read/seed the host clipboard end-to-end. See module docs above
    /// the corresponding param structs for the contract.
    pub const SELECTION_SET: &str = "selection.set";
    pub const SELECTION_CLEAR: &str = "selection.clear";
    pub const SELECTION_DUMP: &str = "selection.dump";
    pub const CLIPBOARD_DUMP: &str = "clipboard.dump";
    pub const CLIPBOARD_WRITE: &str = "clipboard.write";

    /// Test-only PTY drain ops — drive bytes through the OSC scanner,
    /// libghostty, and the input-reply path. Gated behind
    /// `ROOST_TEST_MODE=1` (set in CI for `e2e-gtk` and `e2e-mac`)
    /// because injecting arbitrary PTY output and observing keystroke
    /// bytes is something only a test harness should do.
    pub const TAB_FEED_PTY_BYTES: &str = "tab.feed_pty_bytes";
    pub const TAB_CAPTURE_PTY_INPUT: &str = "tab.capture_pty_input";
    /// Ungated companion: a richer read of the same render state
    /// `tab.dump` already exposes. Pins the resolver call site
    /// (theme bold-color → `resolve_cell_colors`) end-to-end.
    pub const TAB_DUMP_RESOLVED: &str = "tab.dump_resolved";

    pub const EVENT_TAB_OPENED: &str = "tab.opened";
    pub const EVENT_TAB_CLOSED: &str = "tab.closed";
    pub const EVENT_TAB_STATE_CHANGED: &str = "tab.state_changed";
    pub const EVENT_TAB_TITLE_CHANGED: &str = "tab.title_changed";
    pub const EVENT_TAB_CWD_CHANGED: &str = "tab.cwd_changed";
    pub const EVENT_TAB_NOTIFICATION: &str = "tab.notification";
    pub const EVENT_PROJECT_CREATED: &str = "project.created";
    pub const EVENT_PROJECT_RENAMED: &str = "project.renamed";
    pub const EVENT_PROJECT_DELETED: &str = "project.deleted";
    pub const EVENT_ACTIVE_CHANGED: &str = "active.changed";
    pub const EVENT_HOOK_ACTIVE_CHANGED: &str = "hook_active.changed";
    pub const EVENT_NOTIFICATION_FIRED: &str = "notification.fired";
}

// ============================================================================
// String-wrapped int64
// ============================================================================
//
// JSON numbers lose precision past 2^53; the proto schema used int64
// for tab/project ids. Encode as string on the wire.

pub mod string_int64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &i64, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<i64, D::Error> {
        let raw = String::deserialize(de)?;
        raw.parse::<i64>()
            .map_err(|_| serde::de::Error::custom(format!("invalid int64 string: {raw}")))
    }
}

pub mod vec_string_int64 {
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(values: &[i64], ser: S) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(Some(values.len()))?;
        for v in values {
            seq.serialize_element(&v.to_string())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<i64>, D::Error> {
        let raw = Vec::<String>::deserialize(de)?;
        raw.into_iter()
            .map(|s| {
                s.parse::<i64>()
                    .map_err(|_| serde::de::Error::custom(format!("invalid int64 string: {s}")))
            })
            .collect()
    }
}

// ============================================================================
// Bytes-as-base64
// ============================================================================
//
// Standard alphabet, no padding stripping. Binary-clean per the
// `tests/binary_fidelity.rs` roundtrip suite.

pub mod bytes_base64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&STANDARD.encode(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let raw = String::deserialize(de)?;
        STANDARD
            .decode(raw.as_bytes())
            .map_err(|e| serde::de::Error::custom(format!("invalid base64: {e}")))
    }
}

// ============================================================================
// Unit tests — schema sanity (round-trip serialize→deserialize)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
    >(
        value: &T,
    ) {
        let json = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, &back, "round-trip mismatch via {}", json);
    }

    #[test]
    fn tab_state_serializes_as_snake_case() {
        let json = serde_json::to_string(&TabState::NeedsInput).unwrap();
        assert_eq!(json, "\"needs_input\"");
        let back: TabState = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(back, TabState::Running);
    }

    #[test]
    fn tab_round_trip() {
        let t = Tab {
            id: 12345,
            project_id: 67890,
            title: "shell".into(),
            cwd: "/home/me".into(),
            state: TabState::Running,
            has_notification: false,
            is_active: true,
            user_titled: false,
            position: 0,
            created_at: 1_700_000_000,
            last_active: 1_700_000_500,
            hook_active: false,
        };
        round_trip(&t);
        let json = serde_json::to_string(&t).unwrap();
        assert!(
            json.contains("\"id\":\"12345\""),
            "id must be string: {json}"
        );
    }

    #[test]
    fn project_round_trip() {
        let p = Project {
            id: 1,
            name: "Roost".into(),
            cwd: "/Users/me/projects/roost".into(),
            position: 0,
            created_at: 1_700_000_000,
            tabs: vec![],
        };
        round_trip(&p);
    }

    #[test]
    fn raw_request_round_trip() {
        let raw = RawRequest {
            id: 42,
            op: "tab.open".into(),
            params: serde_json::json!({"project_id": "1", "cols": 100, "rows": 30}),
        };
        round_trip(&raw);
    }

    #[test]
    fn raw_request_rejects_unknown_envelope_fields() {
        let bad = r#"{"id":"1","op":"x","params":{},"extra":1}"#;
        assert!(serde_json::from_str::<RawRequest>(bad).is_err());
    }

    #[test]
    fn tab_open_params_reject_unknown() {
        let bad = r#"{"project_id":"1","cols":100,"rows":30,"badfield":true}"#;
        assert!(serde_json::from_str::<TabOpenParams>(bad).is_err());
    }

    #[test]
    fn int64_max_round_trips_via_string_wrapper() {
        let raw = RawRequest {
            id: i64::MAX,
            op: "identify".into(),
            params: empty_object(),
        };
        let json = serde_json::to_string(&raw).unwrap();
        assert!(json.contains(&format!("\"{}\"", i64::MAX)));
        let back: RawRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, i64::MAX);
    }

    #[test]
    fn vec_string_int64_round_trips() {
        let p = TabReorderParams {
            project_id: 1,
            tab_ids: vec![3, 2, 1],
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("[\"3\",\"2\",\"1\"]"), "got: {json}");
        round_trip(&p);
    }

    #[test]
    fn bytes_base64_round_trip() {
        let p = TabWriteParams {
            tab_id: 5,
            data: b"hello\n".to_vec(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"data\":\"aGVsbG8K\""), "got: {json}");
        round_trip(&p);
    }

    /// `tab.feed_pty_bytes` params: tab_id is the string-int64
    /// wrapper + data is base64 — same shape as `tab.write` so the
    /// existing round-trip helper covers it. Drift between this and
    /// the wire vector under `tests/ipc-vectors/` would be caught
    /// by the vector loader; pinning the struct's own shape too
    /// surfaces failures even closer to the source.
    #[test]
    fn tab_feed_pty_bytes_params_round_trip() {
        let p = TabFeedPtyBytesParams {
            tab_id: 5,
            data: b"\x1b]11;rgb:00/11/22\x07".to_vec(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"tab_id\":\"5\""), "got: {json}");
        // Wire format is base64; sanity-check that the payload is
        // not the raw escape sequence.
        assert!(!json.contains("\\x1b"), "got: {json}");
        round_trip(&p);
    }

    /// `tab.capture_pty_input` params: `drain` defaults to false
    /// (peek semantics) when omitted, matching the Mac side's
    /// `decodeIfPresent ?? false`. Tested explicitly because a
    /// silent default flip would break the test harness's
    /// drain-then-assert pattern.
    #[test]
    fn tab_capture_pty_input_params_default_drain_is_false() {
        let p: TabCapturePtyInputParams = serde_json::from_str(r#"{"tab_id":"5"}"#).unwrap();
        assert_eq!(p.tab_id, 5);
        assert!(!p.drain);
        round_trip(&p);
    }

    /// Result struct's `data` field is base64 on the wire — same
    /// helper as the params side, separate test so a future schema
    /// change (e.g. dropping base64 in favor of escaped bytes)
    /// breaks loudly.
    #[test]
    fn tab_capture_pty_input_result_round_trips_base64() {
        let r = TabCapturePtyInputResult {
            data: b"\x00\x01\x02\xfe\xff".to_vec(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"data\":\"AAEC/v8=\""), "got: {json}");
        round_trip(&r);
    }

    /// `deny_unknown_fields` on `TabDumpResolvedParams` rejects
    /// stray keys so a typo in a test client surfaces immediately
    /// rather than getting silently dropped.
    #[test]
    fn tab_dump_resolved_params_reject_unknown_field() {
        let bad = r#"{"tab_id":"5","extra":"x"}"#;
        assert!(serde_json::from_str::<TabDumpResolvedParams>(bad).is_err());
    }

    /// Result struct's resolved-cell list is permissive (no
    /// `deny_unknown_fields` on `TabDumpResolvedResult`), so the
    /// server can add per-cell fields (underline, faint, …) without
    /// breaking older clients. Test the negative — extra fields
    /// must NOT fail.
    #[test]
    fn tab_dump_resolved_result_accepts_extra_fields() {
        let json = r#"{"cols":80,"rows":24,"cells":[],"future_field":42}"#;
        assert!(serde_json::from_str::<TabDumpResolvedResult>(json).is_ok());
    }

    #[test]
    fn screenshot_params_default_scale_is_one() {
        let p: ScreenshotParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.scale, 1);
        assert_eq!(ScreenshotParams::default().scale, 1);
        round_trip(&ScreenshotParams { scale: 2 });
    }

    #[test]
    fn tab_dump_round_trips_and_cursor_is_optional() {
        let p: TabDumpParams = serde_json::from_str(r#"{"tab_id":"7"}"#).unwrap();
        assert_eq!(p.tab_id, 7);
        round_trip(&p);

        let with_cursor = TabDumpResult {
            cols: 80,
            rows: 24,
            cursor: Some(TabDumpCursor {
                row: 1,
                col: 14,
                visible: true,
            }),
            rows_text: vec!["/tmp $ echo hi".into(), "hi".into()],
        };
        round_trip(&with_cursor);

        // cursor omitted entirely when None (skip_serializing_if).
        let no_cursor = TabDumpResult {
            cols: 80,
            rows: 24,
            cursor: None,
            rows_text: vec![],
        };
        let json = serde_json::to_string(&no_cursor).unwrap();
        assert!(
            !json.contains("cursor"),
            "None cursor must be omitted: {json}"
        );
        round_trip(&no_cursor);
    }

    #[test]
    fn palette_round_trips_and_closed_state_is_minimal() {
        let open: PaletteOpenParams = serde_json::from_str(r#"{"kind":"launcher"}"#).unwrap();
        assert_eq!(open.kind, "launcher");
        round_trip(&open);
        // kind defaults to empty (the command palette) when omitted.
        round_trip(&PaletteOpenParams::default());
        round_trip(&PaletteQueryParams {
            query: "the".into(),
        });
        round_trip(&PaletteActivateParams {
            id: "new_tab".into(),
        });
        round_trip(&PaletteStateParams {});
        round_trip(&PaletteDismissParams {});
        // Nullary palette ops reject stray fields (strict envelope).
        assert!(serde_json::from_str::<PaletteStateParams>(r#"{"x":1}"#).is_err());

        let live = PaletteStateResult {
            open: true,
            frame: Some("commands".into()),
            query: "tab".into(),
            selection: 2,
            items: vec![
                PaletteItemView {
                    id: "new_tab".into(),
                    title: "New Tab".into(),
                    subtitle: None,
                },
                PaletteItemView {
                    id: "n:7".into(),
                    title: "Build done".into(),
                    subtitle: Some("exit 0".into()),
                },
            ],
        };
        round_trip(&live);

        // Closed: `frame` omitted (skip_serializing_if), defaults restore it.
        let closed = PaletteStateResult::default();
        let json = serde_json::to_string(&closed).unwrap();
        assert!(!json.contains("frame"), "closed state omits frame: {json}");
        round_trip(&closed);
    }

    #[test]
    fn screenshot_result_round_trip() {
        let r = ScreenshotResult {
            png: b"\x89PNG\r\n\x1a\n".to_vec(),
            width: 2800,
            height: 1800,
            scale: 2,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"png\":\"iVBORw0KGgo=\""), "got: {json}");
        round_trip(&r);
    }

    #[test]
    fn response_ok_and_err_envelopes_round_trip() {
        let ok = Response::ok(7, serde_json::json!({"status": "ok"}));
        round_trip(&ok);
        let err = Response::err(7, "unknown-op", "no such op: foo");
        round_trip(&err);
    }

    #[test]
    fn event_envelope_is_permissive_to_unknown_top_level_fields() {
        // EventEnvelope is server→client only; clients should ignore
        // unknown top-level fields so the server can add new fields
        // forward-compatibly. Server-side strictness lives on the
        // request path, not here.
        let extra = r#"{"event":"tab.opened","data":{},"extra":1}"#;
        let parsed: EventEnvelope = serde_json::from_str(extra).unwrap();
        assert_eq!(parsed.event, "tab.opened");
    }
}
