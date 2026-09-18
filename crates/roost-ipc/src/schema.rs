//! The wire's JSON Schema bundle (plan 067 §3.6): every op's params and
//! result, every event's data, and the envelopes around them, generated
//! from the serde types in [`crate::messages`] and [`crate::agent`] so
//! the schema cannot describe a shape those types do not accept.
//!
//! Which socket serves an op is not encoded here; that is
//! `identify.ops`' job.

use schemars::generate::SchemaSettings;
use schemars::transform::RecursiveTransform;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{json, Map, Value};

use crate::agent::TabAgentReportParams;
use crate::messages::*;

/// The bundle's own shape version, independent of both protocol
/// integers it reports beside it.
const SCHEMA_VERSION: u32 = 1;

/// The whole bundle: the draft 2020-12 `$schema`, the version integers,
/// the six `envelopes`, `ops` (`{name: {params, result}}`), `events`
/// (`{name: data}`), and `$defs`, which every named type is a `$ref`
/// into. Keys are sorted at every level.
pub fn bundle() -> Value {
    let mut generator = SchemaSettings::draft2020_12()
        .with_transform(RecursiveTransform(without_description))
        .into_generator();
    let meta_schema = generator.settings().meta_schema.clone();

    let mut ops = Map::new();
    let mut events = Map::new();
    for row in OP_TYPES {
        match row.shape {
            Shape::Op { params, result } => {
                let entry = json!({
                    "params": params(&mut generator).to_value(),
                    "result": result(&mut generator).to_value(),
                });
                ops.insert(row.name.to_string(), entry);
            }
            Shape::Event { data } => {
                events.insert(row.name.to_string(), data(&mut generator).to_value());
            }
        }
    }

    let response = generator.subschema_for::<Response>().to_value();
    let event = generator.subschema_for::<EventEnvelope>().to_value();
    let envelopes = json!({
        "request": generator.subschema_for::<RawRequest>().to_value(),
        "response": {
            "allOf": [response],
            "properties": {"ok": {"const": true}},
            "required": ["result"],
        },
        "error": {
            "allOf": [response],
            "properties": {"ok": {"const": false}},
            "required": ["error"],
        },
        "event_batch": generator.subschema_for::<EventBatch>().to_value(),
        "session_stopping": terminal_frame::<SessionStoppingEvent>(
            &mut generator,
            &event,
            SESSION_STOPPING_EVENT,
        ),
        "stream_ended": terminal_frame::<StreamEndedEvent>(
            &mut generator,
            &event,
            STREAM_ENDED_EVENT,
        ),
    });

    // The first line of an attach data connection is neither an op nor
    // an event, so it has no slot above; it is reachable by type name.
    generator.subschema_for::<AttachHandshake>();
    generator.subschema_for::<AttachHandshakeReply>();

    let mut result = json!({
        "$schema": meta_schema,
        "$defs": generator.take_definitions(true),
        "schema_version": SCHEMA_VERSION,
        "protocol_version": crate::PROTOCOL_VERSION,
        "session_protocol_version": SESSION_PROTOCOL_VERSION,
        "envelopes": envelopes,
        "ops": ops,
        "events": events,
    });
    mark_always_present(&mut result);
    sorted(result)
}

/// `(json pointer to the object schema, field name)`: fields whose
/// `Option<T>` is optional in Rust's type system but not on the wire —
/// no `#[serde(default)]` and no `skip_serializing_if`, so `Serialize`
/// always writes the key (`null` included) and `Deserialize` has no
/// fallback for its absence (plan 067 review, finding 2).
///
/// `derive(JsonSchema)`'s own `#[schemars(required)]` cannot state this:
/// it routes through `_schemars_private_non_optional_json_schema`, which
/// also strips the type's nullability, and every field here is
/// legitimately `null` on the wire some of the time (that is the whole
/// reason each is `Option<T>` rather than `T`). So `bundle()` patches
/// `required` onto the already-generated schema instead, leaving the
/// type alone.
///
/// Kept honest by `required_matches_serde_for_every_option_field`: a
/// field belongs here iff that scan's `should_be_required` says so, and
/// mutating one out (or leaving a new one off) fails either that test or
/// `every_vector_validates_against_the_bundle` on the recorded vector
/// the field's own value differs in.
const ALWAYS_PRESENT: &[(&str, &str)] = &[
    ("/$defs/AppDockBadgeResult", "label"),
    ("/$defs/MenuItemDump", "action"),
    ("/$defs/AppUpdateStatusResult", "reason"),
    ("/$defs/AppUpdateStatusResult", "last_check"),
    ("/$defs/UpdateCheckDump", "version"),
    ("/$defs/UpdateCheckDump", "detail"),
    ("/$defs/AppNotificationStatusResult", "reason"),
    ("/$defs/TabExpandSelectionAtResult", "text"),
    ("/$defs/DurabilityChangedEvent", "error"),
];

fn mark_always_present(bundle: &mut Value) {
    for (pointer, field) in ALWAYS_PRESENT {
        let target = bundle
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("{pointer} does not exist in the bundle"));
        let object = target
            .as_object_mut()
            .unwrap_or_else(|| panic!("{pointer} is not an object schema"));
        let required = object
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        let required = required
            .as_array_mut()
            .unwrap_or_else(|| panic!("{pointer}/required is not an array"));
        if !required.iter().any(|v| v == field) {
            required.push(Value::String((*field).to_string()));
        }
    }
}

/// [`bundle`] as the checked-in file spells it: pretty, sorted, and
/// ending in a newline.
pub fn bundle_json() -> String {
    let mut text = serde_json::to_string_pretty(&bundle()).expect("a Value always serializes");
    text.push('\n');
    text
}

fn terminal_frame<D: JsonSchema>(
    generator: &mut SchemaGenerator,
    event: &Value,
    name: &str,
) -> Value {
    json!({
        "allOf": [event],
        "properties": {
            "event": {"const": name},
            "data": generator.subschema_for::<D>().to_value(),
        },
    })
}

/// Doc comments stay out of the bundle: it has to move only when a wire
/// shape does, and their rustdoc links and plan references are prose
/// `ipc.md` carries.
fn without_description(schema: &mut Schema) {
    schema.remove("description");
}

/// Re-inserted in key order rather than trusted to `serde_json::Map`:
/// any crate in a build enabling `serde_json/preserve_order` turns every
/// `Map` into insertion order, and the bundle has to read the same from
/// every graph that prints it.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// The result of every op that answers `{}`.
#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
struct EmptyResult {}

/// `tab.list`'s params. No dispatcher decodes them, so no object is
/// refused.
#[derive(JsonSchema)]
struct TabListParams {}

type SchemaFn = fn(&mut SchemaGenerator) -> Schema;

enum Shape {
    Op { params: SchemaFn, result: SchemaFn },
    Event { data: SchemaFn },
}

struct Row {
    name: &'static str,
    shape: Shape,
}

const fn op<P: JsonSchema, R: JsonSchema>(name: &'static str) -> Row {
    Row {
        name,
        shape: Shape::Op {
            params: SchemaGenerator::subschema_for::<P>,
            result: SchemaGenerator::subschema_for::<R>,
        },
    }
}

const fn event<D: JsonSchema>(name: &'static str) -> Row {
    Row {
        name,
        shape: Shape::Event {
            data: SchemaGenerator::subschema_for::<D>,
        },
    }
}

/// One row per `ops::` constant: an op's params and result types, or an
/// event's data type.
const OP_TYPES: &[Row] = &[
    op::<IdentifyParams, IdentifyResult>(ops::IDENTIFY),
    op::<TabOpenParams, TabOpenResult>(ops::TAB_OPEN),
    op::<TabCloseParams, EmptyResult>(ops::TAB_CLOSE),
    op::<TabListParams, TabListResult>(ops::TAB_LIST),
    op::<TabWriteParams, EmptyResult>(ops::TAB_WRITE),
    op::<TabResizeParams, EmptyResult>(ops::TAB_RESIZE),
    op::<TabDumpParams, TabDumpResult>(ops::TAB_DUMP),
    op::<ProjectCreateParams, ProjectCreateResult>(ops::PROJECT_CREATE),
    op::<ProjectEnsureParams, ProjectEnsureResult>(ops::PROJECT_ENSURE),
    op::<ProjectRenameParams, EmptyResult>(ops::PROJECT_RENAME),
    op::<ProjectDeleteParams, EmptyResult>(ops::PROJECT_DELETE),
    op::<TabReorderParams, EmptyResult>(ops::TAB_REORDER),
    op::<ProjectReorderParams, EmptyResult>(ops::PROJECT_REORDER),
    op::<TabFocusParams, TabFocusResult>(ops::TAB_FOCUS),
    op::<TabSetTitleParams, EmptyResult>(ops::TAB_SET_TITLE),
    op::<TabSetStateParams, EmptyResult>(ops::TAB_SET_STATE),
    op::<TabClearNotificationParams, TabClearNotificationResult>(ops::TAB_CLEAR_NOTIFICATION),
    op::<TabSetHookActiveParams, EmptyResult>(ops::TAB_SET_HOOK_ACTIVE),
    op::<TabAgentReportParams, TabAgentReportResult>(ops::TAB_AGENT_REPORT),
    op::<TabSendFileParams, TabSendFileResult>(ops::TAB_SEND_FILE),
    op::<NotificationCreateParams, EmptyResult>(ops::NOTIFICATION_CREATE),
    op::<EventsSubscribeParams, EventsSubscribeResult>(ops::EVENTS_SUBSCRIBE),
    op::<SessionIdentifyParams, SessionIdentify>(ops::SESSION_IDENTIFY),
    op::<SessionStopParams, SessionStopResult>(ops::SESSION_STOP),
    op::<SessionSetThemeParams, SessionSetThemeResult>(ops::SESSION_SET_THEME),
    op::<SessionSetAgentHooksParams, AgentHooksOutcome>(ops::SESSION_SET_AGENT_HOOKS),
    op::<SessionPutFileParams, SessionPutFileResult>(ops::SESSION_PUT_FILE),
    op::<AppActivateParams, EmptyResult>(ops::APP_ACTIVATE),
    op::<ScreenshotParams, ScreenshotResult>(ops::SCREENSHOT),
    op::<WindowMetricsParams, WindowMetricsResult>(ops::WINDOW_METRICS),
    op::<SidebarDumpParams, SidebarDumpResult>(ops::SIDEBAR_DUMP),
    op::<AppRenderStatsParams, AppRenderStatsResult>(ops::APP_RENDER_STATS),
    op::<PaletteOpenParams, PaletteStateResult>(ops::PALETTE_OPEN),
    op::<PaletteStateParams, PaletteStateResult>(ops::PALETTE_STATE),
    op::<PaletteQueryParams, PaletteStateResult>(ops::PALETTE_QUERY),
    op::<PaletteActivateParams, PaletteStateResult>(ops::PALETTE_ACTIVATE),
    op::<PaletteDismissParams, PaletteStateResult>(ops::PALETTE_DISMISS),
    op::<PalettePresentParams, PalettePresentResult>(ops::PALETTE_PRESENT),
    op::<SelectionSetParams, EmptyResult>(ops::SELECTION_SET),
    op::<SelectionClearParams, EmptyResult>(ops::SELECTION_CLEAR),
    op::<SelectionDumpParams, SelectionDumpResult>(ops::SELECTION_DUMP),
    op::<ClipboardDumpParams, ClipboardDumpResult>(ops::CLIPBOARD_DUMP),
    op::<ClipboardWriteParams, EmptyResult>(ops::CLIPBOARD_WRITE),
    op::<TabFeedPtyBytesParams, EmptyResult>(ops::TAB_FEED_PTY_BYTES),
    op::<TabCapturePtyInputParams, TabCapturePtyInputResult>(ops::TAB_CAPTURE_PTY_INPUT),
    op::<TabFeedImeParams, EmptyResult>(ops::TAB_FEED_IME),
    op::<WindowResizeParams, EmptyResult>(ops::WINDOW_RESIZE),
    op::<SidebarSetWidthParams, EmptyResult>(ops::SIDEBAR_SET_WIDTH),
    op::<TabDumpResolvedParams, TabDumpResolvedResult>(ops::TAB_DUMP_RESOLVED),
    op::<TabExpandSelectionAtParams, TabExpandSelectionAtResult>(ops::TAB_EXPAND_SELECTION_AT),
    op::<TabDispatchMouseEventParams, EmptyResult>(ops::TAB_DISPATCH_MOUSE_EVENT),
    op::<AppSetWindowFocusParams, EmptyResult>(ops::APP_SET_WINDOW_FOCUS),
    op::<AppCursorShapeParams, AppCursorShapeResult>(ops::APP_CURSOR_SHAPE),
    op::<AppActiveTerminalFocusedParams, AppActiveTerminalFocusedResult>(
        ops::APP_ACTIVE_TERMINAL_FOCUSED,
    ),
    op::<AppDockBadgeParams, AppDockBadgeResult>(ops::APP_DOCK_BADGE),
    op::<AppSelectedTabIdParams, AppSelectedTabIdResult>(ops::APP_SELECTED_TAB_ID),
    op::<AppMenuDumpParams, AppMenuDumpResult>(ops::APP_MENU_DUMP),
    op::<AppMenuActivateParams, EmptyResult>(ops::APP_MENU_ACTIVATE),
    op::<AppUpdateStatusParams, AppUpdateStatusResult>(ops::APP_UPDATE_STATUS),
    op::<AppUpdateCheckParams, EmptyResult>(ops::APP_UPDATE_CHECK),
    op::<AppNotificationStatusParams, AppNotificationStatusResult>(ops::APP_NOTIFICATION_STATUS),
    op::<AppDialogDumpParams, AppDialogDumpResult>(ops::APP_DIALOG_DUMP),
    op::<AppDialogAnswerParams, EmptyResult>(ops::APP_DIALOG_ANSWER),
    op::<AppKeybindDispatchParams, EmptyResult>(ops::APP_KEYBIND_DISPATCH),
    op::<AgentSetHooksParams, AgentSetHooksResult>(ops::AGENT_SET_HOOKS),
    op::<HostAddParams, HostAddResult>(ops::HOST_ADD),
    op::<HostRemoveParams, EmptyResult>(ops::HOST_REMOVE),
    op::<HostListParams, HostListResult>(ops::HOST_LIST),
    op::<HostConnectParams, HostConnectionResult>(ops::HOST_CONNECT),
    op::<HostDisconnectParams, HostConnectionResult>(ops::HOST_DISCONNECT),
    op::<HostStatusParams, HostStatusResult>(ops::HOST_STATUS),
    event::<TabOpenedEvent>(ops::EVENT_TAB_OPENED),
    event::<TabClosedEvent>(ops::EVENT_TAB_CLOSED),
    event::<TabStateChangedEvent>(ops::EVENT_TAB_STATE_CHANGED),
    event::<TabTitleChangedEvent>(ops::EVENT_TAB_TITLE_CHANGED),
    event::<TabCwdChangedEvent>(ops::EVENT_TAB_CWD_CHANGED),
    event::<TabNotificationEvent>(ops::EVENT_TAB_NOTIFICATION),
    event::<ProjectCreatedEvent>(ops::EVENT_PROJECT_CREATED),
    event::<ProjectRenamedEvent>(ops::EVENT_PROJECT_RENAMED),
    event::<ProjectDeletedEvent>(ops::EVENT_PROJECT_DELETED),
    event::<ActiveChangedEvent>(ops::EVENT_ACTIVE_CHANGED),
    event::<HookActiveChangedEvent>(ops::EVENT_HOOK_ACTIVE_CHANGED),
    event::<NotificationFiredEvent>(ops::EVENT_NOTIFICATION_FIRED),
    event::<AgentReportChangedEvent>(ops::EVENT_AGENT_REPORT_CHANGED),
    event::<TabsReorderedEvent>(ops::EVENT_TABS_REORDERED),
    event::<ProjectsReorderedEvent>(ops::EVENT_PROJECTS_REORDERED),
    event::<TabEffectEvent>(ops::EVENT_TAB_EFFECT),
    event::<DurabilityChangedEvent>(ops::EVENT_WORKSPACE_DURABILITY_CHANGED),
];

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

    use super::*;

    const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

    fn collect_refs(value: &Value) -> Vec<&str> {
        match value {
            Value::Object(map) => {
                let mut refs: Vec<_> = map.values().flat_map(collect_refs).collect();
                if let Some(Value::String(target)) = map.get("$ref") {
                    refs.push(target);
                }
                refs
            }
            Value::Array(items) => items.iter().flat_map(collect_refs).collect(),
            _ => Vec::new(),
        }
    }

    /// Every `description` keyword: a string under that key. A wire
    /// field named `description` would map it to a schema object instead.
    fn descriptions(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                usize::from(matches!(map.get("description"), Some(Value::String(_))))
                    + map.values().map(descriptions).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(descriptions).sum(),
            _ => 0,
        }
    }

    #[test]
    fn the_bundle_is_deterministic_and_draft_2020_12() {
        let first = bundle();
        assert_eq!(first, bundle(), "two bundles differ");
        assert_eq!(bundle_json(), bundle_json());
        assert!(bundle_json().ends_with("}\n"));
        assert_eq!(first["$schema"], DRAFT_2020_12);
        assert_eq!(
            descriptions(&first),
            0,
            "doc comments leaked into the bundle"
        );

        let top: Vec<_> = first.as_object().expect("an object").keys().collect();
        assert_eq!(
            top,
            [
                "$defs",
                "$schema",
                "envelopes",
                "events",
                "ops",
                "protocol_version",
                "schema_version",
                "session_protocol_version",
            ]
        );
        let envelopes: Vec<_> = first["envelopes"]
            .as_object()
            .expect("envelopes")
            .keys()
            .collect();
        assert_eq!(
            envelopes,
            [
                "error",
                "event_batch",
                "request",
                "response",
                "session_stopping",
                "stream_ended"
            ]
        );
        assert_eq!(first["schema_version"], 1);
        assert_eq!(first["protocol_version"], crate::PROTOCOL_VERSION);
        assert_eq!(first["session_protocol_version"], SESSION_PROTOCOL_VERSION);

        let defs = first["$defs"].as_object().expect("$defs");
        let refs = collect_refs(&first);
        assert!(refs.len() > 100, "only {} $refs in the bundle", refs.len());
        for target in refs {
            let name = target
                .strip_prefix("#/$defs/")
                .unwrap_or_else(|| panic!("{target} does not point into $defs"));
            assert!(defs.contains_key(name), "{target} dangles");
        }
    }

    /// `pub const NAME: &str = "value";` out of `messages.rs`'s `ops`
    /// module — parsed from the source, so a constant added this morning
    /// is seen whether or not anyone gave it a row.
    fn constants_declared_in_the_source() -> Vec<(String, String)> {
        let source = include_str!("messages.rs");
        let start = source
            .find("\npub mod ops {\n")
            .expect("messages.rs declares `pub mod ops`");
        let body = &source[start..];
        let end = body.find("\n}\n").expect("the ops module closes");
        let constants: Vec<_> = body[..end]
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("pub const ")?;
                let (name, rest) = rest.split_once(": &str = ")?;
                let value = rest.strip_prefix('"')?.strip_suffix("\";")?;
                Some((name.to_string(), value.to_string()))
            })
            .collect();
        assert!(
            constants.len() > 80,
            "only {} constants parsed out of the ops module - the parser has drifted",
            constants.len()
        );
        constants
    }

    fn is_event(row: &Row) -> bool {
        matches!(row.shape, Shape::Event { .. })
    }

    #[test]
    fn every_op_and_event_constant_has_exactly_one_row() {
        let declared = constants_declared_in_the_source();
        let missing: Vec<_> = declared
            .iter()
            .filter(|(name, value)| {
                !OP_TYPES
                    .iter()
                    .any(|row| row.name == value && is_event(row) == name.starts_with("EVENT_"))
            })
            .map(|(name, value)| format!("ops::{name} ({value:?})"))
            .collect();
        assert!(
            missing.is_empty(),
            "OP_TYPES has no row for: {}",
            missing.join(", ")
        );

        let orphans: Vec<_> = OP_TYPES
            .iter()
            .filter(|row| {
                !declared.iter().any(|(name, value)| {
                    value == row.name && name.starts_with("EVENT_") == is_event(row)
                })
            })
            .map(|row| row.name)
            .collect();
        assert!(
            orphans.is_empty(),
            "rows no `ops` constant names: {orphans:?}"
        );
        assert_eq!(OP_TYPES.len(), declared.len(), "one row per constant");
    }

    /// The deny-unknown-fields structs of `messages.rs` and `agent.rs`,
    /// outside their test modules.
    fn strict_structs(source: &str) -> Vec<String> {
        let source = source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("split yields at least one piece");
        let mut names = Vec::new();
        let mut armed = false;
        for line in source.lines().map(str::trim) {
            if line == "#[serde(deny_unknown_fields)]" {
                armed = true;
            } else if armed {
                if let Some(rest) = line
                    .strip_prefix("pub struct ")
                    .or_else(|| line.strip_prefix("struct "))
                {
                    let name = rest
                        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .next()
                        .expect("a struct has a name");
                    names.push(name.to_string());
                    armed = false;
                } else if !line.starts_with("#[") {
                    armed = false;
                }
            }
        }
        names
    }

    #[test]
    fn every_deny_unknown_fields_struct_is_closed_in_the_bundle() {
        let bundle = bundle();
        let defs = bundle["$defs"].as_object().expect("$defs");
        let strict: Vec<_> = strict_structs(include_str!("messages.rs"))
            .into_iter()
            .chain(strict_structs(include_str!("agent.rs")))
            .collect();
        assert!(
            strict.len() > 60,
            "only {} strict structs parsed",
            strict.len()
        );
        for name in strict {
            let def = defs
                .get(&name)
                .unwrap_or_else(|| panic!("{name} refuses unknown fields but is not in $defs"));
            assert_eq!(def["additionalProperties"], false, "{name}");
        }
    }

    /// One `Option<...>` field a `#[derive(JsonSchema)]` struct declares,
    /// with whatever attributes ride directly above it.
    struct OptionField {
        struct_name: String,
        field: String,
        has_default: bool,
        has_skip: bool,
    }

    /// The attribute lines (doc comments dropped) immediately above
    /// `lines[i]`, each multi-line `#[...]` joined into one string by
    /// bracket-depth, and the index of the first line that is neither.
    fn collect_attrs(lines: &[&str], mut i: usize) -> (Vec<String>, usize) {
        let mut attrs = Vec::new();
        while i < lines.len() {
            let t = lines[i].trim();
            if t.is_empty() || t.starts_with("///") || t.starts_with("//!") {
                i += 1;
            } else if t.starts_with("#[") {
                let mut depth = t.matches('[').count() as i32 - t.matches(']').count() as i32;
                let mut text = t.to_string();
                while depth > 0 {
                    i += 1;
                    let next = lines.get(i).copied().unwrap_or_default().trim();
                    depth += next.matches('[').count() as i32 - next.matches(']').count() as i32;
                    text.push(' ');
                    text.push_str(next);
                }
                attrs.push(text);
                i += 1;
            } else {
                break;
            }
        }
        (attrs, i)
    }

    /// `name: Option<...>,` (a plain struct field, not a function
    /// parameter — callers only feed this lines known to sit inside a
    /// struct's `{ … }`), trailing comments and all.
    fn parse_option_field(line: &str) -> Option<String> {
        let line = line.split("//").next().unwrap_or(line).trim();
        let rest = line.strip_prefix("pub ").unwrap_or(line);
        let (name, ty) = rest.split_once(':')?;
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        let ty = ty.trim().strip_suffix(',')?.trim();
        (ty.starts_with("Option<") && ty.ends_with('>')).then(|| name.to_string())
    }

    /// Every `Option<...>` field of every `#[derive(..., JsonSchema,
    /// ...)]` struct in a source file's production code (the same cut
    /// point [`strict_structs`] uses), scoped strictly to lines inside
    /// that struct's brace range so a same-shaped function parameter
    /// elsewhere in the file (`TabAgentReportParams::sessionless`'s
    /// `lifecycle: Option<AgentLifecycle>` argument, for one) is never
    /// mistaken for a field.
    ///
    /// [`AttachHandshake`] and [`AttachHandshakeReply`] hand-write their
    /// `JsonSchema` impl instead of deriving it (finding 1), so their own
    /// field lists never match the `derive(JsonSchema)` gate here and are
    /// skipped — correctly: their `TryFrom` requiredness is asserted
    /// directly by `attach_handshake_requires_what_try_from_requires` and
    /// `attach_handshake_reply_arms_require_their_own_fields` above, not
    /// by this generic pass over their flat `Raw*` mirrors' own
    /// `#[serde(default, skip_serializing_if = …)]` fields.
    fn option_fields_declared_in_the_source(source: &str) -> Vec<OptionField> {
        let source = source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("split yields at least one piece");
        let lines: Vec<&str> = source.lines().collect();

        let mut fields = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let (attrs, next) = collect_attrs(&lines, i);
            i = next;
            let Some(&line) = lines.get(i) else { break };
            let trimmed = line.trim();
            let struct_name = trimmed
                .strip_prefix("pub struct ")
                .or_else(|| trimmed.strip_prefix("struct "))
                .and_then(|rest| {
                    rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .next()
                });
            let derives_json_schema = attrs
                .iter()
                .any(|a| a.contains("derive") && a.contains("JsonSchema"));
            match struct_name {
                Some(name) if derives_json_schema && trimmed.ends_with('{') => {
                    let struct_name = name.to_string();
                    i += 1;
                    while i < lines.len() && lines[i].trim() != "}" {
                        let (field_attrs, next) = collect_attrs(&lines, i);
                        i = next;
                        if lines.get(i).map(|l| l.trim()) == Some("}") {
                            break;
                        }
                        let Some(&field_line) = lines.get(i) else {
                            break;
                        };
                        if let Some(field) = parse_option_field(field_line) {
                            let is_serde = |a: &&String| a.contains("serde");
                            fields.push(OptionField {
                                struct_name: struct_name.clone(),
                                field,
                                has_default: field_attrs
                                    .iter()
                                    .any(|a| is_serde(&a) && a.contains("default")),
                                has_skip: field_attrs
                                    .iter()
                                    .any(|a| a.contains("skip_serializing_if")),
                            });
                        }
                        i += 1;
                    }
                    i += 1; // the closing `}`
                }
                _ => i += 1,
            }
        }
        fields
    }

    /// `Struct.field` pairs the scan parses out of the source but that
    /// `bundle()` has nowhere to check against: no `$defs` entry named
    /// after the struct. Two reasons, both benign — every field listed
    /// here already has `#[serde(default)]` and/or `skip_serializing_if`,
    /// so `should_be_required` is `false` for all of them and there is
    /// no finding-2 defect being hidden by the exemption:
    ///
    /// - `RawAttachHandshake`/`RawAttachHandshakeReply`: [`AttachHandshake`]
    ///   and [`AttachHandshakeReply`] hand-write `JsonSchema` by calling
    ///   `RawAttachHandshake::json_schema(generator)` directly rather
    ///   than `generator.subschema_for::<RawAttachHandshake>()`, so the
    ///   `Raw*` type itself is never registered in `$defs` — its schema
    ///   is inlined wherever the outer type is. Their own requiredness
    ///   is asserted directly by `attach_handshake_requires_what_try_
    ///   from_requires` and `attach_handshake_reply_arms_require_their_
    ///   own_fields` above.
    /// - `AgentTabState`: derives `JsonSchema` but nothing in `OP_TYPES`
    ///   ever reaches it — `Tab::agent_state()` computes one for
    ///   server-internal use; it never rides the wire as a field of any
    ///   op, event, or nested type, so `bundle()` never calls
    ///   `subschema_for::<AgentTabState>()` and it has no `$defs` entry.
    const NOT_IN_DEFS: &[(&str, &str)] = &[
        ("RawAttachHandshake", "session_id"),
        ("RawAttachHandshake", "kinds"),
        ("RawAttachHandshake", "cols"),
        ("RawAttachHandshake", "rows"),
        ("RawAttachHandshake", "cell_w_px"),
        ("RawAttachHandshake", "cell_h_px"),
        ("RawAttachHandshake", "libghostty_build"),
        ("RawAttachHandshake", "focus"),
        ("RawAttachHandshake", "resume_from_seq"),
        ("RawAttachHandshake", "server_epoch"),
        ("RawAttachHandshake", "tab_generation"),
        ("RawAttachHandshakeReply", "kind"),
        ("RawAttachHandshakeReply", "mode"),
        ("RawAttachHandshakeReply", "seq"),
        ("RawAttachHandshakeReply", "server_epoch"),
        ("RawAttachHandshakeReply", "tab_generation"),
        ("RawAttachHandshakeReply", "snapshot_cols"),
        ("RawAttachHandshakeReply", "snapshot_rows"),
        ("RawAttachHandshakeReply", "error"),
        ("AgentTabState", "ownership"),
    ];

    /// Finding 2: an `Option<T>` field with neither `#[serde(default)]`
    /// nor `skip_serializing_if` is always written by `Serialize` and
    /// has no fallback for `Deserialize`, so it is required on the wire;
    /// the bundle must say so. Conversely a field with either attribute
    /// must stay out of `required` — schemars already leaves it out by
    /// default, so this direction mostly guards against a stray
    /// `#[schemars(required)]` added to a field that is genuinely
    /// optional.
    #[test]
    fn required_matches_serde_for_every_option_field() {
        let fields: Vec<_> = option_fields_declared_in_the_source(include_str!("messages.rs"))
            .into_iter()
            .chain(option_fields_declared_in_the_source(include_str!(
                "agent.rs"
            )))
            .collect();
        assert!(
            fields.len() > 60,
            "only {} Option<> struct fields parsed - the scanner has drifted",
            fields.len()
        );

        let bundle = bundle();
        let defs = bundle["$defs"].as_object().expect("$defs");
        let mut seen_not_in_defs = BTreeSet::new();
        let mut checked = 0;
        let mut mismatches = Vec::new();
        for field in &fields {
            let Some(def) = defs.get(&field.struct_name) else {
                seen_not_in_defs.insert((field.struct_name.as_str(), field.field.as_str()));
                continue;
            };
            let required: BTreeSet<&str> = def["required"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let should_be_required = !field.has_default && !field.has_skip;
            let is_required = required.contains(field.field.as_str());
            checked += 1;
            if should_be_required != is_required {
                mismatches.push(format!(
                    "{}.{}: serde requires it on the wire = {should_be_required}, \
                     the bundle's `required` says {is_required}",
                    field.struct_name, field.field
                ));
            }
        }
        assert!(checked > 60, "only {checked} fields checked against $defs");
        assert!(
            mismatches.is_empty(),
            "the bundle's `required` disagrees with what serde actually does:\n{}",
            mismatches.join("\n")
        );

        let expected: BTreeSet<_> = NOT_IN_DEFS.iter().copied().collect();
        assert_eq!(
            seen_not_in_defs, expected,
            "fields the scan could not check against $defs changed; update NOT_IN_DEFS \
             and its reasoning, or investigate why a struct dropped out of $defs"
        );
    }

    #[test]
    fn the_ref_pattern_accepts_exactly_what_the_parsers_do() {
        let bundle = bundle();
        let samples = [
            "0", "5", "-5", "42", "h3.7", "h3.0", "h3.-7", "h12.345", "", "-0", "+7", "07", " 5",
            "5 ", "h0.7", "h03.7", "h-3.7", "h+3.7", "h3.07", "h3.-0", "h3.", "h.7", "3.7", "h3x7",
            "h3.7.8", "H3.7",
        ];
        for name in ["WireTabRef", "WireProjectRef"] {
            let schema = json!({
                "$schema": DRAFT_2020_12,
                "$defs": bundle["$defs"],
                "$ref": format!("#/$defs/{name}"),
            });
            let validator = jsonschema::draft202012::new(&schema).expect("the schema compiles");
            for text in samples {
                let parses = match name {
                    "WireTabRef" => WireTabRef::parse(text).is_some(),
                    _ => WireProjectRef::parse(text).is_some(),
                };
                assert_eq!(
                    validator.is_valid(&json!(text)),
                    parses,
                    "{name}: the pattern and the parser disagree on {text:?}"
                );
            }
        }
    }

    fn validator_for(bundle: &Value, name: &str) -> jsonschema::Validator {
        let schema = json!({
            "$schema": DRAFT_2020_12,
            "$defs": bundle["$defs"],
            "$ref": format!("#/$defs/{name}"),
        });
        jsonschema::draft202012::new(&schema).expect("the schema compiles")
    }

    fn read_vector(name: &str) -> Value {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ipc-vectors");
        let text =
            std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} is JSON: {e}"))
    }

    /// Finding 1 (plan 067 review): the bundle states exactly what
    /// `TryFrom<RawAttachHandshake>` requires. A handshake missing any
    /// term the conversion `ok_or`s on must fail, and the four real
    /// request vectors — which the conversion accepts — must still pass.
    #[test]
    fn attach_handshake_requires_what_try_from_requires() {
        let bundle = bundle();
        let validator = validator_for(&bundle, "AttachHandshake");

        assert!(
            !validator.is_valid(&json!({"attach": "5", "protocol_version": 6})),
            "a minimal handshake with none of the terms must not validate"
        );

        let vectors = [
            "attach.handshake.request.json",
            "attach.handshake.resume.request.json",
            "attach.handshake.unfocused.request.json",
            "attach.handshake.vt.request.json",
        ];
        for name in vectors {
            let vector = read_vector(name);
            let errors: Vec<_> = validator.iter_errors(&vector).collect();
            assert!(errors.is_empty(), "{name} should validate: {errors:?}");
        }
    }

    /// Finding 1's other half: the bundle states exactly what
    /// `TryFrom<RawAttachHandshakeReply>` requires per arm — an accepted
    /// reply needs every `AttachAccepted` field, a rejected reply needs
    /// only `error` — and the real accepted/rejected vectors must still
    /// pass.
    #[test]
    fn attach_handshake_reply_arms_require_their_own_fields() {
        let bundle = bundle();
        let validator = validator_for(&bundle, "AttachHandshakeReply");

        assert!(
            !validator.is_valid(&json!({"ok": true})),
            "an accepted reply with none of AttachAccepted's fields must not validate"
        );
        assert!(
            !validator.is_valid(&json!({"ok": false})),
            "a rejected reply with no `error` must not validate"
        );

        for name in [
            "attach.handshake.accepted.json",
            "attach.handshake.rejected.json",
        ] {
            let vector = read_vector(name);
            let errors: Vec<_> = validator.iter_errors(&vector).collect();
            assert!(errors.is_empty(), "{name} should validate: {errors:?}");
        }
    }

    /// Ops and events no vector exemplifies, and why. The list only
    /// shrinks: a name here that gains a vector fails the fidelity test
    /// until it is taken out.
    const NO_VECTOR: &[(&str, &str)] = {
        const EMPTY_REPLY: &str = "answers {}, and no exemplar of its params has been written";
        const NOT_WRITTEN: &str = "no exemplar has been written";
        const PALETTE_REPLY: &str =
            "answers palette.state's result, which palette.state.agents.response.json exemplifies";
        const TEST_SEAM: &str = "a test seam (ipc.md files it test-only), not a client contract";
        const MACOS_SEAM: &str = "a ROOST_TEST_MODE seam macOS iced alone serves";
        const UI_READ: &str = "a read of the window's own state; no exemplar has been written";
        &[
            (ops::TAB_CLOSE, EMPTY_REPLY),
            (ops::TAB_RESIZE, EMPTY_REPLY),
            (ops::PROJECT_CREATE, NOT_WRITTEN),
            (ops::PROJECT_RENAME, EMPTY_REPLY),
            (ops::PROJECT_DELETE, EMPTY_REPLY),
            (ops::TAB_REORDER, EMPTY_REPLY),
            (ops::PROJECT_REORDER, EMPTY_REPLY),
            (ops::TAB_FOCUS, NOT_WRITTEN),
            (ops::TAB_SET_TITLE, EMPTY_REPLY),
            (ops::TAB_SET_STATE, EMPTY_REPLY),
            (ops::TAB_SET_HOOK_ACTIVE, EMPTY_REPLY),
            (ops::NOTIFICATION_CREATE, EMPTY_REPLY),
            (ops::APP_ACTIVATE, EMPTY_REPLY),
            (ops::SCREENSHOT, UI_READ),
            (ops::PALETTE_OPEN, PALETTE_REPLY),
            (ops::PALETTE_QUERY, PALETTE_REPLY),
            (ops::PALETTE_ACTIVATE, PALETTE_REPLY),
            (ops::PALETTE_DISMISS, PALETTE_REPLY),
            (ops::SELECTION_SET, TEST_SEAM),
            (ops::SELECTION_CLEAR, TEST_SEAM),
            (ops::SELECTION_DUMP, TEST_SEAM),
            (ops::CLIPBOARD_DUMP, TEST_SEAM),
            (ops::CLIPBOARD_WRITE, TEST_SEAM),
            (ops::TAB_FEED_IME, TEST_SEAM),
            (ops::SIDEBAR_SET_WIDTH, TEST_SEAM),
            (ops::TAB_DISPATCH_MOUSE_EVENT, TEST_SEAM),
            (ops::APP_SET_WINDOW_FOCUS, TEST_SEAM),
            (ops::APP_DIALOG_DUMP, TEST_SEAM),
            (ops::APP_DIALOG_ANSWER, TEST_SEAM),
            (ops::APP_KEYBIND_DISPATCH, TEST_SEAM),
            (ops::APP_CURSOR_SHAPE, UI_READ),
            (ops::APP_ACTIVE_TERMINAL_FOCUSED, UI_READ),
            (ops::APP_SELECTED_TAB_ID, UI_READ),
            (ops::APP_DOCK_BADGE, MACOS_SEAM),
            (ops::APP_MENU_DUMP, MACOS_SEAM),
            (ops::APP_MENU_ACTIVATE, MACOS_SEAM),
            (ops::APP_UPDATE_STATUS, MACOS_SEAM),
            (ops::APP_UPDATE_CHECK, MACOS_SEAM),
            (ops::APP_NOTIFICATION_STATUS, MACOS_SEAM),
            (ops::EVENT_TAB_CLOSED, NOT_WRITTEN),
            (ops::EVENT_TAB_TITLE_CHANGED, NOT_WRITTEN),
            (ops::EVENT_TAB_CWD_CHANGED, NOT_WRITTEN),
            (ops::EVENT_TAB_NOTIFICATION, NOT_WRITTEN),
            (ops::EVENT_PROJECT_CREATED, NOT_WRITTEN),
            (ops::EVENT_PROJECT_RENAMED, NOT_WRITTEN),
            (ops::EVENT_PROJECT_DELETED, NOT_WRITTEN),
            (ops::EVENT_HOOK_ACTIVE_CHANGED, NOT_WRITTEN),
        ]
    };

    /// Validates instances against locations in one bundle, compiling
    /// each location once.
    struct Fidelity {
        bundle: Value,
        compiled: HashMap<String, jsonschema::Validator>,
        errors: Vec<String>,
    }

    impl Fidelity {
        fn check(&mut self, file: &str, pointer: &str, instance: &Value) {
            let bundle = &self.bundle;
            let validator = self.compiled.entry(pointer.to_string()).or_insert_with(|| {
                let target = bundle
                    .pointer(pointer)
                    .unwrap_or_else(|| panic!("the bundle has nothing at {pointer}"));
                let root = json!({
                    "$schema": bundle["$schema"],
                    "$defs": bundle["$defs"],
                    "allOf": [target],
                });
                jsonschema::draft202012::new(&root)
                    .unwrap_or_else(|e| panic!("{pointer} does not compile: {e}"))
            });
            for error in validator.iter_errors(instance) {
                self.errors.push(format!(
                    "{file}: {pointer} at {:?}: {error}",
                    error.instance_path().as_str()
                ));
            }
        }
    }

    /// The row a vector's subject names: `tab.open` for
    /// `tab.open.activate-false`, never `tab.dump` for
    /// `tab.dump_resolved`.
    fn row_for(subject: &str, event: bool) -> Option<&'static str> {
        OP_TYPES
            .iter()
            .filter(|row| is_event(row) == event)
            .map(|row| row.name)
            .filter(|name| {
                subject == *name
                    || subject
                        .strip_prefix(name)
                        .is_some_and(|rest| rest.starts_with('.'))
            })
            .max_by_key(|name| name.len())
    }

    /// `<stem>.v<N>` → `(<stem>, N)`.
    fn generation(stem: &str) -> Option<(&str, u32)> {
        let (rest, tail) = stem.rsplit_once('.')?;
        Some((rest, tail.strip_prefix('v')?.parse().ok()?))
    }

    /// The message `row_for` failing pushes, for either row kind.
    fn no_row_for(file: &str, kind: &str, subject: &str) -> String {
        format!("{file}: no {kind} row for {subject}")
    }

    #[test]
    fn every_vector_validates_against_the_bundle() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ipc-vectors");
        let mut files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .map(|entry| entry.expect("a directory entry").file_name())
            .filter_map(|name| name.into_string().ok())
            .filter(|name| name.ends_with(".json"))
            .collect();
        files.sort();

        let mut fidelity = Fidelity {
            bundle: bundle(),
            compiled: HashMap::new(),
            errors: Vec::new(),
        };
        let mut covered = BTreeSet::new();
        let mut validated = 0;
        for file in &files {
            let text = std::fs::read_to_string(dir.join(file)).expect("read a vector");
            let vector: Value = serde_json::from_str(&text).expect("a vector is JSON");
            let mut stem = file.strip_suffix(".json").expect("filtered on .json");
            if let Some((rest, n)) = generation(stem) {
                // A frozen generation records what an older session
                // answered; the current bundle describes only this one.
                if n < SESSION_PROTOCOL_VERSION {
                    continue;
                }
                assert_eq!(n, SESSION_PROTOCOL_VERSION, "{file} is from the future");
                stem = rest;
            }
            let (subject, shape) = stem.rsplit_once('.').expect("<subject>.<shape>");
            let attach = subject == "attach.handshake" || subject.starts_with("attach.handshake.");
            match (subject, shape) {
                ("events", "batch") => {
                    fidelity.check(file, "/envelopes/event_batch", &vector);
                    for event in vector["events"].as_array().expect("a batch has events") {
                        let name = event["event"].as_str().expect("an event is named");
                        if row_for(name, true) != Some(name) {
                            fidelity.errors.push(no_row_for(file, "events", name));
                            continue;
                        }
                        fidelity.check(file, &format!("/events/{name}"), &event["data"]);
                        covered.insert(name.to_string());
                    }
                }
                ("response", "error") => fidelity.check(file, "/envelopes/error", &vector),
                (_, "request") if attach => {
                    fidelity.check(file, "/$defs/AttachHandshake", &vector);
                }
                (_, "accepted" | "rejected") if attach => {
                    fidelity.check(file, "/$defs/AttachHandshakeReply", &vector);
                }
                (SESSION_STOPPING_EVENT, "event") => {
                    fidelity.check(file, "/envelopes/session_stopping", &vector);
                }
                (STREAM_ENDED_EVENT, "event") => {
                    fidelity.check(file, "/envelopes/stream_ended", &vector);
                }
                (_, "event") => {
                    let Some(name) = row_for(subject, true) else {
                        fidelity.errors.push(no_row_for(file, "events", subject));
                        continue;
                    };
                    assert_eq!(vector["event"], name, "{file} names another event");
                    fidelity.check(file, "/$defs/EventEnvelope", &vector);
                    fidelity.check(file, &format!("/events/{name}"), &vector["data"]);
                    covered.insert(name.to_string());
                }
                (_, "request" | "response" | "error") => {
                    let Some(op) = row_for(subject, false) else {
                        fidelity.errors.push(no_row_for(file, "ops", subject));
                        continue;
                    };
                    match shape {
                        "request" => {
                            assert_eq!(vector["op"], op, "{file} names another op");
                            fidelity.check(file, "/envelopes/request", &vector);
                            let params = vector.get("params").cloned().unwrap_or(json!({}));
                            fidelity.check(file, &format!("/ops/{op}/params"), &params);
                        }
                        "response" => {
                            fidelity.check(file, "/envelopes/response", &vector);
                            fidelity.check(file, &format!("/ops/{op}/result"), &vector["result"]);
                        }
                        _ => fidelity.check(file, "/envelopes/error", &vector),
                    }
                    covered.insert(op.to_string());
                }
                _ => {
                    fidelity
                        .errors
                        .push(format!("{file}: no rule maps this name"));
                    continue;
                }
            }
            validated += 1;
        }

        for (name, _) in NO_VECTOR {
            if covered.contains(*name) {
                fidelity
                    .errors
                    .push(format!("{name} has a vector now; take it out of NO_VECTOR"));
            }
            if !OP_TYPES.iter().any(|row| row.name == *name) {
                fidelity
                    .errors
                    .push(format!("NO_VECTOR names {name}, which has no row"));
            }
        }
        for row in OP_TYPES {
            if !covered.contains(row.name) && !NO_VECTOR.iter().any(|(name, _)| *name == row.name) {
                fidelity.errors.push(format!(
                    "{} has no vector; add one, or a NO_VECTOR entry saying why not",
                    row.name
                ));
            }
        }

        assert!(validated > 90, "only {validated} vectors validated");
        assert!(
            fidelity.errors.is_empty(),
            "the vector corpus and the bundle disagree:\n{}",
            fidelity.errors.join("\n")
        );
    }
}
