//! Replay of the captured hook probe (plan 046 §3.1) through the
//! adapters, driven the way the server drives them.
//!
//! `tests/fixtures/<agent>.jsonl` is the scrubbed 2026-09-04 capture of
//! a real session per agent — one `{"event", "payload"}` object per
//! line, in the order the hooks fired, with emails and machine paths
//! replaced and session ids kept. The Claude file holds two back-to-back
//! sessions (the second hit a permission dialog); the grok file holds
//! five back-to-back sessions (the third hit a plan-mode permission
//! prompt); gx, codex and cursor each hold one.
//!
//! `opencode.jsonl` is a different animal: opencode has no hooks, so
//! this is its raw plugin **event bus** log, all 862 records of it. Only
//! 19 are events the plugin forwards; the other 843 are the
//! `message.part.delta` flood and its friends, kept whole because they
//! are the evidence that the whitelist earns its place.
//!
//! Every emitted report is applied through
//! [`roost_ipc::agent::apply_report`] rather than merely inspected,
//! because half of what this commit changed is `lifecycle_if` — a field
//! whose effect is invisible unless something owns a current lifecycle
//! to guard against.

use std::fs;
use std::path::PathBuf;

use roost_agent::claude::claude_event_to_reports;
use roost_agent::codex::codex_event_to_reports;
use roost_agent::cursor::cursor_event_to_reports;
use roost_agent::grok::grok_event_to_reports;
use roost_agent::opencode::{opencode_event_to_reports, OPENCODE_HOOK_EVENTS};
use roost_ipc::agent::{
    apply_report, validate_report, AgentLifecycle, AgentTabState, AttentionEffect, OwnershipAction,
    Severity, TabAgentReportParams,
};
use serde_json::{json, Value};

const TAB: i64 = 7;
/// Server receipt time. Constant: nothing here reads it back.
const NOW: i64 = 1_757_000_000;

const SESSION_ONE: &str = "228ab1b1-4cc1-4739-9ef4-5605173fd5a0";
const SESSION_TWO: &str = "eed354f6-c5c7-4e10-ad32-fe6a8d343225";

const GROK_SESSION_ONE: &str = "01a06e30-48a2-70d1-8701-5a35ba941d9a";
const GROK_SESSION_TWO: &str = "01a06e3a-b244-7662-a783-2f58e653d534";
const GROK_SESSION_THREE: &str = "01a06e3e-2d6b-7f13-bc74-f86b6c947e08";
const GROK_SESSION_FOUR: &str = "01a06e46-ef99-76f1-afa8-6842c40cd6ab";
const GROK_SESSION_FIVE: &str = "01a06e47-9664-74f3-abe5-8d5e188f3780";

const GX_SESSION: &str = "01a06e35-1139-7931-8a33-a8c5f007b745";

const CODEX_SESSION: &str = "01a06e4d-b178-7f53-bbc3-f9e551c3b56b";

const CURSOR_SESSION: &str = "206da977-c2d4-4b1f-a280-29c6e27ea973";

const OPENCODE_SESSION: &str = "ses_f91cef768ffeTI8TEd0E4v53Ov";

/// The shape every `<agent>_event_to_reports` function has (plan 046
/// §3.1's "one module per agent, one shape").
type Adapter = fn(&str, &Value, i64) -> Vec<TabAgentReportParams>;

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
    /// pin "this event is not mapped" as well as its effect. Feeds
    /// through the Claude adapter — the shorthand the hand-driven
    /// Claude tests at the bottom of this file use.
    fn feed(&mut self, event: &str, payload: &Value) -> usize {
        self.feed_via(claude_event_to_reports, event, payload)
    }

    /// Same as [`Tab::feed`], through an arbitrary adapter — what
    /// [`replay`] drives every fixture with.
    fn feed_via(&mut self, adapter: Adapter, event: &str, payload: &Value) -> usize {
        let reports = adapter(event, payload, TAB);
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

    fn metadata(&self, key: &str) -> Option<&str> {
        self.state
            .ownership
            .as_ref()
            .and_then(|o| o.metadata.get(key))
            .map(String::as_str)
    }
}

/// One line of a fixture, as the replay pins it: the event name, how
/// many reports it produces, the lifecycle the tab holds afterwards, and
/// the session that owns the tab at that point (`None` once released).
///
/// Owner is a column rather than a range match over line numbers so each
/// row states its own answer and nothing has to be renumbered when a
/// fixture grows.
type Row = (&'static str, usize, AgentLifecycle, Option<&'static str>);

/// Replay `tests/fixtures/<agent>.jsonl` through `adapter`, pinning
/// every column of `rows` line by line, and hand the finished tab back
/// so the caller can assert on the banners the whole run produced.
fn replay(adapter: Adapter, agent: &str, rows: &[Row]) -> Tab {
    let records = fixture(agent);
    assert_eq!(records.len(), rows.len(), "{agent}.jsonl changed shape");

    let mut tab = Tab::default();
    for (i, ((event, payload), (want_event, want_reports, want_lifecycle, want_owner))) in
        records.iter().zip(rows.iter().copied()).enumerate()
    {
        let line = i + 1;
        assert_eq!(event, want_event, "{agent}.jsonl:{line}");
        assert_eq!(
            tab.feed_via(adapter, event, payload),
            want_reports,
            "{agent}.jsonl:{line} {event} report count"
        );
        assert_eq!(
            tab.lifecycle(),
            want_lifecycle,
            "{agent}.jsonl:{line} {event} lifecycle"
        );
        assert_eq!(
            tab.owner_session(),
            want_owner,
            "{agent}.jsonl:{line} {event} owner"
        );
    }
    tab
}

// ---------------------------------------------------------------------
// The Claude probe, start to finish
// ---------------------------------------------------------------------

#[test]
fn the_claude_probe_replays_to_the_pinned_lifecycle_sequence() {
    use AgentLifecycle::{Finished, Inactive, Waiting, Working};

    // The whole capture, one row per line of the fixture. Session 1 has
    // no permission dialog; session 2 does.
    let tab = replay(
        claude_event_to_reports,
        "claude",
        &[
            ("SessionStart", 1, Inactive, Some(SESSION_ONE)),
            ("UserPromptSubmit", 1, Working, Some(SESSION_ONE)),
            ("PreToolUse", 1, Working, Some(SESSION_ONE)),
            ("PostToolUse", 1, Working, Some(SESSION_ONE)),
            ("Stop", 1, Finished, Some(SESSION_ONE)),
            // Roost registers no `SubagentStop` hook, and the adapter
            // maps no such event even when one arrives.
            ("SubagentStop", 0, Finished, Some(SESSION_ONE)),
            // The ~60 s nag against a turn that already ended: guarded
            // on `working`, so it lands vetoed and the tab stays
            // Finished.
            ("Notification", 1, Finished, Some(SESSION_ONE)),
            ("UserPromptSubmit", 1, Working, Some(SESSION_ONE)),
            ("SessionEnd", 1, Inactive, None),
            ("SessionStart", 1, Inactive, Some(SESSION_TWO)),
            ("UserPromptSubmit", 1, Working, Some(SESSION_TWO)),
            ("PreToolUse", 1, Working, Some(SESSION_TWO)),
            // The defect this commit fixes: the dialog is visible here…
            ("PermissionRequest", 1, Waiting, Some(SESSION_TWO)),
            // …and there is no second `PreToolUse` after the approval,
            // so the tab goes back to blue when the approved tool
            // finishes.
            ("PostToolUse", 1, Working, Some(SESSION_TWO)),
            ("Stop", 1, Finished, Some(SESSION_TWO)),
            ("SubagentStop", 0, Finished, Some(SESSION_TWO)),
            ("UserPromptSubmit", 1, Working, Some(SESSION_TWO)),
            ("SessionEnd", 1, Inactive, None),
        ],
    );

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
// The grok probe, start to finish (five back-to-back sessions)
// ---------------------------------------------------------------------

#[test]
fn the_grok_probe_replays_to_the_pinned_lifecycle_sequence() {
    use AgentLifecycle::{Finished, Inactive, Waiting, Working};

    // One row per line of grok.jsonl. Every line maps to exactly one
    // report — unlike Claude's fixture there is no `SubagentStop`
    // near-miss in grok's vocabulary.
    //
    // `StopCancelled` never touches ownership (only `SessionEnd` and the
    // release-only `Release` action do), so each session still owns the
    // tab on its own `StopCancelled` line.
    let tab = replay(
        grok_event_to_reports,
        "grok",
        &[
            ("SessionStart", 1, Inactive, Some(GROK_SESSION_ONE)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_ONE)),
            ("PreToolUse", 1, Working, Some(GROK_SESSION_ONE)),
            ("PostToolUse", 1, Working, Some(GROK_SESSION_ONE)),
            ("Stop", 1, Finished, Some(GROK_SESSION_ONE)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_ONE)),
            ("StopCancelled", 1, Finished, Some(GROK_SESSION_ONE)),
            ("SessionEnd", 1, Inactive, None),
            // Ownership already released by the prior line: dropped by
            // the server, not the adapter (see the dedicated test
            // below).
            ("Stop", 1, Inactive, None),
            ("SessionStart", 1, Inactive, Some(GROK_SESSION_TWO)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_TWO)),
            ("PreToolUse", 1, Working, Some(GROK_SESSION_TWO)),
            ("PostToolUse", 1, Working, Some(GROK_SESSION_TWO)),
            ("Stop", 1, Finished, Some(GROK_SESSION_TWO)),
            // The ~60 s nag against a turn that already ended: guarded
            // on `working`, lands vetoed.
            ("Notification", 1, Finished, Some(GROK_SESSION_TWO)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_TWO)),
            ("StopCancelled", 1, Finished, Some(GROK_SESSION_TWO)),
            ("SessionEnd", 1, Inactive, None),
            ("Stop", 1, Inactive, None),
            ("SessionStart", 1, Inactive, Some(GROK_SESSION_THREE)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_THREE)),
            ("PreToolUse", 1, Working, Some(GROK_SESSION_THREE)),
            ("PostToolUse", 1, Working, Some(GROK_SESSION_THREE)),
            ("PreToolUse", 1, Working, Some(GROK_SESSION_THREE)),
            // The plan-mode permission prompt — grok's only blocked
            // signal, observed live (message: "Plan approval
            // requested").
            ("Notification", 1, Waiting, Some(GROK_SESSION_THREE)),
            // No second `PreToolUse` after the prompt clears, exactly
            // like Claude: the approved tool's own `PostToolUse` is what
            // takes the tab back to `working`.
            ("PostToolUse", 1, Working, Some(GROK_SESSION_THREE)),
            ("PreToolUse", 1, Working, Some(GROK_SESSION_THREE)),
            ("StopCancelled", 1, Finished, Some(GROK_SESSION_THREE)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_THREE)),
            ("SessionEnd", 1, Inactive, None),
            ("Stop", 1, Inactive, None),
            ("SessionStart", 1, Inactive, Some(GROK_SESSION_FOUR)),
            ("SessionEnd", 1, Inactive, None),
            ("Stop", 1, Inactive, None),
            ("SessionStart", 1, Inactive, Some(GROK_SESSION_FIVE)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_FIVE)),
            ("PreToolUse", 1, Working, Some(GROK_SESSION_FIVE)),
            ("PostToolUse", 1, Working, Some(GROK_SESSION_FIVE)),
            ("Stop", 1, Finished, Some(GROK_SESSION_FIVE)),
            ("Notification", 1, Finished, Some(GROK_SESSION_FIVE)),
            ("UserPromptSubmit", 1, Working, Some(GROK_SESSION_FIVE)),
            ("StopCancelled", 1, Finished, Some(GROK_SESSION_FIVE)),
            ("SessionEnd", 1, Inactive, None),
            ("Stop", 1, Inactive, None),
        ],
    );

    // Four banners: two plain "Turn complete"s, the plan-mode prompt,
    // and a third "Turn complete" — both `idle_prompt` nags (lines 15
    // and 40) were vetoed and never banner.
    assert_eq!(
        tab.banners,
        vec![
            (Severity::Info, "Turn complete".to_string()),
            (Severity::Info, "Turn complete".to_string()),
            (Severity::Warn, "Plan approval requested".to_string()),
            (Severity::Info, "Turn complete".to_string()),
        ],
    );
    assert!(!tab.pending, "the trailing SessionEnd clears attention");
}

/// Every one of grok's five sessions in the fixture ends with
/// `SessionEnd` immediately followed by a trailing `Stop{reason:
/// shutdown}`. The adapter maps that `Stop` like any other — it has no
/// way to know ownership was just released — so this is the server's
/// job: `apply_report` requires a live owner for a `Preserve` report,
/// and there is none by then. Pinned on the first session in isolation.
#[test]
fn a_trailing_shutdown_stop_after_session_end_is_dropped() {
    let mut tab = Tab::default();
    for (event, payload) in fixture("grok").iter().take(9) {
        tab.feed_via(grok_event_to_reports, event, payload);
    }
    assert_eq!(tab.owner_session(), None);
    assert_eq!(tab.lifecycle(), AgentLifecycle::Inactive);
    assert_eq!(
        tab.banners.len(),
        1,
        "only the Stop banner from line 5, not the dropped trailing one"
    );
}

// ---------------------------------------------------------------------
// The gx probe (grok's fork, same adapter, one session)
// ---------------------------------------------------------------------

#[test]
fn the_gx_probe_replays_through_the_grok_adapter() {
    use AgentLifecycle::{Finished, Inactive, Working};

    let tab = replay(
        grok_event_to_reports,
        "gx",
        &[
            ("SessionStart", 1, Inactive, Some(GX_SESSION)),
            ("UserPromptSubmit", 1, Working, Some(GX_SESSION)),
            ("PreToolUse", 1, Working, Some(GX_SESSION)),
            ("PostToolUse", 1, Working, Some(GX_SESSION)),
            ("Stop", 1, Finished, Some(GX_SESSION)),
            ("UserPromptSubmit", 1, Working, Some(GX_SESSION)),
            ("StopCancelled", 1, Finished, Some(GX_SESSION)),
            ("SessionEnd", 1, Inactive, None),
            ("Stop", 1, Inactive, None),
        ],
    );

    // gx never hit a Notification in this run, so its only banners are
    // the plain "Turn complete" from the one Stop with no in-flight work.
    assert_eq!(
        tab.banners,
        vec![(Severity::Info, "Turn complete".to_string())],
    );
    assert!(!tab.pending);
}

// ---------------------------------------------------------------------
// The codex probe, start to finish
// ---------------------------------------------------------------------

#[test]
fn the_codex_probe_replays_to_the_pinned_lifecycle_sequence() {
    use AgentLifecycle::{Finished, Inactive, Working};

    // The probe's session never triggered an approval dialog (Charlie's
    // codex config runs with approvals off), so this fixture alone does
    // not exercise `PreToolUse` or `PermissionRequest` — those two rows
    // of codex's table are pinned separately by synthetic payloads in
    // `codex_events_test.rs`, not by this replay.
    let tab = replay(
        codex_event_to_reports,
        "codex",
        &[
            ("SessionStart", 1, Inactive, Some(CODEX_SESSION)),
            ("UserPromptSubmit", 1, Working, Some(CODEX_SESSION)),
            ("PostToolUse", 1, Working, Some(CODEX_SESSION)),
            ("Stop", 1, Finished, Some(CODEX_SESSION)),
            ("UserPromptSubmit", 1, Working, Some(CODEX_SESSION)),
            ("PostToolUse", 1, Working, Some(CODEX_SESSION)),
            // The Esc-interrupt signal: ends the turn, no banner,
            // ownership continues.
            ("Interrupt", 1, Finished, Some(CODEX_SESSION)),
            ("SessionEnd", 1, Inactive, None),
        ],
    );

    assert_eq!(
        tab.banners,
        vec![(Severity::Info, "Turn complete".to_string())],
    );
    assert!(!tab.pending, "the trailing SessionEnd clears attention");
}

/// The two fields codex's `SessionStart` carries that Claude's does not
/// name the same way, read off the real capture rather than a synthetic
/// payload. Asserted on its own because ownership — and with it the
/// metadata — is gone by the end of the replay above.
#[test]
fn the_codex_session_start_records_model_and_permission_mode() {
    let mut tab = Tab::default();
    let (event, payload) = &fixture("codex")[0];
    tab.feed_via(codex_event_to_reports, event, payload);
    assert_eq!(tab.metadata("model"), Some("gpt-6-astra"));
    assert_eq!(tab.metadata("permission_mode"), Some("default"));
}

// ---------------------------------------------------------------------
// The cursor probe, start to finish
// ---------------------------------------------------------------------

#[test]
fn the_cursor_probe_replays_to_the_pinned_lifecycle_sequence() {
    use AgentLifecycle::{Finished, Inactive, Working};

    // Cursor never reaches `Waiting`: it has no permission hook at all
    // (`cursor.rs`'s module doc). The three zero-report lines are the
    // events Roost deliberately does not register.
    let tab = replay(
        cursor_event_to_reports,
        "cursor",
        &[
            ("sessionStart", 1, Inactive, Some(CURSOR_SESSION)),
            ("beforeSubmitPrompt", 1, Working, Some(CURSOR_SESSION)),
            ("afterAgentThought", 0, Working, Some(CURSOR_SESSION)),
            ("preToolUse", 1, Working, Some(CURSOR_SESSION)),
            ("afterAgentThought", 0, Working, Some(CURSOR_SESSION)),
            // The pair that would be cursor's blocked signal if it could
            // mean one: 0.1 s apart, because the command was
            // auto-approved.
            ("beforeShellExecution", 0, Working, Some(CURSOR_SESSION)),
            ("afterShellExecution", 0, Working, Some(CURSOR_SESSION)),
            ("postToolUse", 1, Working, Some(CURSOR_SESSION)),
            ("afterAgentThought", 0, Working, Some(CURSOR_SESSION)),
            ("afterAgentThought", 0, Working, Some(CURSOR_SESSION)),
            // Precedes `stop` rather than ending the turn.
            ("afterAgentResponse", 1, Working, Some(CURSOR_SESSION)),
            ("stop", 1, Finished, Some(CURSOR_SESSION)),
            ("beforeSubmitPrompt", 1, Working, Some(CURSOR_SESSION)),
            // Esc: `aborted` ends the turn…
            ("stop", 1, Finished, Some(CURSOR_SESSION)),
            // …and the `error` that follows it
            // is the same interrupt reported twice, so the guard vetoes
            // it. Reading `error` as a failure here would paint every
            // interrupted turn red.
            ("stop", 1, Finished, Some(CURSOR_SESSION)),
            ("sessionEnd", 1, Inactive, None),
        ],
    );

    // Two banners for two turns, not three for three `stop`s — the
    // whole reason `stop` carries `lifecycle_if`.
    assert_eq!(
        tab.banners,
        vec![
            (Severity::Info, "Turn complete".to_string()),
            (Severity::Info, "Turn complete".to_string()),
        ],
    );
    assert!(!tab.pending, "the trailing sessionEnd clears attention");
}

/// The `status` of each `stop` reaches the owner record even where the
/// lifecycle patch is vetoed, which is what makes an interrupted turn
/// distinguishable after the fact despite every status mapping to
/// `finished`.
#[test]
fn every_cursor_stop_status_is_recorded_including_the_vetoed_one() {
    let records = fixture("cursor");
    let mut tab = Tab::default();
    for (event, payload) in records.iter().take(14) {
        tab.feed_via(cursor_event_to_reports, event, payload);
    }
    assert_eq!(tab.detail(), Some("aborted"));

    tab.feed_via(cursor_event_to_reports, &records[14].0, &records[14].1);
    assert_eq!(tab.lifecycle(), AgentLifecycle::Finished);
    assert_eq!(tab.detail(), Some("error"), "a vetoed report still merges");
    assert_eq!(tab.banners.len(), 2, "and it does not banner");
}

#[test]
fn the_cursor_session_start_records_model_and_version() {
    let mut tab = Tab::default();
    let (event, payload) = &fixture("cursor")[0];
    tab.feed_via(cursor_event_to_reports, event, payload);
    assert_eq!(tab.metadata("model"), Some("cursor-grok-4.6-high-fast"));
    assert_eq!(tab.metadata("cursor_version"), Some("2026.09.02-c22c1a3"));
}

// ---------------------------------------------------------------------
// The opencode probe, start to finish
// ---------------------------------------------------------------------

/// One forwarded line of `opencode.jsonl`: its 1-based line number in
/// the raw bus log, then the same four columns every other replay pins.
/// The line number is a column here because the forwarded events are
/// scattered through 862 records — a row has to be able to name which
/// one it is.
type BusRow = (
    usize,
    &'static str,
    usize,
    AgentLifecycle,
    Option<&'static str>,
);

/// Replay only the records the plugin forwards. The rest are fed too —
/// through [`the_bus_noise_the_plugin_filters_maps_to_nothing`] — but
/// they are cost, not policy, so they do not get a row here.
fn replay_forwarded(rows: &[BusRow]) -> Tab {
    let forwarded: Vec<(usize, String, Value)> = fixture("opencode")
        .into_iter()
        .enumerate()
        .filter(|(_, (event, _))| OPENCODE_HOOK_EVENTS.contains(&event.as_str()))
        .map(|(i, (event, payload))| (i + 1, event, payload))
        .collect();
    assert_eq!(forwarded.len(), rows.len(), "opencode.jsonl changed shape");

    let mut tab = Tab::default();
    for (
        (line, event, payload),
        (want_line, want_event, want_reports, want_lifecycle, want_owner),
    ) in forwarded.iter().zip(rows.iter().copied())
    {
        assert_eq!(*line, want_line, "opencode.jsonl: {event} moved");
        assert_eq!(event, want_event, "opencode.jsonl:{line}");
        assert_eq!(
            tab.feed_via(opencode_event_to_reports, event, payload),
            want_reports,
            "opencode.jsonl:{line} {event} report count"
        );
        assert_eq!(
            tab.lifecycle(),
            want_lifecycle,
            "opencode.jsonl:{line} {event} lifecycle"
        );
        assert_eq!(
            tab.owner_session(),
            want_owner,
            "opencode.jsonl:{line} {event} owner"
        );
    }
    tab
}

#[test]
fn the_opencode_probe_replays_to_the_pinned_lifecycle_sequence() {
    use AgentLifecycle::{Finished, Inactive, Waiting, Working};

    const SES: Option<&str> = Some(OPENCODE_SESSION);

    let tab = replay_forwarded(&[
        (51, "session.created", 1, Inactive, SES),
        (52, "chat.message", 1, Working, SES),
        (56, "session.status", 1, Working, SES),
        (110, "session.status", 1, Working, SES),
        // opencode's blocked signal, observed live.
        (140, "permission.asked", 1, Waiting, SES),
        (141, "permission.replied", 1, Working, SES),
        (147, "session.status", 1, Working, SES),
        (152, "session.status", 1, Working, SES),
        (160, "session.status", 1, Working, SES),
        // `session.status idle` is a level, not an edge: `session.idle`
        // is what ends a turn, so this maps to nothing.
        (161, "session.status", 0, Working, SES),
        (162, "session.idle", 1, Finished, SES),
        (166, "chat.message", 1, Working, SES),
        (170, "session.status", 1, Working, SES),
        (175, "session.status", 1, Working, SES),
        // Esc. `MessageAbortedError` arrives on the same channel as a
        // real failure and is the one value that must not read as one.
        (853, "session.error", 1, Finished, SES),
        (854, "session.status", 0, Finished, SES),
        // Still one report each — a vetoed report is accepted, it just
        // carries no patch and no banner.
        (855, "session.idle", 1, Finished, SES),
        (858, "session.status", 0, Finished, SES),
        (859, "session.idle", 1, Finished, SES),
    ]);

    // One banner per real turn, and none for the interrupt. opencode
    // fires `session.idle` three times in this run: once for the turn
    // that completed, and twice trailing the Esc — after
    // `session.error` had already ended the turn and cleared attention.
    // `session_idle`'s `["working", "waiting"]` guard vetoes those two,
    // the same way cursor's `stop` guard silences its repeats, so
    // §3.1's "an interrupt does not banner" rule holds here too.
    assert_eq!(
        tab.banners,
        vec![
            (
                Severity::Warn,
                "Needs permission: `external_directory`".to_string()
            ),
            (Severity::Info, "Turn complete".to_string()),
        ],
    );
    // No `dispose` in the capture: the plugin hook that would release
    // ownership was never observed, so the tab is still owned at the end
    // of the run (`opencode.rs`'s module doc).
    assert_eq!(tab.owner_session(), Some(OPENCODE_SESSION));
}

/// The whitelist is a cost control, not a correctness boundary: the 843
/// records the plugin never forwards map to nothing here either, so a
/// future opencode build that forwards more cannot silently start
/// driving a tab.
#[test]
fn the_bus_noise_the_plugin_filters_maps_to_nothing() {
    let mut filtered = 0;
    for (event, payload) in fixture("opencode") {
        if OPENCODE_HOOK_EVENTS.contains(&event.as_str()) {
            continue;
        }
        filtered += 1;
        assert!(
            opencode_event_to_reports(&event, &payload, TAB).is_empty(),
            "{event} is not forwarded but maps to a report"
        );
    }
    assert_eq!(filtered, 843, "opencode.jsonl changed shape");
}

/// A child session must not claim the tab from its parent: a claim is
/// unconditional, so a subagent would evict the session the user is
/// looking at. The probe has no child session, so this is synthetic —
/// built from the real `session.created` record by giving it a parent.
#[test]
fn a_child_session_created_never_claims_the_tab() {
    let (event, payload) = fixture("opencode")
        .into_iter()
        .find(|(event, _)| event == "session.created")
        .expect("opencode.jsonl has a session.created");
    assert_eq!(opencode_event_to_reports(&event, &payload, TAB).len(), 1);

    // Both spellings: the probe only ever nested `parentID` inside
    // `info`, but opencode's bus spreads session fields at the top level
    // on some events.
    let mut top_level = payload.clone();
    top_level["parentID"] = json!("ses_parent");
    let mut nested = payload.clone();
    nested["info"]["parentID"] = json!("ses_parent");

    for (spelling, child) in [("top-level", top_level), ("nested in info", nested)] {
        assert!(
            opencode_event_to_reports(&event, &child, TAB).is_empty(),
            "a session.created with a {spelling} parentID claimed the tab"
        );
    }
}

// ---------------------------------------------------------------------
// Cross-adapter isolation (plan 046 §3.1, §9)
// ---------------------------------------------------------------------

/// Every fixture replayed through every adapter that is not its own
/// (plan 046 §3.1's isolation table, made table-driven so a fixture or
/// adapter that lands later — C4's cursor and opencode — joins the
/// matrix by adding one row, not by writing a new test).
///
/// The bar is zero *claims*, not zero reports: `Claim` is the only
/// unconditional ownership action (`apply_report`), so a stray one
/// evicts the real owner and no release from that owner ever matches
/// again. A stray `working`/`waiting` report that never gets applied
/// against a live owner of a different source is inert — but every pair
/// below except the one documented exception produces *zero* reports
/// too, because the discriminators reject the payload outright.
#[test]
fn foreign_fixtures_never_claim_ownership_through_the_wrong_adapter() {
    const FIXTURES: &[(&str, &str)] = &[
        ("claude", "claude"),
        ("cursor", "cursor"),
        ("grok", "grok"),
        ("gx", "grok"),
        ("codex", "codex"),
        ("opencode", "opencode"),
    ];
    const ADAPTERS: &[(&str, Adapter)] = &[
        ("claude", claude_event_to_reports),
        ("grok", grok_event_to_reports),
        ("codex", codex_event_to_reports),
        ("cursor", cursor_event_to_reports),
        ("opencode", opencode_event_to_reports),
    ];

    for (fixture_name, native) in FIXTURES {
        let records = fixture(fixture_name);
        assert!(!records.is_empty(), "{fixture_name}.jsonl is empty");

        for (adapter_name, adapter) in ADAPTERS {
            if adapter_name == native {
                continue;
            }

            let mut claims = 0;
            let mut reports = 0;
            for (event, payload) in &records {
                let out = adapter(event, payload, TAB);
                reports += out.len();
                claims += out
                    .iter()
                    .filter(|r| r.ownership_action == OwnershipAction::Claim)
                    .count();
            }

            // codex's and Claude's own payloads are both snake_case-only
            // and share every field name `SessionStart`/`SessionEnd`
            // use; `turn_id` separates every *other* event the two
            // agents have in common, but neither of these two events
            // carries it (codex.rs's module doc). Nothing installs
            // codex's command into Claude's `settings.json` or Claude's
            // into codex's `hooks.json` (plan §3.1 names only grok and
            // cursor as agents that execute Claude's hook format), so
            // this pair never actually runs the other's payload through
            // this function outside this test — the isolation here
            // rests entirely on that installation fact, not on payload
            // content. Documented honestly rather than "fixed" with a
            // discriminator that cannot exist.
            let known_gap = (*fixture_name == "codex" && *adapter_name == "claude")
                || (*fixture_name == "claude" && *adapter_name == "codex");
            if known_gap {
                let want_claims = if *fixture_name == "codex" { 1 } else { 2 };
                assert_eq!(
                    claims, want_claims,
                    "{fixture_name}.jsonl through {adapter_name}: the documented \
                     SessionStart gap changed shape"
                );
                continue;
            }

            assert_eq!(
                claims, 0,
                "{fixture_name}.jsonl through {adapter_name} produced {claims} claim(s)"
            );
            assert_eq!(
                reports, 0,
                "{fixture_name}.jsonl through {adapter_name} produced {reports} report(s) \
                 though none claimed ownership"
            );
        }
    }
}

/// How many of `agent`'s captured records map through `adapter` once
/// every foreign discriminator is stripped from the payload — the
/// measure that keeps the isolation test above from passing merely
/// because two vocabularies do not overlap.
fn mapped_without_discriminators(adapter: Adapter, agent: &str) -> usize {
    fixture(agent)
        .into_iter()
        .filter(|(event, payload)| {
            let mut stripped = payload.clone();
            if let Some(object) = stripped.as_object_mut() {
                for key in ["hookEventName", "conversation_id", "cursor_version"] {
                    object.remove(key);
                }
            }
            !adapter(event, &stripped, TAB).is_empty()
        })
        .count()
}

/// The mirror of [`mapped_without_discriminators`], for the two gates
/// that *require* a key rather than rejecting one: how many of `agent`'s
/// captured records map through `adapter` once `key` is grafted onto
/// every payload.
fn mapped_with_grafted(adapter: Adapter, agent: &str, key: &str) -> usize {
    fixture(agent)
        .into_iter()
        .filter(|(event, payload)| {
            let mut grafted = payload.clone();
            if let Some(object) = grafted.as_object_mut() {
                object.insert(key.to_string(), json!("x"));
            }
            !adapter(event, &grafted, TAB).is_empty()
        })
        .count()
}

/// …and the fence is not vacuous. Both agents fire events whose names
/// normalize onto Claude's own vocabulary — grok's `SessionStart`,
/// cursor's `sessionStart`/`stop`/`preToolUse` — so with the foreign
/// discriminators removed these very payloads *do* map. The rejection
/// is what makes the test above hold, not a lucky absence of overlap.
#[test]
fn the_same_payloads_map_once_their_discriminators_are_removed() {
    for agent in ["grok", "cursor"] {
        assert!(
            mapped_without_discriminators(claude_event_to_reports, agent) > 0,
            "{agent}.jsonl no longer overlaps Claude's vocabulary; the isolation \
             test would pass for the wrong reason"
        );
    }
}

/// grok's fence runs the other direction from Claude's: it requires the
/// camelCase `hookEventName` twin rather than rejecting it. Proved not
/// vacuous the same way — grafting that one key onto a fixture that
/// never carries it (Claude's, event names it already shares with
/// grok's vocabulary) makes the very same payloads map once the gate is
/// satisfied.
#[test]
fn the_grok_gate_is_not_vacuous() {
    assert!(
        mapped_with_grafted(grok_event_to_reports, "claude", "hookEventName") > 0,
        "claude.jsonl no longer overlaps grok's vocabulary once hookEventName \
         is present; the isolation test would pass for the wrong reason"
    );
}

/// …and codex's fence, which mirrors Claude's own list, is not vacuous
/// either: stripping grok's `hookEventName` twin (grok/gx carry none of
/// cursor's two id fields) lets the same event names — `SessionStart`,
/// `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `SessionEnd`
/// — map through the codex adapter.
#[test]
fn the_codex_gate_is_not_vacuous() {
    for agent in ["grok", "gx", "cursor"] {
        assert!(
            mapped_without_discriminators(codex_event_to_reports, agent) > 0,
            "{agent}.jsonl no longer overlaps codex's vocabulary; the isolation \
             test would pass for the wrong reason"
        );
    }
}

/// Cursor's fence runs grok's direction, not Claude's: it *requires*
/// `cursor_version`, because the two keys Claude and codex reject on are
/// cursor's own and the event vocabulary alone would let a Claude
/// `SessionStart` claim a cursor tab. Proved not vacuous the same way
/// grok's is — grafting that one key onto the fixtures that never carry
/// it makes the very same payloads map.
#[test]
fn the_cursor_gate_is_not_vacuous() {
    for agent in ["claude", "codex", "grok", "gx"] {
        assert!(
            mapped_with_grafted(cursor_event_to_reports, agent, "cursor_version") > 0,
            "{agent}.jsonl no longer overlaps cursor's vocabulary once cursor_version \
             is present; the isolation test would pass for the wrong reason"
        );
    }
}

/// opencode has no content gate at all, and needs none: its vocabulary
/// is its own. That claim is the whole isolation argument for this
/// adapter, so it is asserted directly against the four other event
/// lists rather than left to the fixtures to demonstrate — a future
/// opencode event named `stop` would be caught here, not in production.
#[test]
fn opencodes_vocabulary_is_disjoint_from_every_other_agents() {
    use roost_agent::claude::CLAUDE_HOOK_EVENTS;
    use roost_agent::codex::CODEX_HOOK_EVENTS;
    use roost_agent::cursor::CURSOR_HOOK_EVENTS;
    use roost_agent::grok::GROK_HOOK_EVENTS;

    // Permissive enough to satisfy every other adapter's gate at once,
    // so only the event name can be what rejects it.
    let payload = json!({
        "session_id": "s-1",
        "sessionID": "ses_1",
        "source": "startup",
        "hookEventName": "x",
        "conversation_id": "c-1",
        "cursor_version": "v",
    });

    let others: &[(&str, Adapter, &[&str])] = &[
        ("claude", claude_event_to_reports, &CLAUDE_HOOK_EVENTS),
        ("grok", grok_event_to_reports, &GROK_HOOK_EVENTS),
        ("codex", codex_event_to_reports, &CODEX_HOOK_EVENTS),
        ("cursor", cursor_event_to_reports, &CURSOR_HOOK_EVENTS),
    ];

    for (name, adapter, events) in others {
        for event in OPENCODE_HOOK_EVENTS {
            assert!(
                adapter(event, &payload, TAB).is_empty(),
                "{name} maps opencode's {event}"
            );
        }
        for event in *events {
            assert!(
                opencode_event_to_reports(event, &payload, TAB).is_empty(),
                "opencode maps {name}'s {event}"
            );
        }
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
