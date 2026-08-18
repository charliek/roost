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

use crate::agent::{AgentLifecycle, AgentTabState, Ownership, ShellState};

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
///
/// `state` and `hook_active` are **derived** from the three agent axes
/// below (`crate::agent::effective` / `crate::agent::is_live`), not
/// stored alongside them. They stay on the wire because every shipped
/// client reads them; `state` stays a closed four-value enum for the
/// reason spelled out on [`crate::agent::effective`].
///
/// The axes themselves carry `#[serde(default)]` without exception so an
/// older client decoding a newer server — and the reverse — keeps
/// working while the two UIs land in separate commits (plan 002 §3.6).
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
    #[serde(default)]
    pub shell_state: ShellState,
    #[serde(default)]
    pub agent_lifecycle: AgentLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<Ownership>,
}

impl Tab {
    /// The three agent axes as the one record `crate::agent` operates
    /// on. Consumers that need to re-derive (the sidebar rollup, a
    /// reconnecting UI rebuilding its per-tab cache) go through this
    /// rather than reading `state` / `hook_active` back off the wire.
    pub fn agent_state(&self) -> AgentTabState {
        AgentTabState {
            shell: self.shell_state,
            lifecycle: self.agent_lifecycle,
            ownership: self.ownership.clone(),
        }
    }
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
/// `agent` is present only on rows the agents frame (`kind: "agents"`)
/// builds — absent on every other row. Because this type is shared with
/// `palette.present` requests, an `agent` supplied there is ignored
/// (those rows render generic) — including a *malformed* one: the field
/// decodes leniently to `None` rather than failing the request, matching
/// the pre-agent behavior where the key was unknown and dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteItemView {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_agent_row"
    )]
    pub agent: Option<PaletteAgentRow>,
}

fn lenient_agent_row<'de, D>(de: D) -> Result<Option<PaletteAgentRow>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// The agents palette frame's per-row payload (§3.9). Wired from the
/// tab's `effective_lifecycle` (`crate::agent::effective_lifecycle`) —
/// the same value the tab pill and sidebar rollup render — so dot color,
/// status text, and rank can never disagree with them. `metrics_text` is
/// absent while a git-metrics probe is still pending and always present
/// once resolved (`"—"` or the formatted string), so pending vs. resolved
/// is observable on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteAgentRow {
    pub effective_lifecycle: AgentLifecycle,
    pub project: String,
    pub name: String,
    pub status_text: String,
    pub time_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics_text: Option<String>,
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
    /// Whether the highlighted row is fully within the scrolled
    /// viewport — the observable for the keep-selection-visible behavior
    /// (the palette scrolls the highlight into view as it moves). `None`
    /// when a UI can't report it (e.g. no selection, or a UI that
    /// doesn't expose list geometry); only set this `false` when the
    /// selected row is genuinely clipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_in_view: Option<bool>,
}

/// `palette.present` params: open the palette on a caller-supplied list
/// of rows and block until the user picks one or dismisses. This is the
/// programmatic twin of the command palette — a script (or a Roost
/// provider) hands Roost a list and Roost returns the chosen `id`. The
/// item shape is the same [`PaletteItemView`] the read ops emit, so the
/// "present a list / read a list" contract is one schema. `title` and
/// `placeholder` are optional chrome; `items` is required (an empty list
/// is rejected `invalid-param` — nothing to present).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PalettePresentParams {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub placeholder: String,
    pub items: Vec<PaletteItemView>,
}

/// Reply for `palette.present`: the user's choice. `selected_id` carries
/// the activated row's id; `dismissed` is true when the user closed the
/// palette without picking (Esc / focus loss / another palette opening
/// over it). Exactly one is meaningful — `dismissed: true` leaves
/// `selected_id` `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PalettePresentResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_id: Option<String>,
    #[serde(default)]
    pub dismissed: bool,
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

/// `tab.feed_ime` request: drive an IME preedit/commit/session-boundary
/// event through the terminal's active keyboard route — the same
/// production path (`ime_preedit` / `ime_commit` / `ime_session_boundary`)
/// a real input-method event takes. Routes by the UI's keyboard route,
/// not directly by `tab_id`: `tab_id` must match the tab currently
/// holding the route, so a stale/wrong id fails loudly instead of
/// silently feeding the wrong tab. Gated like `tab.feed_pty_bytes`
/// (ROOST_TEST_MODE=1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabFeedImeParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    /// `"preedit" | "commit" | "clear"`.
    pub action: String,
    #[serde(default)]
    pub text: String,
    /// Byte offsets into `text` for the preedit cursor/underline span.
    /// Both must be given together — one without the other, or
    /// `cursor_start > cursor_end`, is rejected with `invalid-param`.
    #[serde(default)]
    pub cursor_start: Option<usize>,
    #[serde(default)]
    pub cursor_end: Option<usize>,
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

/// `tab.expand_selection_at` request: drive the same double-/triple-
/// click word/line expansion the UI runs from a real mouse press,
/// then commit the resulting span as the tab's selection. Gated like
/// `tab.feed_pty_bytes` (ROOST_TEST_MODE=1) — promotes to a real op
/// later if hooks need it. Mirrors the `handle_click_count` /
/// `handleClickCount` shape on each port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabExpandSelectionAtParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub col: u16,
    pub row: u16,
    /// 2 → double-click (word), 3+ → triple-click (line). Values
    /// below 2 are rejected with `invalid-param`.
    pub click_count: u8,
}

/// `tab.dispatch_mouse_event` request: drive a synthetic mouse event
/// into the UI's mouse handler at cell-grid coordinates. Same path
/// the real NSEvent / GestureClick takes — gating on the negotiated
/// mouse-tracking mode, encoder choice, and SGR / X10 / pixel
/// formats happens inside the handler so this op tests exactly what
/// production does.
///
/// `kind` is one of `"press"`, `"release"`, `"motion"`. `button` is
/// `"left" | "right" | "middle" | "wheel_up" | "wheel_down" | "none"`
/// (use `"none"` for motion-without-button events under mode 1003).
/// `cell_x` / `cell_y` are 0-indexed grid coordinates. `mods` carries
/// the same bit layout as the key encoder's `Mods`. Gated by
/// `ROOST_TEST_MODE=1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabDispatchMouseEventParams {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    /// `"press" | "release" | "motion"`.
    pub kind: String,
    /// `"left" | "right" | "middle" | "wheel_up" | "wheel_down" | "none"`.
    pub button: String,
    pub cell_x: u32,
    pub cell_y: u32,
    /// Bit layout matches the key encoder's `Mods`: shift(0), ctrl(1),
    /// alt(2), cmd/super(3). `0` for no modifiers.
    #[serde(default)]
    pub mods: u32,
}

/// `app.set_window_focus` request: drive the focus-tracking emit
/// path without actually moving OS focus. When mode 1004 is on, the
/// UI writes `\x1b[I` / `\x1b[O` onto the tab's input channel; tests
/// pick those up via `tab.capture_pty_input`. Gated by
/// `ROOST_TEST_MODE=1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSetWindowFocusParams {
    pub focus: bool,
}

/// `app.cursor_shape` request: return the W3C cursor name the UI is
/// currently applying for the active tab. A transient UI-owned link hover may
/// override the last-seen OSC 22 payload; otherwise returns that payload, or
/// `"default"` if no shape has been requested yet
/// (and `"default"` for the empty-string reset form, so callers can
/// always assert against a non-empty name). Not gated — read-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppCursorShapeParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppCursorShapeResult {
    /// W3C cursor name (`default`, `pointer`, `text`, …).
    pub shape: String,
}

/// `app.active_terminal_focused` request: report whether the active
/// tab's terminal owns the UI's *logical* keyboard route. This is
/// intentionally separate from native toplevel/compositor focus, so
/// callers can distinguish terminal input ownership from application
/// activation. Not gated — read-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppActiveTerminalFocusedParams {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppActiveTerminalFocusedResult {
    /// True when keyboard input is logically routed to the active
    /// terminal. False when an overlay owns input or no terminal exists.
    pub focused: bool,
}

/// `app.selected_tab_id` params. Read-only, not gated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSelectedTabIdParams {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSelectedTabIdResult {
    /// The tab id currently selected in the active project's AdwTabView
    /// (the on-screen tab — UI truth, independent of the core's active
    /// tab). `0` when there is no selection. Lets tests assert the UI
    /// selection and the workspace core agree.
    #[serde(with = "string_int64")]
    pub tab_id: i64,
}

/// `app.dock_badge` request: read the macOS Dock tile's live badge
/// label. Gated like `tab.feed_pty_bytes` (ROOST_TEST_MODE=1), and
/// implemented only by the macOS iced UI — every other UI answers
/// `not-implemented`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppDockBadgeParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppDockBadgeResult {
    /// The badge text AppKit currently holds — the notification-inbox
    /// count as a decimal string, or `null` when the badge is cleared
    /// (the UI writes `nil` at zero, matching `App.swift`). Read back
    /// off the Dock tile, never recomputed from the inbox.
    pub label: Option<String>,
}

/// `app.menu_dump` request: read back the live native menu bar the
/// macOS iced UI installed. Gated like `app.dock_badge`, and
/// implemented only by the macOS iced UI — every other UI answers
/// `not-implemented`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppMenuDumpParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppMenuDumpResult {
    /// One entry per top-level menu bar item (App/File/View/Edit/
    /// Window), in `NSMenu` order.
    pub menus: Vec<MenuDump>,
}

/// One top-level menu and its items, read straight off the live
/// `NSMenu` — nothing here is re-derived from the keybind table, so a
/// table/menu drift bug shows up as a dump mismatch instead of being
/// papered over.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuDump {
    /// The submenu's own title. For the App menu this is the profile
    /// display name — AppKit substitutes it for display, but the title
    /// string itself (what this reads) is set to the display name at
    /// install time, so no separate normalization step is needed here.
    pub title: String,
    pub items: Vec<MenuItemDump>,
}

/// One menu item, read straight off the live `NSMenuItem`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuItemDump {
    pub title: String,
    /// The raw `keyEquivalent` string AppKit holds (empty when the item
    /// has none, or when its equivalent is currently blanked by the
    /// gating seam).
    pub key_equivalent: String,
    /// Fixed vocabulary, always in this order:
    /// `["shift","ctrl","alt","super"]`.
    pub modifiers: Vec<String>,
    pub enabled: bool,
    /// `"on"` or `"off"` — `NSControlStateValueMixed` never appears;
    /// nothing in this menu bar ever sets it, and a dump that saw it
    /// would be an internal error.
    pub state: String,
    pub separator: bool,
    /// The wire name of the bound `KeybindAction`
    /// (`KeybindAction::to_wire_name()`), a `"select_project:<id>"` /
    /// `"select_tab:<id>"` Window-row marker, the `"quit"` /
    /// `"check_for_updates"` markers, an `"appkit:<selector>"`
    /// standard-selector item, or `null` for an inert item (Cut, Select
    /// All, separators).
    pub action: Option<String>,
}

/// `app.menu_activate` request: resolve `path` (a title path, e.g.
/// `["File", "New Tab"]`) through the live native menu bar and fire it
/// via `performActionForItemAtIndex:` — the same dispatch a real click
/// takes. Titles carry real ellipses (U+2026), not `...`. Errors on an
/// unknown path, an ambiguous one (two items sharing a title at the
/// same level — dynamic Window rows can collide; seed unique names),
/// or a disabled item (`performActionForItemAtIndex:` runs no
/// validation itself, so this op checks `isEnabled` first). Gated and
/// platform-restricted like `app.menu_dump`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppMenuActivateParams {
    pub path: Vec<String>,
}

/// `app.update_status` request: read back the Sparkle updater's state
/// from the macOS iced UI's seam. Gated and platform-restricted like
/// `app.menu_dump`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppUpdateStatusParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUpdateStatusResult {
    /// Whether `Sparkle.framework` was found beside the executable and
    /// `dlopen`ed. False for every bare-binary build — the framework
    /// only ever ships inside `Roost-Iced.app` (plan 028 § 3.8).
    pub framework_loaded: bool,
    /// `"started"` or `"unavailable"`. Started means `-startUpdater:`
    /// succeeded, which is also what the "Check for Updates…" menu
    /// item's enabled state is derived from.
    pub updater: String,
    /// Why the updater is unavailable (no framework, a refused start),
    /// or `null` once it started.
    pub reason: Option<String>,
    /// Increments once per completed check, so a poll cannot pass on a
    /// stale `last_check` from an earlier one — condition-wait on this
    /// advancing rather than on `last_check` becoming non-null.
    pub check_id: i64,
    pub last_check: Option<UpdateCheckDump>,
}

/// The outcome of one completed update check, recorded by the seam's
/// `SPUUpdaterDelegate` callbacks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckDump {
    /// `"found"` (a newer version is in the appcast), `"none"` (the
    /// feed parsed and offered nothing newer) or `"error"` (no feed, an
    /// unreachable one, a malformed appcast).
    pub outcome: String,
    /// The found update's version, from `SUAppcastItem`'s
    /// `displayVersionString`. Only set for `"found"`.
    pub version: Option<String>,
    /// The reporting error's `localizedDescription`, when there was one.
    pub detail: Option<String>,
}

/// `app.update_check` request: start a non-interactive
/// `-[SPUUpdater checkForUpdateInformation]` — no UI, no download. The
/// result lands in `app.update_status` via the delegate callbacks, so
/// callers condition-wait on `check_id` advancing. Errors when the
/// updater is unavailable. Gated and platform-restricted like
/// `app.menu_dump`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppUpdateCheckParams {}

/// `app.notification_status` request: read back the macOS iced UI's
/// `UNUserNotificationCenter` backend state
/// (`crates/roost-iced/src/macos/notifications.rs`). Gated and
/// platform-restricted like `app.menu_dump`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppNotificationStatusParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppNotificationStatusResult {
    /// `"available"` once the UN delegate installed (a bundled launch
    /// that has reached `window_opened`), `"unavailable"` otherwise —
    /// every bare-binary build, and a bundled app before its first
    /// window opens.
    pub backend: String,
    /// Why the backend is unavailable — no app bundle, or the window
    /// has not opened yet — or `null` once it is available.
    pub reason: Option<String>,
    /// The user's answer to the authorization prompt. Always `false`
    /// while unavailable. CI's TCC authorization state is unknowable,
    /// so nothing in the automated suite asserts this `true`.
    pub authorized: bool,
}

/// `tab.expand_selection_at` response: the committed selection's
/// bounds, mirroring `WordSpan`. `text` is the extracted selection
/// content (same path `selection.dump` uses), or `None` when the
/// span turned out to be a single-cell selection that the renderer
/// reports as empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabExpandSelectionAtResult {
    pub col0: u16,
    pub col1: u16,
    pub text: Option<String>,
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

/// `tab.agent_report` reply. The params live in
/// [`crate::agent::TabAgentReportParams`] alongside the state machine
/// that consumes them.
///
/// `accepted` is false when the report lost the ownership check (plan
/// §3.3) — the tab is then returned unchanged. Callers get the post-
/// report `Tab` back so an adapter never needs a follow-up `tab.list`
/// to see what its report did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabAgentReportResult {
    pub accepted: bool,
    pub tab: Tab,
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

/// `app.window_metrics` request — nullary envelope (`{}`). The struct
/// is empty but `deny_unknown_fields` rejects strays so a typo in the
/// client surfaces immediately, matching every other op's contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowMetricsParams {}

/// `app.window_metrics` response — logical (point) measurements of the
/// running UI's window + sidebar. Used by the sidebar layout regression
/// tests to assert the sidebar holds its width across window resizes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowMetricsResult {
    pub window_width: f64,
    pub window_height: f64,
    pub sidebar_width: f64,
    pub sidebar_collapsed: bool,
    /// Application-owned top edge of the terminal viewport, when the UI can
    /// report it exactly. Optional so older and native-toolkit adapters keep
    /// their existing response shape until they expose equivalent geometry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_top: Option<f64>,
    /// Renderer-resolved terminal family, when the adapter can report it.
    /// This is diagnostic state rather than a config echo: fallback chains
    /// report the installed family actually used by the live terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_font_family: Option<String>,
}

/// `app.sidebar_dump` request — nullary envelope (`{}`), matching
/// `WindowMetricsParams`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarDumpParams {}

/// One agent row as actually rendered under a project in the sidebar
/// (plan 007 §3.8), plus whether it is the active tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarDumpAgentRow {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    pub name: String,
    pub lifecycle: AgentLifecycle,
    pub status_text: String,
    pub time_text: String,
    pub is_active: bool,
}

/// One project's rendered agent rows, in sidebar order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarDumpProject {
    #[serde(with = "string_int64")]
    pub project_id: i64,
    pub agents: Vec<SidebarDumpAgentRow>,
}

/// `app.sidebar_dump` response — the sidebar's **last-rendered** agent
/// rows, read from the same per-project cache the sidebar paints from
/// (`RenderedAgentRow` on both UIs), not re-derived from the workspace
/// snapshot. That makes a missed refresh a wire-visible test failure
/// instead of an invisible one. `agents_visible` is the config/feature
/// toggle only; `projects[].agents` stays populated when the toggle is
/// off or mid-drag — those are transient UI state, not part of this
/// contract. All projects appear, in sidebar order, including ones
/// with zero agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarDumpResult {
    pub agents_visible: bool,
    pub projects: Vec<SidebarDumpProject>,
}

/// `app.render_stats` request — read the running UI's render-path
/// counters. `reset` zeroes them *after* the read, so a caller can
/// read-reset, run a workload, then read the delta directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppRenderStatsParams {
    #[serde(default)]
    pub reset: bool,
}

/// `app.render_stats` response — running totals since process start
/// (or the last `reset: true`).
///
/// Every counter is string-wrapped: the nanosecond accumulators pass
/// 2^53 after roughly 104 days of measured render time, and a JSON
/// number would silently lose precision in any JS client. The
/// remaining counters ride the same convention so the shape is
/// uniform rather than half-and-half.
///
/// Permissive (no `deny_unknown_fields`) like every other result
/// struct, so a newer UI can add counters without breaking older
/// clients.
///
/// `view_calls`/`view_nanos`/`elide_calls`/`elide_nanos` carry `default`
/// on top of `string_int64`: the mac Swift handler doesn't send them, so
/// a decode against that response must still succeed (same tolerance
/// pattern as `TabOpenParams.project_id`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRenderStatsResult {
    #[serde(with = "string_int64")]
    pub refresh_calls: i64,
    #[serde(with = "string_int64")]
    pub refresh_nanos: i64,
    #[serde(with = "string_int64")]
    pub rows_rebuilt: i64,
    #[serde(with = "string_int64")]
    pub cells_walked: i64,
    #[serde(with = "string_int64")]
    pub draw_calls: i64,
    #[serde(with = "string_int64")]
    pub draw_nanos: i64,
    #[serde(with = "string_int64")]
    pub fill_text_calls: i64,
    #[serde(with = "string_int64", default)]
    pub view_calls: i64,
    #[serde(with = "string_int64", default)]
    pub view_nanos: i64,
    #[serde(with = "string_int64", default)]
    pub elide_calls: i64,
    #[serde(with = "string_int64", default)]
    pub elide_nanos: i64,
}

/// `window.resize` request — programmatically set the window's logical
/// size. Test-mode only (gated by `ROOST_TEST_MODE=1`); see the op
/// const comment block below for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowResizeParams {
    pub width: f64,
    pub height: f64,
}

/// `sidebar.set_width` request — programmatically set the projects
/// sidebar's logical width. The UI clamps through the workspace to
/// `SIDEBAR_MIN_WIDTH..=SIDEBAR_MAX_WIDTH`, so an out-of-band value
/// lands at the nearest bound rather than being rejected. Test-mode
/// only (gated by `ROOST_TEST_MODE=1`); see the op const comment block
/// below for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarSetWidthParams {
    pub width: f64,
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

/// `agent_report.changed` — the full agent record after an accepted
/// report or shell mark.
///
/// `TabStateChangedEvent` and `HookActiveChangedEvent` still fire for
/// their (derived) slices so existing consumers keep working; this
/// carries what those two projections lose — which lifecycle, whose
/// session, and the shell axis underneath.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReportChangedEvent {
    #[serde(with = "string_int64")]
    pub tab_id: i64,
    #[serde(default)]
    pub shell_state: ShellState,
    #[serde(default)]
    pub agent_lifecycle: AgentLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<Ownership>,
    /// The projection this record produces on `Tab.state`, so a
    /// subscriber never re-derives it.
    pub state: TabState,
    pub hook_active: bool,
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
    /// The single op every agent adapter writes through: ownership
    /// claim/preserve/release + lifecycle + attention, applied under
    /// session scoping. Params + state machine live in
    /// [`crate::agent`].
    pub const TAB_AGENT_REPORT: &str = "tab.agent_report";
    pub const NOTIFICATION_CREATE: &str = "notification.create";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// Raise + focus the running UI window. Sent by a second launch
    /// that loses the single-instance flock; takes no params (#6).
    pub const APP_ACTIVATE: &str = "app.activate";
    /// Render the running UI's whole window (sidebar + tabs + active
    /// terminal) to a PNG, in-process. Returns the bytes base64-encoded
    /// so an agent can `see` the live UI without OS screen capture.
    pub const SCREENSHOT: &str = "app.screenshot";
    /// Logical (point) measurements of the running UI's window + sidebar.
    /// Used by the sidebar-holds-width regression tests; always available
    /// (no test-mode gate — it's read-only).
    pub const WINDOW_METRICS: &str = "app.window_metrics";
    /// The sidebar's last-rendered agent rows, per project, plus the
    /// agents-visible toggle. Read-only, ungated: it reads the same
    /// per-project cache the sidebar paints from, so a missed refresh
    /// is a wire-visible test failure rather than an invisible one
    /// (plan 007 §3.8).
    pub const SIDEBAR_DUMP: &str = "app.sidebar_dump";
    /// Read the running UI's render-path counters (refresh + draw call
    /// counts, elapsed nanos, rows/cells walked, `fill_text` calls),
    /// optionally zeroing them after the read. Ungated for the same
    /// reason as `tab.dump_resolved`: reading counters mutates nothing
    /// a user can see. The draw-side numbers only exist in a running
    /// app — `TerminalWidget::draw` needs a live renderer, which unit
    /// tests can't construct — so this op is the only way to measure
    /// the real draw path.
    pub const APP_RENDER_STATS: &str = "app.render_stats";

    /// Command-palette overlay: open a root frame, read the current
    /// frame's rows, set the filter, activate a row (same dispatch as its
    /// keybind), and dismiss. Each replies with the resulting palette
    /// state. UI-only — routed through the UI seam, not the workspace.
    pub const PALETTE_OPEN: &str = "palette.open";
    pub const PALETTE_STATE: &str = "palette.state";
    pub const PALETTE_QUERY: &str = "palette.query";
    pub const PALETTE_ACTIVATE: &str = "palette.activate";
    pub const PALETTE_DISMISS: &str = "palette.dismiss";
    /// Open the palette on a caller-supplied list and block until the
    /// user picks a row or dismisses. The programmatic twin of the
    /// command palette: a script or a Roost provider hands Roost the
    /// rows and gets back the chosen id. UI-only — routed through the
    /// UI seam, not the workspace.
    pub const PALETTE_PRESENT: &str = "palette.present";

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
    /// Test-only IME driver. Same gate as `tab.feed_pty_bytes`; drives
    /// the production `ime_preedit` / `ime_commit` /
    /// `ime_session_boundary` path so the e2e suite can pin composed
    /// text (CJK, emoji, accents) without a real input method.
    pub const TAB_FEED_IME: &str = "tab.feed_ime";
    /// Programmatically resize the running UI's window. Gated for the
    /// same reason as the PTY drain ops: harness-only driver for the
    /// sidebar layout regression suite (`tools/roosttest`); a real user
    /// resizes the window themselves.
    pub const WINDOW_RESIZE: &str = "window.resize";
    /// Programmatically set the projects sidebar's width. Gated for the
    /// same reason as `window.resize`: harness-only driver for the
    /// sidebar resize e2e (`tools/roosttest`); a real user drags the
    /// seam. The width is clamped by the workspace, not by the wire.
    pub const SIDEBAR_SET_WIDTH: &str = "sidebar.set_width";
    /// Ungated companion: a richer read of the same render state
    /// `tab.dump` already exposes. Pins the resolver call site
    /// (theme bold-color → `resolve_cell_colors`) end-to-end.
    pub const TAB_DUMP_RESOLVED: &str = "tab.dump_resolved";
    /// Test-only word/line selection driver. Same gate as
    /// `tab.feed_pty_bytes`; runs the production `handle_click_count`
    /// dispatch on both UIs so the e2e suite can pin word/line
    /// expansion without synthetic mouse events.
    pub const TAB_EXPAND_SELECTION_AT: &str = "tab.expand_selection_at";
    /// Test-only synthetic mouse event. Drives the same
    /// `routeMouseEvent` helper a real NSEvent / GestureClick uses,
    /// so the e2e suite can pin button-event and motion encoding
    /// without a window manager. Gated by `ROOST_TEST_MODE=1`.
    pub const TAB_DISPATCH_MOUSE_EVENT: &str = "tab.dispatch_mouse_event";
    /// Test-only focus-state driver. Drives the focus-tracking emit
    /// path so the e2e suite can pin mode 1004 `\x1b[I` / `\x1b[O`
    /// without taking real OS focus from the test runner. Gated by
    /// `ROOST_TEST_MODE=1`.
    pub const APP_SET_WINDOW_FOCUS: &str = "app.set_window_focus";
    /// Ungated read of the active tab's current W3C cursor name —
    /// the latest OSC 22 payload, or `"default"` if none has landed.
    /// Used by the e2e suite to assert OSC 22 actually applied.
    pub const APP_CURSOR_SHAPE: &str = "app.cursor_shape";
    /// Ungated read of whether the active tab's terminal owns the UI's
    /// logical keyboard route. This is independent of native toplevel or
    /// compositor focus and becomes false while an in-app overlay owns input.
    pub const APP_ACTIVE_TERMINAL_FOCUSED: &str = "app.active_terminal_focused";

    /// Test-only read of the macOS Dock tile's badge label. Same gate
    /// as `tab.feed_pty_bytes`; the e2e suite drives a notification and
    /// asserts the badge AppKit actually holds, which no unit test can
    /// observe. macOS iced only — every other UI answers
    /// `not-implemented`.
    pub const APP_DOCK_BADGE: &str = "app.dock_badge";

    /// `app.selected_tab_id` — the active project's on-screen selected
    /// tab id (UI truth), for asserting the core and the displayed tab
    /// agree. Implemented by the Rust UI adapters; read-only, not gated.
    pub const APP_SELECTED_TAB_ID: &str = "app.selected_tab_id";

    /// Test-only read of the macOS iced UI's live native menu bar —
    /// walks `NSApp.mainMenu` (not the keybind table), so the e2e suite
    /// can prove table↔menu agreement. Same gate as `app.dock_badge`;
    /// macOS iced only.
    pub const APP_MENU_DUMP: &str = "app.menu_dump";
    /// Test-only dispatch into the live native menu bar by title path
    /// (e.g. `["File", "New Tab"]`), through the same
    /// `performActionForItemAtIndex:` a real click takes. Same gate and
    /// platform restriction as `app.menu_dump`.
    pub const APP_MENU_ACTIVATE: &str = "app.menu_activate";

    /// Test-only read of the macOS iced UI's Sparkle updater state —
    /// whether the framework loaded, whether the updater started, and
    /// the outcome of the last completed check. Same gate and platform
    /// restriction as `app.menu_dump`.
    pub const APP_UPDATE_STATUS: &str = "app.update_status";
    /// Test-only non-interactive update check (`checkForUpdateInformation`
    /// — no UI, no download). Results are read back through
    /// `app.update_status`. Same gate and platform restriction.
    pub const APP_UPDATE_CHECK: &str = "app.update_check";

    /// Test-only read of the macOS iced UI's `UNUserNotificationCenter`
    /// backend state — whether the delegate installed and the user's
    /// authorization answer. Same gate and platform restriction as
    /// `app.menu_dump`.
    pub const APP_NOTIFICATION_STATUS: &str = "app.notification_status";

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
    pub const EVENT_AGENT_REPORT_CHANGED: &str = "agent_report.changed";
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
            shell_state: ShellState::ForegroundProcess,
            agent_lifecycle: AgentLifecycle::Inactive,
            ownership: None,
        };
        round_trip(&t);
        let json = serde_json::to_string(&t).unwrap();
        assert!(
            json.contains("\"id\":\"12345\""),
            "id must be string: {json}"
        );
    }

    /// The agent axes are additive: a `Tab` encoded by a pre-plan-002
    /// server still decodes, and every axis lands on its default.
    /// Non-negotiable while the two UIs ship in separate commits.
    #[test]
    fn tab_decodes_without_the_agent_axes() {
        let legacy = r#"{
            "id":"5","project_id":"1","title":"zsh","cwd":"/tmp",
            "state":"running","has_notification":false,"is_active":true,
            "user_titled":false,"position":0,"created_at":1,"last_active":2,
            "hook_active":false
        }"#;
        let tab: Tab = serde_json::from_str(legacy).unwrap();
        assert_eq!(tab.shell_state, ShellState::Unknown);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
        assert_eq!(tab.ownership, None);
        assert_eq!(tab.agent_state(), AgentTabState::default());
    }

    /// `roostctl doctor` decides whether a server predates the agent
    /// state model by looking for the raw `shell_state` **and**
    /// `agent_lifecycle` keys on the tab objects `tab.list` returns (plan
    /// 003 §3.5) — nothing else discriminates, since `PROTOCOL_VERSION`
    /// doesn't bump for additive changes. Adding `skip_serializing_if` to
    /// either would silently invert that check: doctor would present this
    /// struct's serde default as something the server said.
    #[test]
    fn tab_always_serializes_the_agent_axis_keys() {
        let tab = Tab {
            id: 1,
            project_id: 1,
            title: String::new(),
            cwd: String::new(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position: 0,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: ShellState::default(),
            agent_lifecycle: AgentLifecycle::default(),
            ownership: None,
        };
        let value = serde_json::to_value(&tab).unwrap();
        for key in ["shell_state", "agent_lifecycle"] {
            assert!(
                value.get(key).is_some(),
                "{key} must serialize even at its default: {value}"
            );
        }
    }

    /// A `failed` tab must still decode on a client that only knows
    /// the four legacy states (plan §3.2 / AC 11). `TabState` is the
    /// closed enum both Swift decoders mirror, so this is the Rust
    /// half of that guard.
    #[test]
    fn failed_lifecycle_projects_onto_the_legacy_state_enum() {
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        enum LegacyTabState {
            None,
            Running,
            NeedsInput,
            Idle,
        }
        let state = crate::agent::effective(&AgentTabState {
            shell: ShellState::AtPrompt,
            lifecycle: AgentLifecycle::Failed,
            ownership: Some(Ownership {
                source: "claude".into(),
                ..Ownership::default()
            }),
        });
        let encoded = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<LegacyTabState>(&encoded).unwrap(),
            LegacyTabState::NeedsInput
        );
    }

    #[test]
    fn agent_report_changed_event_round_trips() {
        let ev = AgentReportChangedEvent {
            tab_id: 3,
            shell_state: ShellState::AtPrompt,
            agent_lifecycle: AgentLifecycle::Waiting,
            ownership: Some(Ownership {
                source: "claude".into(),
                session_id: "abc123".into(),
                last_event_at: 1_700_000_000,
                detail: "permission_prompt".into(),
                metadata: Default::default(),
            }),
            state: TabState::NeedsInput,
            hook_active: true,
        };
        round_trip(&ev);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"tab_id\":\"3\""), "got: {json}");
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

    #[test]
    fn tab_expand_selection_at_params_round_trip() {
        let p = TabExpandSelectionAtParams {
            tab_id: 7,
            col: 2,
            row: 0,
            click_count: 2,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"tab_id\":\"7\""), "got: {json}");
        assert!(json.contains("\"click_count\":2"), "got: {json}");
        round_trip(&p);
    }

    #[test]
    fn tab_expand_selection_at_result_round_trip() {
        let r = TabExpandSelectionAtResult {
            col0: 4,
            col1: 15,
            text: Some("/tmp/foo.txt".to_string()),
        };
        round_trip(&r);
        let r_none = TabExpandSelectionAtResult {
            col0: 0,
            col1: 0,
            text: None,
        };
        round_trip(&r_none);
    }

    #[test]
    fn tab_feed_ime_params_round_trip() {
        let p = TabFeedImeParams {
            tab_id: 9,
            action: "preedit".to_string(),
            text: "こ".to_string(),
            cursor_start: Some(0),
            cursor_end: Some(3),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"tab_id\":\"9\""), "got: {json}");
        assert!(json.contains("\"action\":\"preedit\""), "got: {json}");
        round_trip(&p);
    }

    /// `text` defaults to empty and both cursor fields default to
    /// `None` when omitted, matching the `"clear"` action's shape
    /// (no composed text, no cursor span).
    #[test]
    fn tab_feed_ime_params_defaults() {
        let p: TabFeedImeParams =
            serde_json::from_str(r#"{"tab_id":"9","action":"clear"}"#).unwrap();
        assert_eq!(p.tab_id, 9);
        assert_eq!(p.action, "clear");
        assert_eq!(p.text, "");
        assert_eq!(p.cursor_start, None);
        assert_eq!(p.cursor_end, None);
        round_trip(&p);
    }

    #[test]
    fn tab_feed_ime_params_reject_unknown_field() {
        let bad = r#"{"tab_id":"9","action":"commit","text":"a","extra":"x"}"#;
        assert!(serde_json::from_str::<TabFeedImeParams>(bad).is_err());
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
                    agent: None,
                },
                PaletteItemView {
                    id: "n:7".into(),
                    title: "Build done".into(),
                    subtitle: Some("exit 0".into()),
                    agent: None,
                },
            ],
            selected_in_view: Some(true),
        };
        round_trip(&live);

        // Closed: `frame` + `selected_in_view` omitted (skip_serializing_if),
        // defaults restore them.
        let closed = PaletteStateResult::default();
        let json = serde_json::to_string(&closed).unwrap();
        assert!(!json.contains("frame"), "closed state omits frame: {json}");
        assert!(
            !json.contains("selected_in_view"),
            "closed state omits selected_in_view: {json}"
        );
        assert_eq!(closed.selected_in_view, None);
        round_trip(&closed);
    }

    #[test]
    fn palette_present_round_trips() {
        let present = PalettePresentParams {
            title: "Open shed".into(),
            placeholder: "Pick a service…".into(),
            items: vec![
                PaletteItemView {
                    id: "web".into(),
                    title: "shed: web".into(),
                    subtitle: Some("../shed/web".into()),
                    agent: None,
                },
                PaletteItemView {
                    id: "api".into(),
                    title: "shed: api".into(),
                    subtitle: None,
                    agent: None,
                },
            ],
        };
        round_trip(&present);
        // Strict envelope: unknown top-level fields are rejected.
        assert!(serde_json::from_str::<PalettePresentParams>(r#"{"items":[],"bogus":1}"#).is_err());

        // A picked row carries `selected_id`, `dismissed` false.
        let picked = PalettePresentResult {
            selected_id: Some("api".into()),
            dismissed: false,
        };
        round_trip(&picked);
        // Dismissal omits `selected_id` on the wire.
        let dismissed = PalettePresentResult {
            selected_id: None,
            dismissed: true,
        };
        let json = serde_json::to_string(&dismissed).unwrap();
        assert!(
            !json.contains("selected_id"),
            "dismissal omits selected_id: {json}"
        );
        round_trip(&dismissed);
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
    fn window_metrics_result_round_trip() {
        round_trip(&WindowMetricsResult {
            window_width: 1100.0,
            window_height: 700.0,
            sidebar_width: 220.0,
            sidebar_collapsed: false,
            terminal_top: Some(34.0),
            terminal_font_family: Some("JetBrains Mono".to_string()),
        });
        let native = WindowMetricsResult {
            window_width: 1800.0,
            window_height: 700.0,
            sidebar_width: 0.0,
            sidebar_collapsed: true,
            terminal_top: None,
            terminal_font_family: None,
        };
        let json = serde_json::to_string(&native).unwrap();
        assert!(
            !json.contains("terminal_top"),
            "None changed the old wire shape"
        );
        round_trip(&native);

        let old: WindowMetricsResult = serde_json::from_str(
            r#"{"window_width":1100.0,"window_height":700.0,"sidebar_width":220.0,"sidebar_collapsed":false}"#,
        )
        .unwrap();
        assert_eq!(old.terminal_top, None);
        assert_eq!(old.terminal_font_family, None);
    }

    #[test]
    fn sidebar_dump_result_round_trip() {
        round_trip(&SidebarDumpResult {
            agents_visible: true,
            projects: vec![
                SidebarDumpProject {
                    project_id: 1,
                    agents: vec![SidebarDumpAgentRow {
                        tab_id: 7,
                        name: "slauth-refactor".to_string(),
                        lifecycle: AgentLifecycle::Waiting,
                        status_text: "Waiting for input".to_string(),
                        time_text: "2m".to_string(),
                        is_active: false,
                    }],
                },
                SidebarDumpProject {
                    project_id: 2,
                    agents: Vec::new(),
                },
            ],
        });
    }

    #[test]
    fn sidebar_dump_params_reject_unknown_field() {
        round_trip(&SidebarDumpParams {});
        let bad = r#"{"extra":"x"}"#;
        assert!(serde_json::from_str::<SidebarDumpParams>(bad).is_err());
    }

    /// `reset` defaults to false so `{"op":"app.render_stats"}` with no
    /// params at all is a plain read — the common case. Same shape as
    /// `TabCapturePtyInputParams::drain`.
    #[test]
    fn app_render_stats_params_default_reset_is_false() {
        let p: AppRenderStatsParams = serde_json::from_str("{}").unwrap();
        assert!(!p.reset);
        assert!(!AppRenderStatsParams::default().reset);
        round_trip(&AppRenderStatsParams { reset: true });
        let bad = r#"{"reset":true,"extra":"x"}"#;
        assert!(serde_json::from_str::<AppRenderStatsParams>(bad).is_err());
    }

    /// Every counter is string-wrapped int64: nanosecond accumulators
    /// exceed JS's 2^53 safe range, and a mixed number/string shape
    /// would be the worst of both. Assert the *encoding*, not just the
    /// round-trip, so dropping `string_int64` from a field fails here.
    #[test]
    fn app_render_stats_result_counters_are_string_wrapped() {
        let r = AppRenderStatsResult {
            refresh_calls: 12,
            refresh_nanos: 9_007_199_254_740_993,
            rows_rebuilt: 288,
            cells_walked: 23_040,
            draw_calls: 30,
            draw_nanos: 4_500_000,
            fill_text_calls: 720,
            view_calls: 288,
            view_nanos: 3_100_000,
            elide_calls: 0,
            elide_nanos: 0,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""refresh_calls":"12""#), "got: {json}");
        assert!(
            json.contains(r#""refresh_nanos":"9007199254740993""#),
            "got: {json}"
        );
        assert!(json.contains(r#""rows_rebuilt":"288""#), "got: {json}");
        assert!(json.contains(r#""cells_walked":"23040""#), "got: {json}");
        assert!(json.contains(r#""draw_calls":"30""#), "got: {json}");
        assert!(json.contains(r#""draw_nanos":"4500000""#), "got: {json}");
        assert!(json.contains(r#""fill_text_calls":"720""#), "got: {json}");
        assert!(json.contains(r#""view_calls":"288""#), "got: {json}");
        assert!(json.contains(r#""view_nanos":"3100000""#), "got: {json}");
        assert!(json.contains(r#""elide_calls":"0""#), "got: {json}");
        assert!(json.contains(r#""elide_nanos":"0""#), "got: {json}");
        round_trip(&r);
    }

    /// Result structs stay permissive so a newer UI can add a counter
    /// without breaking older clients — the same contract
    /// `TabDumpResolvedResult` documents.
    #[test]
    fn app_render_stats_result_accepts_extra_fields() {
        let json = r#"{"refresh_calls":"1","refresh_nanos":"2","rows_rebuilt":"3",
                       "cells_walked":"4","draw_calls":"5","draw_nanos":"6",
                       "fill_text_calls":"7","view_calls":"8","view_nanos":"9",
                       "elide_calls":"10","elide_nanos":"11","gpu_nanos":"12"}"#;
        let r: AppRenderStatsResult = serde_json::from_str(json).expect("permissive decode");
        assert_eq!(r.fill_text_calls, 7);
        assert_eq!(r.view_calls, 8);
        assert_eq!(r.view_nanos, 9);
        assert_eq!(r.elide_calls, 10);
        assert_eq!(r.elide_nanos, 11);
    }

    /// The mac Swift handler doesn't send `view_*`/`elide_*` — those
    /// fields must default to 0 rather than fail the decode, the same
    /// tolerance `TabOpenParams.project_id` relies on.
    #[test]
    fn app_render_stats_result_defaults_missing_view_elide_fields() {
        let json = r#"{"refresh_calls":"1","refresh_nanos":"2","rows_rebuilt":"3",
                       "cells_walked":"4","draw_calls":"5","draw_nanos":"6",
                       "fill_text_calls":"7"}"#;
        let r: AppRenderStatsResult = serde_json::from_str(json).expect("permissive decode");
        assert_eq!(r.view_calls, 0);
        assert_eq!(r.view_nanos, 0);
        assert_eq!(r.elide_calls, 0);
        assert_eq!(r.elide_nanos, 0);
    }

    #[test]
    fn window_resize_params_reject_unknown_field() {
        round_trip(&WindowResizeParams {
            width: 1100.0,
            height: 700.0,
        });
        let bad = r#"{"width":1100.0,"height":700.0,"extra":"x"}"#;
        assert!(serde_json::from_str::<WindowResizeParams>(bad).is_err());
    }

    #[test]
    fn sidebar_set_width_params_reject_unknown_field() {
        round_trip(&SidebarSetWidthParams { width: 260.0 });
        let bad = r#"{"width":260.0,"extra":"x"}"#;
        assert!(serde_json::from_str::<SidebarSetWidthParams>(bad).is_err());
    }

    #[test]
    fn tab_dispatch_mouse_event_params_round_trip() {
        let p = TabDispatchMouseEventParams {
            tab_id: 11,
            kind: "press".into(),
            button: "left".into(),
            cell_x: 6,
            cell_y: 3,
            mods: 0,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"tab_id\":\"11\""), "got: {json}");
        assert!(json.contains("\"kind\":\"press\""), "got: {json}");
        round_trip(&p);

        // `mods` defaults to 0 when omitted (most tests don't carry mods).
        let no_mods: TabDispatchMouseEventParams = serde_json::from_str(
            r#"{"tab_id":"3","kind":"motion","button":"none","cell_x":1,"cell_y":1}"#,
        )
        .unwrap();
        assert_eq!(no_mods.mods, 0);

        let bad =
            r#"{"tab_id":"3","kind":"press","button":"left","cell_x":1,"cell_y":1,"extra":"x"}"#;
        assert!(serde_json::from_str::<TabDispatchMouseEventParams>(bad).is_err());
    }

    #[test]
    fn app_set_window_focus_params_round_trip() {
        round_trip(&AppSetWindowFocusParams { focus: true });
        round_trip(&AppSetWindowFocusParams { focus: false });
        let bad = r#"{"focus":true,"extra":"x"}"#;
        assert!(serde_json::from_str::<AppSetWindowFocusParams>(bad).is_err());
    }

    #[test]
    fn app_cursor_shape_round_trips_empty_params_and_result() {
        round_trip(&AppCursorShapeParams {});
        round_trip(&AppCursorShapeResult {
            shape: "pointer".into(),
        });
        round_trip(&AppCursorShapeResult {
            shape: "default".into(),
        });
        let bad = r#"{"extra":"x"}"#;
        assert!(serde_json::from_str::<AppCursorShapeParams>(bad).is_err());
    }

    #[test]
    fn app_active_terminal_focused_round_trips() {
        round_trip(&AppActiveTerminalFocusedParams {});
        round_trip(&AppActiveTerminalFocusedResult { focused: true });
        round_trip(&AppActiveTerminalFocusedResult { focused: false });
        let bad = r#"{"extra":"x"}"#;
        assert!(serde_json::from_str::<AppActiveTerminalFocusedParams>(bad).is_err());
    }

    #[test]
    fn app_dock_badge_round_trips() {
        round_trip(&AppDockBadgeParams {});
        round_trip(&AppDockBadgeResult {
            label: Some("3".into()),
        });
        round_trip(&AppDockBadgeResult { label: None });
        // The cleared badge is `null`, not an omitted or empty field —
        // the e2e distinguishes "no badge" from "a badge reading ''".
        let cleared = serde_json::to_string(&AppDockBadgeResult { label: None }).unwrap();
        assert_eq!(cleared, r#"{"label":null}"#);
        let bad = r#"{"extra":"x"}"#;
        assert!(serde_json::from_str::<AppDockBadgeParams>(bad).is_err());
    }

    #[test]
    fn app_menu_dump_round_trips() {
        round_trip(&AppMenuDumpParams {});
        round_trip(&AppMenuDumpResult {
            menus: vec![MenuDump {
                title: "File".into(),
                items: vec![
                    MenuItemDump {
                        title: "New Tab".into(),
                        key_equivalent: "t".into(),
                        modifiers: vec!["super".into()],
                        enabled: true,
                        state: "off".into(),
                        separator: false,
                        action: Some("new_tab".into()),
                    },
                    MenuItemDump {
                        title: String::new(),
                        key_equivalent: String::new(),
                        modifiers: vec![],
                        enabled: true,
                        state: "off".into(),
                        separator: true,
                        action: None,
                    },
                ],
            }],
        });
        round_trip(&AppMenuDumpResult::default());
        let bad = r#"{"extra":"x"}"#;
        assert!(serde_json::from_str::<AppMenuDumpParams>(bad).is_err());
    }

    #[test]
    fn app_menu_activate_round_trips() {
        round_trip(&AppMenuActivateParams {
            path: vec!["File".into(), "New Tab".into()],
        });
        round_trip(&AppMenuActivateParams { path: vec![] });
        let bad = r#"{"path":["File"],"extra":"x"}"#;
        assert!(serde_json::from_str::<AppMenuActivateParams>(bad).is_err());
    }

    #[test]
    fn app_update_status_round_trips() {
        round_trip(&AppUpdateStatusParams {});
        round_trip(&AppUpdateCheckParams {});
        round_trip(&AppUpdateStatusResult {
            framework_loaded: true,
            updater: "started".into(),
            reason: None,
            check_id: 3,
            last_check: Some(UpdateCheckDump {
                outcome: "found".into(),
                version: Some("99.0.0".into()),
                detail: None,
            }),
        });
        round_trip(&AppUpdateStatusResult {
            framework_loaded: false,
            updater: "unavailable".into(),
            reason: Some("dlopen(...) failed".into()),
            check_id: 0,
            last_check: None,
        });
        round_trip(&AppUpdateStatusResult::default());

        // The bare-binary shape the e2e asserts on: `null` is how a
        // never-checked updater differs from one whose check reported
        // nothing, so the distinction has to survive the wire.
        let never_checked = serde_json::to_value(&AppUpdateStatusResult {
            framework_loaded: false,
            updater: "unavailable".into(),
            reason: Some("no framework".into()),
            check_id: 0,
            last_check: None,
        })
        .unwrap();
        assert_eq!(never_checked["last_check"], serde_json::Value::Null);

        for bad in [r#"{"extra":"x"}"#, r#"{"foo":1}"#] {
            assert!(serde_json::from_str::<AppUpdateStatusParams>(bad).is_err());
            assert!(serde_json::from_str::<AppUpdateCheckParams>(bad).is_err());
        }
    }

    #[test]
    fn app_notification_status_round_trips() {
        round_trip(&AppNotificationStatusParams {});
        // Deliberately never a fixture with `authorized: true` — CI's
        // TCC authorization state is unknowable, and this wire-format
        // test is the wrong place to normalize asserting it.
        let available = AppNotificationStatusResult {
            backend: "available".into(),
            reason: None,
            authorized: false,
        };
        round_trip(&available);
        // A `reason`-less status serializes the field as null, not an
        // omitted field — the wire shape ipc.md documents.
        assert_eq!(
            serde_json::to_value(&available).unwrap()["reason"],
            serde_json::Value::Null
        );
        round_trip(&AppNotificationStatusResult {
            backend: "unavailable".into(),
            reason: Some("not running from an app bundle".into()),
            authorized: false,
        });
        round_trip(&AppNotificationStatusResult::default());

        let bad = r#"{"extra":"x"}"#;
        assert!(serde_json::from_str::<AppNotificationStatusParams>(bad).is_err());
    }

    #[test]
    fn app_selected_tab_id_round_trips() {
        round_trip(&AppSelectedTabIdParams {});
        round_trip(&AppSelectedTabIdResult { tab_id: 42 });
        round_trip(&AppSelectedTabIdResult { tab_id: 0 });
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
