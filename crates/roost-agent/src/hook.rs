//! What a hook *entrypoint* has to decide before an adapter runs.
//!
//! Two binaries carry the `agent-hook <agent>` verb — `roostctl` on the
//! client and `roost-session` on a host — and both have to read
//! `ROOST_TAB_ID` the same way, decide the same thing about stdin, and
//! find the event name in the same place. Duplicating any of that would
//! let the local and remote halves of the same install disagree about
//! which payloads count, so the pure part of the decision lives here and
//! each binary keeps only its own I/O.
//!
//! Everything here is pure — functions over bytes and JSON, plus the
//! numbers both entrypoints have to answer within: no env reads, no
//! socket, nothing this crate's charter (`lib.rs`) forbids.

use std::time::Duration;

use serde_json::Value;

use crate::common::field_alias;

/// How much stdin a hook payload may be before it is simply truncated
/// (and therefore rejected by [`hook_payload`] as unparseable). Every
/// probed agent's largest payload is orders of magnitude under this.
///
/// Shared for the same reason the parse rules are: a cap that differed
/// between the two entrypoints would let one payload count locally and
/// vanish on a host.
pub const STDIN_CAP: u64 = 1 << 20;

/// Connect budget for both hook verbs.
///
/// Claude's and codex's `PermissionRequest` are *decision* hooks: the
/// approval dialog the user is looking at blocks on this process. So
/// this path may not wait on a UI that is gone — a dead socket has to
/// fail fast, not hold the dialog.
///
/// `claude-hook` is held to the same budget rather than the untimed dial
/// it once had: `CLAUDE_HOOK_EVENTS` gained `PermissionRequest`, so a
/// freshly written `claude-settings.json` puts a decision hook on that
/// verb too, and a socket that accepts but never answers would hold the
/// dialog open forever.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Total budget from the first socket call through the last report.
///
/// Both budgets are multiplied by
/// `roost_ipc::session_launch::timeout_scale` at each use — reading that
/// is an env read, which is the entrypoint's half of the job.
pub const TOTAL_BUDGET: Duration = Duration::from_secs(2);

/// `ROOST_TAB_ID` as a usable tab id — the one parse every hook
/// entrypoint and `doctor` share, so doctor cannot report `ok` on a
/// value the hook silently drops.
///
/// Deliberately does **not** trim: every other per-tab command reads the
/// same variable through clap, whose `i64` parser rejects surrounding
/// whitespace outright, so accepting `" 7 "` here would make doctor bless
/// a value that exits 2 everywhere else. `0` and negatives are the same
/// silent no-op as an unparseable value.
pub fn parse_tab_id(raw: &str) -> Option<i64> {
    raw.parse::<i64>().ok().filter(|id| *id > 0)
}

/// The event a payload names itself with.
///
/// `agent-hook <agent>` takes no `--event` flag: every probed agent
/// stamps its own event name into the body, so the payload is the only
/// place both halves of a hook install can agree on. grok and cursor
/// spell it camelCase, which is why this reads both — the adapters
/// normalize the value itself.
pub fn payload_event_name(payload: &Value) -> &str {
    field_alias(payload, "hook_event_name", "hookEventName")
}

/// Decode a hook's stdin into the payload the adapter should see, or
/// `None` when the body must map to nothing.
///
/// Stdin that arrived but does not parse is corrupt or foreign traffic.
/// Without this, the caller would hand the adapter a body with none of
/// the sending agent's discriminators left in it — grok and cursor both
/// execute Claude-format hooks, so that traffic is not hypothetical.
///
/// Genuinely absent stdin is the different, documented by-hand case
/// (`docs/development/claude-testing.md`): it alone is answered with
/// [`manual_payload`]. A body that *parsed* is left exactly as it
/// arrived — see that function for why synthesizing into it is unsafe.
pub fn hook_payload(stdin_buf: &[u8], tab_id: i64) -> Option<Value> {
    if stdin_buf.iter().all(u8::is_ascii_whitespace) {
        return Some(manual_payload(tab_id));
    }
    serde_json::from_slice(stdin_buf).ok()
}

/// The payload a hook invocation with **no stdin at all** stands in for.
///
/// `docs/development/claude-testing.md` drives the hook by hand exactly
/// that way, and the adapter refuses to claim ownership for an empty
/// session id (a claim supersedes unconditionally, so an id nothing can
/// match would strand the tab), which would make those bare invocations
/// silently no-op. A deterministic per-tab id keeps the manual flow
/// self-consistent: `SessionStart` claims `manual:7` and `SessionEnd`
/// releases the same one.
///
/// Deliberately **not** merged into a payload that parsed. A body is
/// only missing `session_id` because it is foreign or malformed, and
/// synthesizing one there would defeat the adapters' own empty-session
/// guard: `{"hook_event_name":"SessionStart"}` would become an
/// unconditional claim as `claude/manual:7`, evicting the tab's real
/// owner with an identity no release that owner ever sends can match.
pub fn manual_payload(tab_id: i64) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "session_id".into(),
        Value::String(format!("manual:{tab_id}")),
    );
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::claude_event_to_reports;
    use roost_ipc::agent::OwnershipAction;
    use serde_json::json;

    #[test]
    fn a_payloadless_hook_invocation_still_claims_and_releases() {
        // `docs/development/claude-testing.md` drives the hook by hand
        // with no stdin. Without a synthesized session id the adapter
        // drops SessionStart, nothing owns the tab, and every later
        // manual event is rejected by the server's ownership check.
        let payload = manual_payload(7);
        assert_eq!(
            payload.get("session_id").and_then(|v| v.as_str()),
            Some("manual:7")
        );

        let start = claude_event_to_reports("session-start", &payload, 7);
        assert_eq!(start.len(), 1, "SessionStart dropped");
        assert_eq!(start[0].ownership_action, OwnershipAction::Claim);
        assert_eq!(start[0].session_id, "manual:7");

        // The release has to carry the same identity or it won't match.
        let end = claude_event_to_reports("session-end", &payload, 7);
        assert_eq!(end[0].ownership_action, OwnershipAction::Release);
        assert_eq!(end[0].session_id, "manual:7");
    }

    /// A body that *parsed* keeps whatever identity it arrived with —
    /// including none.
    ///
    /// The synthesized `manual:<tab>` id is for absent stdin only. Merged
    /// into a parsed payload it would walk straight past each adapter's
    /// empty-session guard and turn a body that carries no owner at all
    /// into an unconditional claim, evicting the tab's real owner for an
    /// identity no release can ever match. grok and cursor both execute
    /// Claude-format hooks, so a foreign or truncated `SessionStart` is
    /// real traffic, not a hypothetical.
    #[test]
    fn a_parsed_payload_is_never_given_a_synthesized_session() {
        for body in [
            &br#"{"hook_event_name":"SessionStart"}"#[..],
            &br#"{"hook_event_name":"SessionStart","session_id":""}"#[..],
            &br#""not an object""#[..],
            &b"null"[..],
        ] {
            let payload = hook_payload(body, 7).expect("a parsed body is a payload");
            assert_ne!(
                payload.get("session_id").and_then(|v| v.as_str()),
                Some("manual:7"),
                "a session was synthesized into {}",
                String::from_utf8_lossy(body)
            );
            assert!(
                claude_event_to_reports("session-start", &payload, 7).is_empty(),
                "an ownerless SessionStart claimed the tab: {}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// Truncated or foreign stdin must map to nothing. grok and cursor
    /// both execute Claude-format hooks, so a body that fails to parse
    /// is real traffic from another agent — and synthesizing a session
    /// for it would hand `SessionStart` an unconditional claim that
    /// evicts that agent's own owner, permanently: no release it ever
    /// sends can match `manual:7`.
    #[test]
    fn stdin_that_does_not_parse_yields_no_payload() {
        for body in [
            &br#"{"hookEventName":"session_start""#[..],
            &b"not json at all"[..],
            &b"{"[..],
        ] {
            assert!(
                hook_payload(body, 7).is_none(),
                "unparseable body accepted: {}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// The by-hand flow `docs/development/claude-testing.md` documents
    /// runs the hook with no stdin at all. That is absence, not
    /// corruption, and must keep working.
    #[test]
    fn absent_stdin_still_gets_a_synthesized_session() {
        for body in [&b""[..], &b"  \n\t "[..]] {
            let payload = hook_payload(body, 7).expect("absent stdin rejected");
            assert_eq!(
                payload.get("session_id").and_then(|v| v.as_str()),
                Some("manual:7")
            );
            let start = claude_event_to_reports("session-start", &payload, 7);
            assert_eq!(start[0].ownership_action, OwnershipAction::Claim);
        }
    }

    #[test]
    fn a_well_formed_payload_reaches_the_adapter_unchanged() {
        let payload = hook_payload(br#"{"session_id":"abc123"}"#, 7).expect("rejected");
        assert_eq!(
            payload.get("session_id").and_then(|v| v.as_str()),
            Some("abc123")
        );
    }

    #[test]
    fn a_real_session_id_is_never_overwritten() {
        let payload = hook_payload(br#"{"session_id":"abc123"}"#, 7).expect("rejected");
        assert_eq!(
            payload.get("session_id").and_then(|v| v.as_str()),
            Some("abc123")
        );
    }

    #[test]
    fn a_tab_id_is_a_positive_integer_with_no_surrounding_space() {
        assert_eq!(parse_tab_id("7"), Some(7));
        for refused in ["", " 7 ", "7 ", "0", "-1", "seven", "7.0"] {
            assert_eq!(parse_tab_id(refused), None, "{refused:?}");
        }
    }

    /// Both casings, because grok pays every borrowed field twice and
    /// cursor spells its own vocabulary camelCase.
    #[test]
    fn the_event_name_is_read_in_either_casing() {
        assert_eq!(
            payload_event_name(&json!({ "hook_event_name": "SessionStart" })),
            "SessionStart"
        );
        assert_eq!(
            payload_event_name(&json!({ "hookEventName": "session_start" })),
            "session_start"
        );
        // Snake wins when both are present and non-empty.
        assert_eq!(
            payload_event_name(&json!({
                "hook_event_name": "stop",
                "hookEventName": "stop_cancelled",
            })),
            "stop"
        );
        assert_eq!(payload_event_name(&json!({})), "");
        assert_eq!(payload_event_name(&Value::Null), "");
    }

    /// Every captured hook payload names itself, because the generic
    /// verb has nowhere else to read the event from.
    ///
    /// opencode is absent on purpose: its fixture is the raw plugin
    /// event bus, and the event name is stamped on by
    /// `assets/opencode/roost-agent-state.js` as it forwards.
    #[test]
    fn every_captured_payload_names_its_own_event() {
        for agent in ["claude", "grok", "gx", "codex", "cursor"] {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("{agent}.jsonl"));
            let text = std::fs::read_to_string(&path).unwrap();
            for (i, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
                let record: Value = serde_json::from_str(line).unwrap();
                assert!(
                    !payload_event_name(&record["payload"]).is_empty(),
                    "{agent}.jsonl:{} carries no event name",
                    i + 1
                );
            }
        }
    }
}
