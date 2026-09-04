//! Replay of the captured hook probe (plan 046 §3.1) through the
//! adapters, driven the way the server drives them.
//!
//! `tests/fixtures/<agent>.jsonl` is the scrubbed 2026-09-04 capture of
//! a real session per agent — one `{"event", "payload"}` object per
//! line, in the order the hooks fired, with emails and machine paths
//! replaced and session ids kept. The Claude file holds two
//! back-to-back sessions; the second is the one that hit a permission
//! dialog.
//!
//! Every emitted report is applied through
//! [`roost_ipc::agent::apply_report`] rather than merely inspected,
//! because half of what this commit changed is `lifecycle_if` — a field
//! whose effect is invisible unless something owns a current lifecycle
//! to guard against.

use std::fs;
use std::path::PathBuf;

use roost_agent::claude::claude_event_to_reports;
use roost_ipc::agent::{
    apply_report, validate_report, AgentLifecycle, AgentTabState, AttentionEffect, Severity,
};
use serde_json::{json, Value};

const TAB: i64 = 7;
/// Server receipt time. Constant: nothing here reads it back.
const NOW: i64 = 1_757_000_000;

const SESSION_ONE: &str = "228ab1b1-4cc1-4739-9ef4-5605173fd5a0";
const SESSION_TWO: &str = "eed354f6-c5c7-4e10-ad32-fe6a8d343225";

fn fixture(agent: &str) -> Vec<(String, Value)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{agent}.jsonl"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            let record: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{}:{}: {e}", path.display(), i + 1));
            let event = record["event"]
                .as_str()
                .unwrap_or_else(|| panic!("{}:{}: no event name", path.display(), i + 1))
                .to_string();
            (event, record["payload"].clone())
        })
        .collect()
}

/// A tab as the server holds one: the agent axes plus the attention
/// effects a caller would have turned into banners.
#[derive(Default)]
struct Tab {
    state: AgentTabState,
    banners: Vec<(Severity, String)>,
    pending: bool,
}

impl Tab {
    /// Returns how many reports the event produced, so a caller can
    /// pin "this event is not mapped" as well as its effect.
    fn feed(&mut self, event: &str, payload: &Value) -> usize {
        let reports = claude_event_to_reports(event, payload, TAB);
        for report in &reports {
            validate_report(report).unwrap_or_else(|e| panic!("{event}: {e}"));
            let outcome = apply_report(&self.state, report, NOW);
            self.state = outcome.state;
            match outcome.attention {
                AttentionEffect::Set { severity, body, .. } => {
                    self.banners.push((severity, body));
                    self.pending = true;
                }
                AttentionEffect::Clear => self.pending = false,
                AttentionEffect::Unchanged => {}
            }
        }
        reports.len()
    }

    fn lifecycle(&self) -> AgentLifecycle {
        self.state.lifecycle
    }

    fn owner_session(&self) -> Option<&str> {
        self.state.ownership.as_ref().map(|o| o.session_id.as_str())
    }

    fn detail(&self) -> Option<&str> {
        self.state.ownership.as_ref().map(|o| o.detail.as_str())
    }
}

// ---------------------------------------------------------------------
// The Claude probe, start to finish
// ---------------------------------------------------------------------

#[test]
fn the_claude_probe_replays_to_the_pinned_lifecycle_sequence() {
    use AgentLifecycle::{Finished, Inactive, Waiting, Working};

    // The whole capture, one row per line of the fixture: the event, how
    // many reports it produces, and the lifecycle the tab holds
    // afterwards. Session 1 has no permission dialog; session 2 does.
    let expected: [(&str, usize, AgentLifecycle); 18] = [
        ("SessionStart", 1, Inactive),
        ("UserPromptSubmit", 1, Working),
        ("PreToolUse", 1, Working),
        ("PostToolUse", 1, Working),
        ("Stop", 1, Finished),
        // Roost registers no `SubagentStop` hook, and the adapter maps
        // no such event even when one arrives.
        ("SubagentStop", 0, Finished),
        // The ~60 s nag against a turn that already ended: guarded on
        // `working`, so it lands vetoed and the tab stays Finished.
        ("Notification", 1, Finished),
        ("UserPromptSubmit", 1, Working),
        ("SessionEnd", 1, Inactive),
        ("SessionStart", 1, Inactive),
        ("UserPromptSubmit", 1, Working),
        ("PreToolUse", 1, Working),
        // The defect this commit fixes: the dialog is visible here…
        ("PermissionRequest", 1, Waiting),
        // …and there is no second `PreToolUse` after the approval, so
        // the tab goes back to blue when the approved tool finishes.
        ("PostToolUse", 1, Working),
        ("Stop", 1, Finished),
        ("SubagentStop", 0, Finished),
        ("UserPromptSubmit", 1, Working),
        ("SessionEnd", 1, Inactive),
    ];

    let records = fixture("claude");
    assert_eq!(records.len(), expected.len(), "the fixture changed shape");

    let mut tab = Tab::default();
    for (i, ((event, payload), (want_event, want_reports, want_lifecycle))) in
        records.iter().zip(expected).enumerate()
    {
        let line = i + 1;
        assert_eq!(event, want_event, "claude.jsonl:{line}");
        assert_eq!(
            tab.feed(event, payload),
            want_reports,
            "claude.jsonl:{line} {event} report count"
        );
        assert_eq!(
            tab.lifecycle(),
            want_lifecycle,
            "claude.jsonl:{line} {event} lifecycle"
        );

        match line {
            1..=8 => assert_eq!(tab.owner_session(), Some(SESSION_ONE), "line {line}"),
            9 => assert_eq!(tab.owner_session(), None, "SessionEnd released"),
            10..=17 => assert_eq!(tab.owner_session(), Some(SESSION_TWO), "line {line}"),
            _ => assert_eq!(tab.owner_session(), None, "SessionEnd released"),
        }
    }

    // Three banners, not four: the idle_prompt nag on line 7 was
    // accepted (its detail merged) but its `attention: set` was dropped
    // with its vetoed lifecycle.
    assert_eq!(
        tab.banners,
        vec![
            (Severity::Info, "Turn complete".to_string()),
            (Severity::Warn, "Needs permission to use `Bash`".to_string()),
            (Severity::Info, "Turn complete".to_string()),
        ],
    );
    assert!(!tab.pending, "the trailing SessionEnd clears attention");
}

#[test]
fn the_idle_prompt_nag_is_accepted_even_where_its_patch_is_vetoed() {
    let records = fixture("claude");
    let mut tab = Tab::default();
    for (event, payload) in records.iter().take(7) {
        tab.feed(event, payload);
    }
    // The barrier that proves line 7 reached the state machine rather
    // than being dropped whole: `detail` merges on a vetoed report.
    assert_eq!(tab.detail(), Some("idle_prompt"));
    assert_eq!(tab.lifecycle(), AgentLifecycle::Finished);
    assert_eq!(tab.banners.len(), 1, "only the Stop banner");
}

// ---------------------------------------------------------------------
// Cross-adapter isolation (plan 046 §3.1, §9)
// ---------------------------------------------------------------------

/// grok can load a Claude-shaped settings file and cursor loads
/// `~/.claude/settings.json` unconditionally, so with Roost's Claude
/// entries installed both would run `agent-hook claude` on their own
/// events. Zero reports — not merely zero claims: the discriminators
/// reject the payload before any mapping runs.
#[test]
fn no_grok_or_cursor_event_produces_a_claude_report() {
    for agent in ["grok", "cursor"] {
        let records = fixture(agent);
        assert!(!records.is_empty(), "{agent}.jsonl is empty");
        for (i, (event, payload)) in records.iter().enumerate() {
            assert!(
                claude_event_to_reports(event, payload, TAB).is_empty(),
                "{agent}.jsonl:{}: {event} reached the Claude adapter",
                i + 1
            );
        }
    }
}

/// …and the fence is not vacuous. Both agents fire events whose names
/// normalize onto Claude's own vocabulary — grok's `SessionStart`,
/// cursor's `sessionStart`/`stop`/`preToolUse` — so with the foreign
/// discriminators removed these very payloads *do* map. The rejection
/// is what makes the test above hold, not a lucky absence of overlap.
#[test]
fn the_same_payloads_map_once_their_discriminators_are_removed() {
    for agent in ["grok", "cursor"] {
        let mapped = fixture(agent)
            .into_iter()
            .filter(|(event, payload)| {
                let mut stripped = payload.clone();
                if let Some(object) = stripped.as_object_mut() {
                    for key in ["hookEventName", "conversation_id", "cursor_version"] {
                        object.remove(key);
                    }
                }
                !claude_event_to_reports(event, &stripped, TAB).is_empty()
            })
            .count();
        assert!(
            mapped > 0,
            "{agent}.jsonl no longer overlaps Claude's vocabulary; the isolation \
             test would pass for the wrong reason"
        );
    }
}

// ---------------------------------------------------------------------
// Reordered delivery
// ---------------------------------------------------------------------

fn notification(kind: &str) -> Value {
    json!({ "session_id": "s-1", "message": "m", "notification_type": kind })
}

fn started() -> Tab {
    let mut tab = Tab::default();
    tab.feed(
        "SessionStart",
        &json!({ "session_id": "s-1", "source": "startup" }),
    );
    tab.feed("UserPromptSubmit", &json!({ "session_id": "s-1" }));
    assert_eq!(tab.lifecycle(), AgentLifecycle::Working);
    tab
}

/// Claude's notifications are timer-driven and may legally arrive after
/// the event they describe was already resolved. In the ordering the
/// probe recorded — `PermissionRequest` first — the late
/// `permission_prompt` is vetoed and nothing happens twice.
#[test]
fn a_late_permission_prompt_is_vetoed_while_the_dialog_is_still_open() {
    let mut tab = started();
    tab.feed(
        "PermissionRequest",
        &json!({ "session_id": "s-1", "tool_name": "Bash" }),
    );
    assert_eq!(tab.lifecycle(), AgentLifecycle::Waiting);
    assert_eq!(tab.banners.len(), 1);

    tab.feed("Notification", &notification("permission_prompt"));
    assert_eq!(tab.lifecycle(), AgentLifecycle::Waiting);
    assert_eq!(tab.banners.len(), 1, "the prompt must banner exactly once");
    assert_eq!(tab.detail(), Some("permission_prompt"));
}

/// The honest edge, asserted as it actually behaves rather than as one
/// would like it to: if the notification is slow enough that the tool
/// already ran, `PostToolUse` has taken the tab back to `working` and
/// the guard *passes*, so the stale prompt re-blocks the tab and
/// banners. The guard defends the common in-order case; it cannot
/// distinguish a stale notification from a fresh one, and the tab
/// recovers on the next real signal.
#[test]
fn a_permission_prompt_that_arrives_after_the_approval_re_blocks_the_tab() {
    let mut tab = started();
    tab.feed(
        "PermissionRequest",
        &json!({ "session_id": "s-1", "tool_name": "Bash" }),
    );
    tab.feed(
        "PostToolUse",
        &json!({ "session_id": "s-1", "tool_name": "Bash" }),
    );
    assert_eq!(tab.lifecycle(), AgentLifecycle::Working);

    tab.feed("Notification", &notification("permission_prompt"));
    assert_eq!(tab.lifecycle(), AgentLifecycle::Waiting);
    assert_eq!(tab.banners.len(), 2);

    // …and the next real signal clears it, so the wart is transient.
    tab.feed("Stop", &json!({ "session_id": "s-1" }));
    assert_eq!(tab.lifecycle(), AgentLifecycle::Finished);
}

/// A failed turn is news the nag must not overwrite: `failed` is the
/// loudest lifecycle Roost has and a ~60 s timer is not evidence the
/// failure resolved.
#[test]
fn a_late_idle_prompt_leaves_a_failed_turn_failed() {
    let mut tab = started();
    tab.feed(
        "StopFailure",
        &json!({ "session_id": "s-1", "error": "rate_limit" }),
    );
    assert_eq!(tab.lifecycle(), AgentLifecycle::Failed);
    let banners = tab.banners.len();

    tab.feed("Notification", &notification("idle_prompt"));
    assert_eq!(tab.lifecycle(), AgentLifecycle::Failed);
    assert_eq!(tab.banners.len(), banners, "a vetoed nag does not banner");
    assert_eq!(tab.detail(), Some("idle_prompt"));
}

/// The case the guard exists to serve: after an Esc interrupt there is
/// no `Stop`, so `idle_prompt` is the only later signal — and from
/// `working` it applies, ending the turn.
#[test]
fn an_idle_prompt_ends_an_interrupted_turn() {
    let mut tab = started();
    tab.feed("Notification", &notification("idle_prompt"));
    assert_eq!(tab.lifecycle(), AgentLifecycle::Finished);
    assert_eq!(tab.banners.len(), 1);
    assert_eq!(tab.banners[0].0, Severity::Info);
}
