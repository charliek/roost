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
