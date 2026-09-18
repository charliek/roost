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

    sorted(json!({
        "$schema": meta_schema,
        "$defs": generator.take_definitions(true),
        "schema_version": SCHEMA_VERSION,
        "protocol_version": crate::PROTOCOL_VERSION,
        "session_protocol_version": SESSION_PROTOCOL_VERSION,
        "envelopes": envelopes,
        "ops": ops,
        "events": events,
    }))
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
