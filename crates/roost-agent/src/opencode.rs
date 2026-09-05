//! OpenCode adapter — plan 046 §3.1.
//!
//! OpenCode has no command hooks, so the events reach `roostctl` a step
//! removed: `assets/opencode/roost-agent-state.js` subscribes to
//! opencode's plugin event bus and forwards a whitelist of bus events to
//! `"$ROOST_AGENT_HOOK" agent-hook opencode` as stdin JSON
//! `{...event.properties, "hook_event_name": event.type, "session_id":
//! <root session>}`. The plugin carries **no policy** — this module is
//! the single source of truth for what each event means, so opencode's
//! mapping is replayable from a fixture exactly like the other four.
//!
//! # Verified bus contract
//!
//! Captured live against opencode `1.18.23` on 2026-09-04 (plan 046's
//! probe): the raw bus log, 862 records, of which 19 are events the
//! plugin forwards. Field names are camelCase with an all-caps `ID`
//! suffix (`sessionID`, `messageID`) and there is no snake_case
//! counterpart anywhere — this is opencode's internal bus schema, not a
//! hook envelope borrowed from Claude.
//!
//! ```text
//! session.created     sessionID, info{id, version, directory, title, agent,
//!                                     model{id, providerID}, …}
//! chat.message        sessionID, agent
//! session.status      sessionID, status{type: "busy" | "idle"}
//! permission.asked    id, sessionID, permission, patterns, metadata, tool{…}
//! permission.replied  sessionID, requestID, reply
//! question.asked      id, sessionID, questions[]{question, header, options}
//! question.replied    sessionID, requestID, answers[]
//! session.idle        sessionID
//! session.error       sessionID, error{name, data{message}}
//! dispose             (the plugin's own teardown hook, see below)
//! ```
//!
//! The two `question.*` rows are the exception: the probe never fired
//! either, so their field lists come from opencode's own published
//! types (`@opencode-ai/sdk`, `types.gen.d.ts`'s `QuestionRequest` /
//! `QuestionInfo`) rather than from a capture. Everything else above is
//! what the bus actually sent.
//!
//! The other 843 records are `message.part.delta` (697 of them),
//! `plugin.added`, `message.updated` and friends. The plugin's whitelist
//! exists so that flood never spawns a process; this adapter maps none
//! of them either, so the whitelist is a cost control, not a
//! correctness boundary.
//!
//! # Session identity
//!
//! Ownership's `session_id` is opencode's **root** session id. The
//! plugin tracks it (a `session.created` with no `parentID`) and stamps
//! it onto every forwarded event, so a child session's
//! `permission.asked` reports against the root the user is actually
//! looking at. This module reads `session_id` first and falls back to
//! the bus event's own `sessionID`, which is what lets the raw probe
//! log replay through it unchanged.
//!
//! A child `session.created` is dropped rather than claimed: a claim
//! supersedes any live owner, and a subagent taking the tab from its own
//! parent is never what the user meant. The plugin still forwards it,
//! and still learns from it — when the *first* creation it sees is a
//! child (`opencode attach`, or a plugin loaded mid-session) it adopts
//! that child's `parentID` as the root, so the child's later
//! `permission.asked` is scoped to a session the tab's owner can match.
//!
//! # Known limits (plan §4, §9)
//!
//! **`dispose` may never fire.** It is declared in opencode's plugin
//! `Hooks` type but the probe never observed it. If opencode exits
//! without calling it, ownership survives as a label until the OSC 133
//! failsafe drops the lifecycle at the next shell prompt — exactly what
//! happens to a killed Claude.
//!
//! **`opencode attach`** to a server started from another tab reports
//! against *that* tab: the plugin runs in the server process, which
//! inherited the other tab's `ROOST_TAB_ID`. A `session.created` whose
//! `directory` matches the tab's cwd is not a reliable fix, so this is
//! documented rather than guessed at.

use roost_ipc::agent::{
    AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::Value;

use crate::common::{field, field_alias, non_empty, parse_normalized};

pub const SOURCE: &str = "opencode";

/// The forwarding plugin, compiled in.
///
/// It ships beside the adapter rather than beside the install engine so
/// the whitelist below and the one in the file cannot be edited apart:
/// `every_forwarded_event_is_in_the_plugin` reads these same bytes.
/// `roost-agent-install` writes them out under its own header.
pub const PLUGIN_SOURCE: &str = include_str!("../assets/opencode/roost-agent-state.js");

/// What the plugin is called on disk. opencode loads every `.js` in its
/// `plugins/` directory, so the name is only an identity, not a hook.
pub const PLUGIN_FILE_NAME: &str = "roost-agent-state.js";

/// The bus events the plugin forwards, plus its own `dispose` teardown.
///
/// Unlike the other four agents this list is not written into a config
/// file of hook registrations — opencode has none. It is the plugin's
/// whitelist, and `assets/opencode/roost-agent-state.js` must agree with
/// it (pinned by a test below).
pub const OPENCODE_HOOK_EVENTS: [&str; 10] = [
    EventKind::SessionCreated.canonical(),
    EventKind::ChatMessage.canonical(),
    EventKind::SessionStatus.canonical(),
    EventKind::PermissionAsked.canonical(),
    EventKind::PermissionReplied.canonical(),
    EventKind::QuestionAsked.canonical(),
    EventKind::QuestionReplied.canonical(),
    EventKind::SessionIdle.canonical(),
    EventKind::SessionError.canonical(),
    EventKind::Dispose.canonical(),
];

const TITLE: &str = "OpenCode";

/// opencode's name for an interrupted turn. It arrives on the same
/// `session.error` channel as a real failure and is the only value that
/// must not paint the tab red.
const ABORTED: &str = "MessageAbortedError";

/// Map one forwarded opencode event to the reports it implies.
///
/// Nothing here rejects a "foreign" payload by content, and nothing
/// needs to: opencode's vocabulary is disjoint from every other probed
/// agent's — no Claude, grok, codex or cursor event name normalizes onto
/// one of [`OPENCODE_HOOK_EVENTS`], and none of opencode's normalizes
/// onto one of theirs. The event name *is* the discriminator, so a
/// marker key would be decoration.
pub fn opencode_event_to_reports(
    event: &str,
    payload: &Value,
    tab_id: i64,
) -> Vec<TabAgentReportParams> {
    if !payload.is_object() {
        return Vec::new();
    }

    let Some(kind) = EventKind::parse(event) else {
        return Vec::new();
    };

    let session_id = field_alias(payload, "session_id", "sessionID");
    if matches!(kind, EventKind::SessionCreated) && (session_id.is_empty() || has_parent(payload)) {
        return Vec::new();
    }

    let base = TabAgentReportParams {
        session_id: session_id.to_string(),
        ..TabAgentReportParams::sessionless(tab_id, SOURCE, OwnershipAction::Preserve, None)
    };

    let report = match kind {
        EventKind::SessionCreated => session_created(base, payload),
        EventKind::ChatMessage => working_and_clear(base, "chat_message"),
        EventKind::SessionStatus => return session_status(base, payload),
        EventKind::PermissionAsked => permission_asked(base, payload),
        EventKind::PermissionReplied => turn_progress(base, "permission_replied"),
        EventKind::QuestionAsked => question_asked(base, payload),
        EventKind::QuestionReplied => turn_progress(base, "question_replied"),
        EventKind::SessionIdle => session_idle(base),
        EventKind::SessionError => session_error(base, payload),
        EventKind::Dispose => dispose(base),
    };

    vec![report]
}

/// A session with a parent is a child (subagent) session. The probe only
/// ever saw `parentID` nested inside an `info` object, but opencode's
/// bus also spreads session fields at the top level on some events, so
/// both spellings count.
fn has_parent(payload: &Value) -> bool {
    names_a_parent(payload) || payload.get("info").is_some_and(names_a_parent)
}

/// A `parentID` that actually names a session.
///
/// Absent, `null` and `""` all say the same thing — no parent — and the
/// empty string is the one a plain presence check gets wrong. Reading it
/// as a parent drops a **root** `session.created`, which is the only
/// event that claims the tab: no claim is made, no later event makes
/// one, and the tab stays unowned for the rest of the session.
fn names_a_parent(value: &Value) -> bool {
    value
        .get("parentID")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

/// A dotted path of string-keyed lookups, or `""` if any hop is missing
/// or the leaf is not a string. Only opencode nests, so this stays here
/// rather than in `common.rs`.
fn nested<'a>(payload: &'a Value, path: &[&str]) -> &'a str {
    let mut node = payload;
    for key in path {
        match node.get(key) {
            Some(next) => node = next,
            None => return "",
        }
    }
    node.as_str().unwrap_or("")
}

fn session_created(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    report.ownership_action = OwnershipAction::Claim;
    report.lifecycle = Some(AgentLifecycle::Inactive);
    report.detail = "session_created".to_string();
    for (key, path) in [
        ("model", &["info", "model", "id"][..]),
        ("agent", &["info", "agent"][..]),
        ("version", &["info", "version"][..]),
    ] {
        if let Some(value) = non_empty(nested(payload, path)) {
            report.metadata.insert(key.to_string(), value.to_string());
        }
    }
    report
}

fn working_and_clear(mut report: TabAgentReportParams, detail: &str) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.attention = AttentionOp::Clear;
    report.detail = detail.to_string();
    report
}

/// A reply to a permission or a question resumes the turn. Attention is
/// left alone rather than cleared, matching every other adapter's
/// mid-turn events: the next `chat.message` or `session.status busy`
/// clears it.
fn turn_progress(mut report: TabAgentReportParams, detail: &str) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.detail = detail.to_string();
    report
}

/// `session.status` is a level, not an edge: `busy` says the turn is
/// running, `idle` merely says it is not, and `session.idle` is the
/// event that actually ends a turn. Reporting `idle` here would finish
/// the turn a second time, and would do it *before* `session.error` on
/// an interrupt (probe lines 853-854), so it maps to nothing.
fn session_status(report: TabAgentReportParams, payload: &Value) -> Vec<TabAgentReportParams> {
    if nested(payload, &["status", "type"]) != "busy" {
        return Vec::new();
    }
    vec![working_and_clear(report, "session_status")]
}

fn permission_asked(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Waiting);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Warn;
    report.title = TITLE.to_string();
    report.body = match non_empty(field(payload, "permission")) {
        Some(permission) => format!("Needs permission: `{permission}`"),
        None => "Needs permission to continue".to_string(),
    };
    report.detail = "permission_asked".to_string();
    report
}

/// Not observed in the probe — the shape comes from opencode's own
/// `QuestionRequest` type and is pinned by a synthetic payload.
fn question_asked(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Waiting);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Warn;
    report.title = TITLE.to_string();
    report.body = first_question(payload)
        .unwrap_or("Has a question")
        .to_string();
    report.detail = "question_asked".to_string();
    report
}

/// The text for a `question.asked` banner.
///
/// The question is *not* a top-level field: `question.asked` carries a
/// `QuestionRequest`, whose `questions` array holds one `QuestionInfo`
/// per question. Roost shows one line, so it reads the first.
///
/// `header` is preferred over `question` because opencode already
/// defines it as the short form — "very short label (max 30 chars)",
/// against a `question` that is the complete, possibly multi-sentence
/// text. A banner is a label, not a transcript.
fn first_question(payload: &Value) -> Option<&str> {
    let first = payload.get("questions")?.as_array()?.first()?;
    non_empty(field(first, "header")).or_else(|| non_empty(field(first, "question")))
}

/// Guarded for the same reason cursor's `stop` is: the probe fires
/// `session.idle` three times — once for the turn that completed, and
/// twice trailing an interrupt, *after* `session.error` already ended
/// the turn and cleared attention. Unguarded, an interrupted turn
/// banners "Turn complete" twice, which is exactly what §3.1's "an
/// interrupt does not banner" rule forbids. `["working", "waiting"]`
/// leaves one banner per real turn and none for an interrupt.
fn session_idle(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Finished);
    report.lifecycle_if = Some(vec![AgentLifecycle::Working, AgentLifecycle::Waiting]);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Info;
    report.title = TITLE.to_string();
    report.body = "Turn complete".to_string();
    report.detail = "session_idle".to_string();
    report
}

/// `session.error` carries both real failures and the Esc interrupt.
/// `MessageAbortedError` is the interrupt, and an interrupt is not news:
/// it ends the turn and clears attention, the same shape codex's
/// `Interrupt` and grok's `StopCancelled` have.
fn session_error(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let name = non_empty(nested(payload, &["error", "name"])).unwrap_or("unknown");
    if name == ABORTED {
        report.lifecycle = Some(AgentLifecycle::Finished);
        report.attention = AttentionOp::Clear;
        report.detail = "message_aborted".to_string();
        return report;
    }
    report.lifecycle = Some(AgentLifecycle::Failed);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Error;
    report.title = TITLE.to_string();
    report.body = non_empty(nested(payload, &["error", "data", "message"]))
        .map(str::to_string)
        .unwrap_or_else(|| format!("Stopped: {name}"));
    report.detail = name.to_string();
    report
}

fn dispose(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.ownership_action = OwnershipAction::Release;
    report.lifecycle = Some(AgentLifecycle::Inactive);
    report.attention = AttentionOp::Clear;
    report
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    SessionCreated,
    ChatMessage,
    SessionStatus,
    PermissionAsked,
    PermissionReplied,
    QuestionAsked,
    QuestionReplied,
    SessionIdle,
    SessionError,
    Dispose,
}

impl EventKind {
    fn parse(event: &str) -> Option<EventKind> {
        parse_normalized(
            event,
            &[
                ("sessioncreated", EventKind::SessionCreated),
                ("chatmessage", EventKind::ChatMessage),
                ("sessionstatus", EventKind::SessionStatus),
                ("permissionasked", EventKind::PermissionAsked),
                ("permissionreplied", EventKind::PermissionReplied),
                ("questionasked", EventKind::QuestionAsked),
                ("questionreplied", EventKind::QuestionReplied),
                ("sessionidle", EventKind::SessionIdle),
                ("sessionerror", EventKind::SessionError),
                ("dispose", EventKind::Dispose),
            ],
        )
    }

    const fn canonical(self) -> &'static str {
        match self {
            EventKind::SessionCreated => "session.created",
            EventKind::ChatMessage => "chat.message",
            EventKind::SessionStatus => "session.status",
            EventKind::PermissionAsked => "permission.asked",
            EventKind::PermissionReplied => "permission.replied",
            EventKind::QuestionAsked => "question.asked",
            EventKind::QuestionReplied => "question.replied",
            EventKind::SessionIdle => "session.idle",
            EventKind::SessionError => "session.error",
            EventKind::Dispose => "dispose",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped plugin, read at compile time so the fence below can
    /// never drift from what the install engine will write out.
    const PLUGIN: &str = PLUGIN_SOURCE;

    /// Same discipline as the other four adapters' twin tests.
    #[test]
    fn every_event_kind_is_a_forwarded_event() {
        let all = [
            EventKind::SessionCreated,
            EventKind::ChatMessage,
            EventKind::SessionStatus,
            EventKind::PermissionAsked,
            EventKind::PermissionReplied,
            EventKind::QuestionAsked,
            EventKind::QuestionReplied,
            EventKind::SessionIdle,
            EventKind::SessionError,
            EventKind::Dispose,
        ];
        for kind in all {
            match kind {
                EventKind::SessionCreated
                | EventKind::ChatMessage
                | EventKind::SessionStatus
                | EventKind::PermissionAsked
                | EventKind::PermissionReplied
                | EventKind::QuestionAsked
                | EventKind::QuestionReplied
                | EventKind::SessionIdle
                | EventKind::SessionError
                | EventKind::Dispose => {}
            }
            assert!(
                OPENCODE_HOOK_EVENTS.contains(&kind.canonical()),
                "{} is mapped but never forwarded",
                kind.canonical()
            );
        }
        assert_eq!(all.len(), OPENCODE_HOOK_EVENTS.len());
    }

    /// The plugin's whitelist and this module's vocabulary are one
    /// list split across two languages: an event the adapter maps but
    /// the plugin never forwards is dead policy, and one the plugin
    /// forwards but the adapter drops is a process spawned for nothing.
    ///
    /// `dispose` is the one asymmetry — it is a plugin lifecycle hook,
    /// not a bus event, so it is synthesized rather than whitelisted.
    #[test]
    fn the_plugin_forwards_exactly_the_bus_events_this_module_maps() {
        let bus: Vec<&str> = OPENCODE_HOOK_EVENTS
            .iter()
            .copied()
            .filter(|name| *name != EventKind::Dispose.canonical())
            .collect();
        let whitelist = PLUGIN
            .split_once("const FORWARDED = new Set([")
            .expect("the plugin's whitelist is no longer a `FORWARDED` Set literal")
            .1
            .split_once("]);")
            .expect("unterminated FORWARDED Set literal")
            .0;

        for name in &bus {
            assert!(
                whitelist.contains(&format!("\"{name}\"")),
                "the plugin does not forward {name}"
            );
        }
        assert_eq!(
            whitelist.matches('"').count(),
            bus.len() * 2,
            "the plugin forwards an event this module does not map"
        );
        assert!(
            PLUGIN.contains(&format!("forward(\"{}\"", EventKind::Dispose.canonical())),
            "the plugin must still synthesize the dispose event"
        );
    }
}
