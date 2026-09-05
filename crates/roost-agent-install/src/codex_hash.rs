//! codex's hook trust hash and state key — a port, not a guess.
//!
//! codex gates hook changes behind a review dialog. It stores, per
//! handler, a `[hooks.state."<key>"] trusted_hash`; a handler whose hash
//! does not match asks the user again on the next launch. Roost writes
//! the hash it *knows* codex will compute, so wiring an agent does not
//! hand the user a dialog they did not ask for. (That this is Roost
//! approving its own command on the user's behalf is a stance, not an
//! accident — `docs/guides/agents.md` says so next to the verb.)
//!
//! Ported from `openai/codex@773f0b081de689b0d54f2809e7b17bfdb4c9f341`
//! (2026-09-04):
//!
//! * `codex-rs/hooks/src/lib.rs:95-123` — `hook_event_key_label`,
//!   `hook_key`
//! * `codex-rs/hooks/src/engine/discovery.rs:505-575, 742-793` —
//!   `normalize_command_hook`, `NormalizedHookIdentity`, `hook_hash`
//! * `codex-rs/config/src/fingerprint.rs:50-79` — `version_for_toml`,
//!   `canonical_json`
//! * `codex-rs/hooks/src/events/common.rs:112-128` —
//!   `matcher_pattern_for_event`
//! * `codex-rs/hooks/src/events/session_end.rs:20,23` — the 1 s / 3 s
//!   pair
//! * `codex-rs/hooks/src/output_spill.rs:12` — the 2500 default
//!
//! **Honesty about the evidence.** Exactly one vector in
//! `tests/fixtures/codex-golden-vectors.json` — `a` — is proven against
//! a hash a real codex wrote. The other nine are a faithful port of
//! source read at that revision, internally consistent and nothing more.
//! The formula is version-coupled by construction (it hashes a
//! serialized config struct, so any new field without
//! `skip_serializing_if` moves every hash), which is why doctor compares
//! the expected hash against the present one instead of assuming.
//!
//! Two details that are easy to get wrong and cost a dialog on every
//! launch when you do: the label is **snake_case** in both the key and
//! the hashed `event_name` (the CamelCase spellings only ever appear as
//! keys inside `hooks.json`), and the hashed `async` is the **declared**
//! value — codex forces `SessionEnd` to run synchronously but hashes
//! what the file says.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// `SESSION_END_DEFAULT_TIMEOUT_SEC` (`events/session_end.rs:20`).
const SESSION_END_DEFAULT_TIMEOUT_SEC: u64 = 1;
/// `SESSION_END_MAX_TIMEOUT_SEC` (`events/session_end.rs:23`).
const SESSION_END_MAX_TIMEOUT_SEC: u64 = 3;
/// `discovery.rs:762`.
const DEFAULT_TIMEOUT_SEC: u64 = 600;
/// `DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT` (`output_spill.rs:12`).
const DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT: i64 = 2500;

/// Every event codex has, paired with the snake_case label that appears
/// in the state key *and* in the hashed `event_name`
/// (`hooks/src/lib.rs:95-110`).
const EVENT_LABELS: [(&str, &str); 12] = [
    ("PreToolUse", "pre_tool_use"),
    ("PermissionRequest", "permission_request"),
    ("PostToolUse", "post_tool_use"),
    ("PreCompact", "pre_compact"),
    ("PostCompact", "post_compact"),
    ("SessionStart", "session_start"),
    ("SessionEnd", "session_end"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("SubagentStart", "subagent_start"),
    ("SubagentStop", "subagent_stop"),
    ("Stop", "stop"),
    ("Interrupt", "interrupt"),
];

/// The events whose `matcher` survives into the hash. For every other
/// one `matcher_pattern_for_event` forces `None` *before* hashing, so a
/// matcher written into `hooks.json` there changes nothing.
const MATCHER_EVENTS: [&str; 9] = [
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
];

/// The events that keep `additionalContextLimit` (`discovery.rs:536-552`).
const ADDITIONAL_CONTEXT_EVENTS: [&str; 5] = [
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "UserPromptSubmit",
    "SubagentStart",
];

/// The snake_case label for a `hooks.json` event key, or `None` for a
/// name codex does not have.
pub fn event_label(event: &str) -> Option<&'static str> {
    EVENT_LABELS
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, label)| *label)
}

/// The `hooks.json` event key a state-key label came from — the reverse
/// of [`event_label`], for reading a `[hooks.state]` table back.
pub fn event_for_label(label: &str) -> Option<&'static str> {
    EVENT_LABELS
        .iter()
        .find(|(_, name)| *name == label)
        .map(|(event, _)| *event)
}

/// One `hooks.json` command handler, as codex normalizes it before
/// hashing.
///
/// Roost writes only `command`, `timeout` and `async`; the other three
/// exist so a drift check over a *user's* entry models the same object
/// codex does and does not report drift that isn't there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handler {
    pub command: String,
    pub timeout_sec: Option<u64>,
    pub is_async: bool,
    pub status_message: Option<String>,
    pub additional_context_limit: Option<i64>,
}

impl Handler {
    /// The shape Roost installs: synchronous, with an explicit timeout.
    pub fn roost(command: impl Into<String>, timeout_sec: u64) -> Handler {
        Handler {
            command: command.into(),
            timeout_sec: Some(timeout_sec),
            is_async: false,
            status_message: None,
            additional_context_limit: None,
        }
    }
}

/// `normalize_command_hook` (`discovery.rs:742-764`).
///
/// The clamp is why the timeout Roost *writes* is free to differ from
/// the one it used to write: on `SessionEnd` and `Interrupt` codex
/// hashes **3** whatever the file declares above it, so a declared 10
/// and a declared 3 land on the same `trusted_hash`.
///
/// It is also what `codex::hook_timeout_secs` writes with — running the
/// declared value through codex's own normalizer is what makes "write
/// it, then hash it" a fixed point rather than a coincidence.
pub fn normalize_timeout(event: &str, timeout_sec: Option<u64>) -> u64 {
    if event == "SessionEnd" || event == "Interrupt" {
        return timeout_sec
            .unwrap_or(SESSION_END_DEFAULT_TIMEOUT_SEC)
            .clamp(SESSION_END_DEFAULT_TIMEOUT_SEC, SESSION_END_MAX_TIMEOUT_SEC);
    }
    timeout_sec.unwrap_or(DEFAULT_TIMEOUT_SEC).max(1)
}

/// The `sha256:…` value codex will compare against, or `None` for an
/// event codex does not have.
pub fn trusted_hash(event: &str, matcher: Option<&str>, handler: &Handler) -> Option<String> {
    let label = event_label(event)?;

    let mut hooked = Map::new();
    hooked.insert("type".into(), json!("command"));
    hooked.insert("command".into(), json!(handler.command));
    hooked.insert(
        "timeout".into(),
        json!(normalize_timeout(event, handler.timeout_sec)),
    );
    // No `skip_serializing_if`, so `false` is emitted too — and it is
    // the *declared* value, not the effective one.
    hooked.insert("async".into(), json!(handler.is_async));
    if let Some(message) = &handler.status_message {
        hooked.insert("statusMessage".into(), json!(message));
    }
    if let Some(limit) = handler.additional_context_limit {
        if ADDITIONAL_CONTEXT_EVENTS.contains(&event) && limit != DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT {
            hooked.insert("additionalContextLimit".into(), json!(limit));
        }
    }

    let mut identity = Map::new();
    identity.insert("event_name".into(), json!(label));
    if let Some(matcher) = matcher.filter(|_| MATCHER_EVENTS.contains(&event)) {
        identity.insert("matcher".into(), json!(matcher));
    }
    identity.insert("hooks".into(), Value::Array(vec![Value::Object(hooked)]));

    Some(format!("sha256:{}", hex(&sha256(&canonical(&identity)))))
}

/// The `[hooks.state]` key for a handler, or `None` for an unknown
/// event.
///
/// `<abs hooks.json path>:<label>:<group index>:<handler index>`, both
/// indices 0-based (`.enumerate()` at `discovery.rs:487,503`).
pub fn state_key(
    hooks_json: &str,
    event: &str,
    group_index: usize,
    handler_index: usize,
) -> Option<String> {
    let label = event_label(event)?;
    Some(format!(
        "{hooks_json}:{label}:{group_index}:{handler_index}"
    ))
}

/// Split a `[hooks.state]` key back into its parts.
///
/// **From the right**, always: a Windows path (`C:\…`) puts a colon in
/// the first field, so splitting from the left silently mis-parses it.
/// Roost is Mac + Linux, but the key it reads may have been written by
/// a codex that isn't.
pub fn split_state_key(key: &str) -> Option<(&str, &str, usize, usize)> {
    let (rest, handler) = key.rsplit_once(':')?;
    let (rest, group) = rest.rsplit_once(':')?;
    let (path, label) = rest.rsplit_once(':')?;
    Some((path, label, group.parse().ok()?, handler.parse().ok()?))
}

/// `canonical_json` + `serde_json::to_vec` (`fingerprint.rs:50-79`):
/// recursive key sort, array order kept, `,`/`:` separators with no
/// spaces, non-ASCII emitted raw.
///
/// The sort is `serde_json::Map`'s own — it is a `BTreeMap` — which is
/// only true while nothing in this workspace turns on the crate's
/// `preserve_order` feature. `sorting_is_what_makes_this_canonical`
/// below fails loudly if that ever changes.
fn canonical(identity: &Map<String, Value>) -> Vec<u8> {
    serde_json::to_vec(&Value::Object(identity.clone())).expect("a JSON object always serializes")
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every vector, run against the port. Vector `a` is the only one
    /// proven against a hash a real codex wrote (Charlie's
    /// `~/.codex/config.toml`, 2026-09-04); the rest are the source read
    /// at `773f0b0` and no more than that. The file records which is
    /// which in `verified_against_real_codex_state`, and this test
    /// asserts at least one is true so the honest one cannot quietly
    /// disappear. A **lower** bound, deliberately: proving a second
    /// vector against a real codex is strictly better evidence, and a
    /// gate that turned red for it would be a gate arguing against its
    /// own purpose.
    #[test]
    fn the_golden_vectors_from_codex_773f0b0_all_reproduce() {
        let raw = include_str!("../tests/fixtures/codex-golden-vectors.json");
        let doc: Value = serde_json::from_str(raw).expect("golden vectors parse");
        let vectors = doc["vectors"].as_array().expect("vectors array");
        assert_eq!(vectors.len(), 10, "the vector set changed size");

        let mut verified = 0;
        for vector in vectors {
            let id = vector["id"].as_str().unwrap();
            let input = &vector["input"];
            let event = input["event_name"].as_str().unwrap();
            let handler = Handler {
                command: input["command"].as_str().unwrap().to_string(),
                timeout_sec: input.get("timeout").and_then(Value::as_u64),
                is_async: input.get("async").and_then(Value::as_bool).unwrap_or(false),
                status_message: input
                    .get("statusMessage")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                additional_context_limit: input
                    .get("additionalContextLimit")
                    .and_then(Value::as_i64),
            };

            let got = trusted_hash(
                event,
                input.get("matcher").and_then(Value::as_str),
                &handler,
            );
            assert_eq!(
                got.as_deref(),
                vector["expected_hash"].as_str(),
                "hash mismatch for {id}"
            );

            let key = state_key(
                input["hooks_json_path"].as_str().unwrap(),
                event,
                input["group_index"].as_u64().unwrap() as usize,
                input["handler_index"].as_u64().unwrap() as usize,
            );
            assert_eq!(
                key.as_deref(),
                vector["expected_key"].as_str(),
                "key mismatch for {id}"
            );

            if vector["verified_against_real_codex_state"] == Value::Bool(true) {
                verified += 1;
            }
        }
        assert!(
            verified >= 1,
            "no vector is empirically proven any more — the one hash a real \
             codex wrote has been dropped or unflagged"
        );
    }

    /// The canonicalization rests on `serde_json::Map` being a
    /// `BTreeMap`. Turning on `serde_json`'s `preserve_order` anywhere
    /// in this workspace would swap it for an `IndexMap` and silently
    /// change every hash Roost writes — into a review dialog on every
    /// codex launch, with nothing else failing to say why.
    #[test]
    fn sorting_is_what_makes_this_canonical() {
        let mut map = Map::new();
        map.insert("zebra".into(), json!(1));
        map.insert("apple".into(), json!(2));
        assert_eq!(
            String::from_utf8(canonical(&map)).unwrap(),
            r#"{"apple":2,"zebra":1}"#,
            "serde_json::Map is no longer sorted — is `preserve_order` on?"
        );
    }

    /// Roost writes `timeout: 10` uniformly, and codex hashes 3 of it on
    /// two events. Getting the clamp wrong is invisible until a user
    /// sees a dialog.
    #[test]
    fn the_session_end_and_interrupt_clamp_is_hashed() {
        assert_eq!(normalize_timeout("SessionEnd", Some(10)), 3);
        assert_eq!(normalize_timeout("Interrupt", Some(10)), 3);
        assert_eq!(normalize_timeout("SessionEnd", None), 1);
        assert_eq!(normalize_timeout("Interrupt", Some(0)), 1);
        assert_eq!(normalize_timeout("SessionStart", Some(10)), 10);
        assert_eq!(normalize_timeout("SessionStart", None), 600);
        assert_eq!(normalize_timeout("Stop", Some(0)), 1);

        // And the clamp shows up in the hash, not just the helper: a
        // SessionEnd written with 10 hashes the same as one written
        // with 3.
        let ten = trusted_hash("SessionEnd", None, &Handler::roost("x", 10));
        let three = trusted_hash("SessionEnd", None, &Handler::roost("x", 3));
        assert_eq!(ten, three);
        assert_ne!(
            ten,
            trusted_hash("SessionStart", None, &Handler::roost("x", 10))
        );
    }

    #[test]
    fn a_matcher_counts_only_on_the_events_that_have_one() {
        let handler = Handler::roost("cmd", 10);
        // PreToolUse keeps it, so the hash moves.
        assert_ne!(
            trusted_hash("PreToolUse", Some("Bash"), &handler),
            trusted_hash("PreToolUse", None, &handler)
        );
        // Stop, UserPromptSubmit and Interrupt drop it before hashing.
        for event in ["Stop", "UserPromptSubmit", "Interrupt"] {
            assert_eq!(
                trusted_hash(event, Some("Bash"), &handler),
                trusted_hash(event, None, &handler),
                "{event}"
            );
        }
    }

    #[test]
    fn labels_round_trip_both_ways() {
        for (event, label) in EVENT_LABELS {
            assert_eq!(event_label(event), Some(label));
            assert_eq!(event_for_label(label), Some(event));
        }
        assert_eq!(
            event_for_label("SessionStart"),
            None,
            "labels are snake_case"
        );
    }

    #[test]
    fn an_unknown_event_has_neither_a_hash_nor_a_key() {
        assert_eq!(event_label("NotAnEvent"), None);
        assert_eq!(
            trusted_hash("NotAnEvent", None, &Handler::roost("x", 10)),
            None
        );
        assert_eq!(state_key("/p/hooks.json", "NotAnEvent", 0, 0), None);
    }

    #[test]
    fn a_key_splits_from_the_right_so_a_windows_drive_survives() {
        assert_eq!(
            split_state_key("/Users/c/.codex/hooks.json:session_start:0:0"),
            Some(("/Users/c/.codex/hooks.json", "session_start", 0, 0))
        );
        assert_eq!(
            split_state_key(r"C:\Users\c\.codex\hooks.json:stop:1:2"),
            Some((r"C:\Users\c\.codex\hooks.json", "stop", 1, 2))
        );
        assert_eq!(split_state_key("no-colons"), None);
        assert_eq!(split_state_key("/p:session_start:zero:0"), None);
    }

    /// Non-ASCII is emitted raw by `serde_json`, so a port that escapes
    /// it (Python's default, JavaScript's `JSON.stringify` for some
    /// code points) computes a different hash. Vector `d` covers the
    /// same ground; this names the reason.
    #[test]
    fn non_ascii_is_not_escaped() {
        let mut map = Map::new();
        map.insert("k".into(), json!("café 目录"));
        assert_eq!(
            String::from_utf8(canonical(&map)).unwrap(),
            r#"{"k":"café 目录"}"#
        );
    }
}
