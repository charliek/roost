//! Agent state model — the four independent axes of plan 002 §3.1 plus
//! the pure state machine that projects them onto the legacy
//! [`TabState`] wire field.
//!
//! Everything here is I/O-free and takes its input as parameters, so
//! one definition serves every consumer: the Rust workspace calls these
//! functions from its mutation path, the Swift port
//! (`mac/Sources/Roost/AgentState.swift`) mirrors them, and the shared
//! corpus under `tests/agent-state-fixtures/` pins the two together.
//!
//! Three axes persist ([`ShellState`], [`AgentLifecycle`],
//! [`Ownership`]). Attention is not state but an effect: [`apply_report`]
//! returns it as an [`AttentionEffect`] so the caller can fire or clear a
//! notification without re-deriving anything.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::messages::TabState;

// ============================================================================
// Axes
// ============================================================================

/// Shell activity, written by OSC 133 marks. `Unknown` is the state of
/// a shell that has not emitted a mark yet (no shell integration, or
/// nothing has run).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShellState {
    #[default]
    Unknown,
    AtPrompt,
    ForegroundProcess,
}

/// Agent turn state, written by adapters. Independent of [`ShellState`]
/// — an agent can be `Working` while the shell sits at a prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycle {
    #[default]
    Inactive,
    Working,
    Waiting,
    Finished,
    Failed,
}

/// Notification severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    Warn,
    Error,
}

/// What a report intends to do to ownership. Required on every report —
/// there is no sensible default, since "take the tab" and "I already
/// own the tab" have opposite failure modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipAction {
    Claim,
    Preserve,
    Release,
}

/// What a report intends to do to the tab's attention (notification)
/// state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttentionOp {
    Set,
    Clear,
    #[default]
    Preserve,
}

/// Who owns the tab. Identity is the **pair** `(source, session_id)`:
/// two agents can collide on an opaque session id, so neither half is
/// sufficient alone.
///
/// `source` is an open string (AD-8) — adding a second agent must not
/// require touching this enum-free type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Ownership {
    pub source: String,
    #[serde(default)]
    pub session_id: String,
    /// Server receipt time of the most recent accepted report. Stamped
    /// by [`apply_report`]'s `now` parameter, never by the caller.
    #[serde(default)]
    pub last_event_at: i64,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTabState {
    #[serde(default)]
    pub shell: ShellState,
    #[serde(default)]
    pub lifecycle: AgentLifecycle,
    #[serde(default)]
    pub ownership: Option<Ownership>,
}

// ============================================================================
// `tab.agent_report` params
// ============================================================================

/// `tab.agent_report` request — the single op every agent adapter
/// writes through (plan §3.6).
///
/// Patch semantics are explicit rather than inferred: `ownership_action`
/// is required, an omitted `lifecycle` means "unchanged", and
/// `attention` defaults to `preserve`. That keeps adapters pure — they
/// never need to read current state to describe an event.
///
/// `metadata` is the open extension channel. `deny_unknown_fields` (the
/// server-side convention, see `messages.rs`) means a new *named* field
/// is a request-schema change rather than an additive one — an older
/// server rejects the whole report — so anything an adapter can express
/// as data goes in the map instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TabAgentReportParams {
    #[serde(with = "crate::messages::string_int64")]
    #[schemars(with = "String")]
    pub tab_id: i64,
    pub source: String,
    /// Empty for sources that have no session concept (e.g. `manual`).
    #[serde(default)]
    pub session_id: String,
    pub ownership_action: OwnershipAction,
    /// `None` (omitted on the wire) means "leave lifecycle unchanged".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<AgentLifecycle>,
    /// Guard on the tab's **current** lifecycle. When present, the
    /// `lifecycle` patch and any `attention: Set` apply only if the
    /// current lifecycle is in this set; omitted means unconditional.
    ///
    /// A transition that did not happen is not news: agents nag on a
    /// timer (Claude's `idle_prompt`, cursor's repeated `stop`) and an
    /// unguarded nag re-banners a turn that already ended. `detail` and
    /// `metadata` still merge, and `attention: Clear` still applies. A
    /// `Release` still forces `Inactive` — the guard cannot keep a
    /// departed agent's lifecycle alive — but its `attention: Set` is
    /// gated like any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_if: Option<Vec<AgentLifecycle>>,
    #[serde(default)]
    pub attention: AttentionOp,
    #[serde(default)]
    pub severity: Severity,
    /// Required when `attention == Set`, ignored otherwise. See
    /// [`validate_report`].
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Free-form reason for the report (`permission_prompt`,
    /// `2 background tasks`, an error name…). Recorded on the owner
    /// when non-empty.
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl TabAgentReportParams {
    /// A report from a source with no session concept — the legacy
    /// `tab.set_state` / `tab.set_hook_active` adapters, and any caller
    /// that only needs the ownership + lifecycle axes.
    ///
    /// `ownership_action` stays a required argument: it deliberately has
    /// no `Default` (see [`OwnershipAction`]), so there is no
    /// `..Default::default()` form of this that isn't a guess.
    pub fn sessionless(
        tab_id: i64,
        source: impl Into<String>,
        ownership_action: OwnershipAction,
        lifecycle: Option<AgentLifecycle>,
    ) -> Self {
        Self {
            tab_id,
            source: source.into(),
            session_id: String::new(),
            ownership_action,
            lifecycle,
            lifecycle_if: None,
            attention: AttentionOp::Preserve,
            severity: Severity::Info,
            title: String::new(),
            body: String::new(),
            detail: String::new(),
            metadata: BTreeMap::new(),
        }
    }
}

/// Why a report is malformed. Returned by [`validate_report`] so the op
/// dispatcher can reject with `invalid-param` before mutating anything.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReportError {
    #[error("source must not be empty")]
    EmptySource,
    #[error("attention=set requires a non-empty title")]
    MissingTitle,
    #[error("attention=set requires a non-empty body")]
    MissingBody,
}

/// Shape validation, separate from [`apply_report`] so the pure state
/// machine stays total: the dispatcher validates, then applies.
pub fn validate_report(report: &TabAgentReportParams) -> Result<(), ReportError> {
    if report.source.is_empty() {
        return Err(ReportError::EmptySource);
    }
    if report.attention == AttentionOp::Set {
        if report.title.is_empty() {
            return Err(ReportError::MissingTitle);
        }
        if report.body.is_empty() {
            return Err(ReportError::MissingBody);
        }
    }
    Ok(())
}

// ============================================================================
// Derivation
// ============================================================================

/// Ownership source for a hand-driven `roostctl tab set-state`.
///
/// Lives here, beside the state machine, because it is contract rather
/// than implementation: the producer (the workspace's `tab.set_state`
/// adapter) and every consumer that has to tell "an agent owns this"
/// from "a human poked it" must agree on the exact string.
pub const SOURCE_MANUAL: &str = "manual";
/// Ownership source for the deprecated `tab.set_hook_active` alias.
/// See [`SOURCE_MANUAL`] for why it lives here.
pub const SOURCE_LEGACY: &str = "legacy";

/// Ownership is "live" iff it is present with a non-empty source.
///
/// There is deliberately **no timestamp or TTL heuristic** (AD-3):
/// Claude fires no periodic hook, so a long tool call would look stale
/// and get released mid-turn. Ownership is cleared only by the explicit
/// rules in [`apply_report`]; [`apply_shell_mark`] at a prompt drops the
/// lifecycle but keeps the owner.
pub fn is_live(state: &AgentTabState) -> bool {
    matches!(&state.ownership, Some(o) if !o.source.is_empty())
}

/// Whether the agent axis wins derivation: a live owner that is doing
/// something. The single expression of plan §3.2's precedence rule —
/// [`effective_lifecycle`] and [`suppress_raw_osc`] are both this
/// predicate, so a dead agent can never keep driving one of them while
/// having released the other.
fn agent_drives(state: &AgentTabState) -> bool {
    is_live(state) && state.lifecycle != AgentLifecycle::Inactive
}

/// The lifecycle a tab actually presents (plan §3.2): the agent's own
/// when it drives, otherwise the shell axis lifted onto the same enum.
///
/// This — not [`effective`] — is what the UIs render, because it keeps
/// `Failed` distinct from `Waiting`; `effective` collapses the two for
/// the legacy wire field. Both UIs rank, colour, and icon off this, so
/// the sidebar stripe cannot disagree with the tab's own dot.
pub fn effective_lifecycle(state: &AgentTabState) -> AgentLifecycle {
    if agent_drives(state) {
        return state.lifecycle;
    }
    match state.shell {
        ShellState::ForegroundProcess => AgentLifecycle::Working,
        ShellState::Unknown | ShellState::AtPrompt => AgentLifecycle::Inactive,
    }
}

/// Project the axes onto the legacy `tab.state` field (plan §3.2) — the
/// lossy half of [`effective_lifecycle`].
///
/// `AgentLifecycle::Failed` maps to [`TabState::NeedsInput`] on
/// purpose. `TabState` stays a **closed four-value enum** — the Swift
/// decoders (`IPCTabState`, `Workspace.TabState`) have no fallback case,
/// so a fifth value throws on the Mac client, and `docs/reference/ipc.md`
/// classifies a new enum value as a breaking protocol change. True
/// failure is observable on the agent lifecycle axis instead. Do not
/// "fix" this by adding a `Failed` variant to `TabState`.
pub fn effective(state: &AgentTabState) -> TabState {
    match effective_lifecycle(state) {
        AgentLifecycle::Inactive => TabState::None,
        AgentLifecycle::Working => TabState::Running,
        AgentLifecycle::Waiting | AgentLifecycle::Failed => TabState::NeedsInput,
        AgentLifecycle::Finished => TabState::Idle,
    }
}

/// Attention ordering for the sidebar stripe, the overview sort, and
/// the agent switcher (plan §3.2).
///
/// Operates on [`AgentLifecycle`] — which has `Failed` — rather than on
/// the projected [`TabState`], where `failed` and `waiting` collapse
/// into one value.
pub fn rank(lifecycle: AgentLifecycle) -> u8 {
    match lifecycle {
        AgentLifecycle::Failed => 4,
        AgentLifecycle::Waiting => 3,
        AgentLifecycle::Working => 2,
        AgentLifecycle::Finished => 1,
        AgentLifecycle::Inactive => 0,
    }
}

/// Whether raw OSC 9 / 99 / 777 notifications should be dropped because
/// an agent is actively driving the tab (plan §3.4). Explicit
/// `notification.create` is never suppressed — only raw OSC.
pub fn suppress_raw_osc(state: &AgentTabState) -> bool {
    agent_drives(state)
}

/// Apply an OSC 133 shell mark. Returns `None` for a body the spec
/// doesn't define, meaning "no change" — matching the prior
/// `command_mark_state` semantics in both UIs.
///
/// `C` (command start) writes only the shell axis: a foreground process
/// exists, but if an agent owns the tab its lifecycle still wins.
///
/// `A`/`B` (prompt) and `D` (command end) additionally drop the
/// lifecycle to `Inactive` while **retaining ownership as a label** —
/// the failsafe against a killed agent muting a tab forever (plan §3.4).
/// The shell only reaches a prompt once the foreground command exited,
/// so an agent that owned the tab is necessarily gone. Derivation then
/// falls through to the shell axis and [`suppress_raw_osc`] re-opens raw
/// OSC, so a dead agent degrades cosmetically instead of silently
/// swallowing notifications.
pub fn apply_shell_mark(current: &AgentTabState, body: &str) -> Option<AgentTabState> {
    let (shell, lifecycle) = match body.chars().next()? {
        'C' => (ShellState::ForegroundProcess, current.lifecycle),
        'A' | 'B' | 'D' => (ShellState::AtPrompt, AgentLifecycle::Inactive),
        _ => return None,
    };
    Some(AgentTabState {
        shell,
        lifecycle,
        ownership: current.ownership.clone(),
    })
}

// ============================================================================
// Report application
// ============================================================================

/// What [`apply_report`] wants the caller to do about attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionEffect {
    Set {
        title: String,
        body: String,
        severity: Severity,
    },
    Clear,
    Unchanged,
}

/// Why [`apply_report`] dropped a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// A `preserve` or `release` from anyone but the owner, including
    /// any report to a tab nobody owns.
    NotOwner,
    /// A claim from an agent the owner is running; see
    /// [`is_nested_claim`].
    NestedClaim,
}

/// One report a server dropped, as `Tab.dropped_reports` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DroppedReport {
    pub source: String,
    #[serde(default)]
    pub session_id: String,
    pub ownership_action: OwnershipAction,
    pub reason: DropReason,
    /// Server receipt time.
    pub at: i64,
}

/// How many reports a tab's server has dropped, and the latest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DroppedReports {
    pub count: u64,
    pub last: DroppedReport,
}

/// Result of applying a report: the new state plus everything the
/// caller needs to emit events without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub state: AgentTabState,
    /// Set when the report was dropped; the state is then returned
    /// unchanged.
    pub dropped: Option<DropReason>,
    /// Whether the owner's **identity or presence** changed. A refreshed
    /// `last_event_at` (or merged metadata) is not an ownership change —
    /// otherwise every accepted report would look like one.
    pub ownership_changed: bool,
    pub lifecycle_changed: bool,
    pub attention: AttentionEffect,
}

impl ApplyOutcome {
    pub fn accepted(&self) -> bool {
        self.dropped.is_none()
    }
}

/// Apply a report to the current state, enforcing session scoping
/// (plan §3.3 + §3.6). Pure: the caller owns the mutation and the event
/// emission, this decides what they should be.
///
/// * Identity is the pair `(source, session_id)`; a report that does
///   not match the current owner is dropped.
/// * `Claim` takes ownership, replacing any existing owner — the sole
///   supersede path — unless it is a [nested claim](is_nested_claim),
///   which is dropped like a mismatch.
/// * `Release` requires a match; it clears ownership and forces
///   lifecycle `Inactive`.
/// * `lifecycle_if` gates the lifecycle patch and any `attention: Set`
///   on the *current* lifecycle. A vetoed report is still `accepted` —
///   it matched the owner — and its `detail`/`metadata` still merge. A
///   `Release`'s forced `Inactive` is exempt from the guard; its
///   `attention: Set` is not.
/// * `last_event_at` is stamped from `now` (server receipt time).
pub fn apply_report(
    current: &AgentTabState,
    report: &TabAgentReportParams,
    now: i64,
) -> ApplyOutcome {
    let dropped = match report.ownership_action {
        OwnershipAction::Claim => {
            is_nested_claim(current, report).then_some(DropReason::NestedClaim)
        }
        OwnershipAction::Preserve | OwnershipAction::Release => {
            (!owner_matches(current, report)).then_some(DropReason::NotOwner)
        }
    };
    if dropped.is_some() {
        return ApplyOutcome {
            state: current.clone(),
            dropped,
            ownership_changed: false,
            lifecycle_changed: false,
            attention: AttentionEffect::Unchanged,
        };
    }

    let ownership = match report.ownership_action {
        OwnershipAction::Claim => Some(Ownership {
            source: report.source.clone(),
            session_id: report.session_id.clone(),
            last_event_at: now,
            detail: report.detail.clone(),
            metadata: report.metadata.clone(),
        }),
        OwnershipAction::Preserve => {
            let mut kept = current.ownership.clone();
            if let Some(owner) = kept.as_mut() {
                owner.last_event_at = now;
                // Empty fields mean "this event says nothing about it",
                // not "clear it" — metadata accumulates across a
                // session (model at SessionStart, cron counts at Stop)
                // and v1 has no delete channel.
                if !report.detail.is_empty() {
                    owner.detail.clone_from(&report.detail);
                }
                for (key, value) in &report.metadata {
                    owner.metadata.insert(key.clone(), value.clone());
                }
            }
            kept
        }
        OwnershipAction::Release => None,
    };

    let vetoed =
        matches!(&report.lifecycle_if, Some(allowed) if !allowed.contains(&current.lifecycle));

    let lifecycle = match report.ownership_action {
        OwnershipAction::Release => AgentLifecycle::Inactive,
        OwnershipAction::Claim | OwnershipAction::Preserve => {
            if vetoed {
                current.lifecycle
            } else {
                report.lifecycle.unwrap_or(current.lifecycle)
            }
        }
    };

    let state = AgentTabState {
        shell: current.shell,
        lifecycle,
        ownership,
    };

    let attention = match report.attention {
        AttentionOp::Set if vetoed => AttentionEffect::Unchanged,
        AttentionOp::Set => AttentionEffect::Set {
            title: report.title.clone(),
            body: report.body.clone(),
            severity: report.severity,
        },
        AttentionOp::Clear => AttentionEffect::Clear,
        AttentionOp::Preserve => AttentionEffect::Unchanged,
    };

    ApplyOutcome {
        dropped: None,
        ownership_changed: identity(&state.ownership) != identity(&current.ownership),
        lifecycle_changed: state.lifecycle != current.lifecycle,
        attention,
        state,
    }
}

/// Whether a claim comes from an agent the current owner is running —
/// a `codex exec` review, a headless `gx -p` — rather than one taking
/// the tab over.
///
/// Anything an agent runs from a tool call inherits the tab's
/// `ROOST_TAB_ID`. Honored, the inner agent's `SessionStart` evicts the
/// owner and its `SessionEnd` then releases the tab, so the outer agent,
/// still mid-turn, is left with no owner and every report it sends after
/// that is dropped. The shell axis tells the two cases apart without a
/// timestamp: another agent can only replace the owner once the owner's
/// foreground command has exited, and the prompt mark that follows
/// drops the lifecycle to `Inactive`. A live owner still `Working` or
/// `Waiting` under a foreground process has not exited, so a different
/// agent claiming now is running inside it.
///
/// The same source under a new session id still supersedes: that is
/// the agent switching sessions itself (Claude's `/clear`). `manual`
/// and `legacy` are someone taking the wheel, never nested. With no
/// shell integration the shell axis stays `Unknown`, so claims
/// supersede as they always did — no prompt mark would ever free a tab
/// whose owner died.
///
/// The Swift port does not carry this rule; it is frozen pending its
/// retirement.
fn is_nested_claim(current: &AgentTabState, report: &TabAgentReportParams) -> bool {
    let Some(owner) = &current.ownership else {
        return false;
    };
    current.shell == ShellState::ForegroundProcess
        && matches!(
            current.lifecycle,
            AgentLifecycle::Working | AgentLifecycle::Waiting
        )
        && owner.source != report.source
        && is_agent_source(&owner.source)
        && is_agent_source(&report.source)
}

fn is_agent_source(source: &str) -> bool {
    !source.is_empty() && source != SOURCE_MANUAL && source != SOURCE_LEGACY
}

fn owner_matches(current: &AgentTabState, report: &TabAgentReportParams) -> bool {
    match &current.ownership {
        Some(owner) => owner.source == report.source && owner.session_id == report.session_id,
        None => false,
    }
}

fn identity(ownership: &Option<Ownership>) -> Option<(&str, &str)> {
    ownership
        .as_ref()
        .map(|o| (o.source.as_str(), o.session_id.as_str()))
}

// ============================================================================
// Unit tests — shape + the helpers the shared fixtures don't cover
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(source: &str, session: &str) -> Option<Ownership> {
        Some(Ownership {
            source: source.into(),
            session_id: session.into(),
            last_event_at: 1_700_000_000,
            detail: String::new(),
            metadata: BTreeMap::new(),
        })
    }

    fn report(source: &str, session: &str, action: OwnershipAction) -> TabAgentReportParams {
        TabAgentReportParams {
            session_id: session.into(),
            ..TabAgentReportParams::sessionless(3, source, action, None)
        }
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ShellState::ForegroundProcess).unwrap(),
            "\"foreground_process\""
        );
        assert_eq!(
            serde_json::to_string(&AgentLifecycle::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(serde_json::to_string(&Severity::Warn).unwrap(), "\"warn\"");
        assert_eq!(
            serde_json::to_string(&OwnershipAction::Claim).unwrap(),
            "\"claim\""
        );
        assert_eq!(
            serde_json::to_string(&AttentionOp::Preserve).unwrap(),
            "\"preserve\""
        );
    }

    #[test]
    fn report_params_defaults_and_round_trip() {
        let minimal: TabAgentReportParams = serde_json::from_str(
            r#"{"tab_id":"3","source":"claude","ownership_action":"preserve"}"#,
        )
        .unwrap();
        assert_eq!(minimal.tab_id, 3);
        assert_eq!(minimal.session_id, "");
        assert_eq!(minimal.lifecycle, None);
        assert_eq!(minimal.lifecycle_if, None);
        assert_eq!(minimal.attention, AttentionOp::Preserve);
        assert_eq!(minimal.severity, Severity::Info);
        assert!(minimal.metadata.is_empty());

        let json = serde_json::to_string(&minimal).unwrap();
        assert!(json.contains("\"tab_id\":\"3\""), "got: {json}");
        assert!(!json.contains("lifecycle"), "omitted when None: {json}");
        let back: TabAgentReportParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back, minimal);
    }

    #[test]
    fn lifecycle_if_round_trips_and_is_omitted_when_absent() {
        let guarded: TabAgentReportParams = serde_json::from_str(
            r#"{"tab_id":"3","source":"claude","ownership_action":"preserve",
                "lifecycle":"waiting","lifecycle_if":["working","waiting"]}"#,
        )
        .unwrap();
        assert_eq!(
            guarded.lifecycle_if,
            Some(vec![AgentLifecycle::Working, AgentLifecycle::Waiting])
        );

        let json = serde_json::to_string(&guarded).unwrap();
        assert!(
            json.contains(r#""lifecycle_if":["working","waiting"]"#),
            "got: {json}"
        );
        assert_eq!(
            serde_json::from_str::<TabAgentReportParams>(&json).unwrap(),
            guarded
        );

        let unguarded = report("claude", "s1", OwnershipAction::Preserve);
        assert_eq!(unguarded.lifecycle_if, None);
        let json = serde_json::to_string(&unguarded).unwrap();
        assert!(!json.contains("lifecycle_if"), "got: {json}");
    }

    #[test]
    fn report_params_reject_unknown_field() {
        let bad = r#"{"tab_id":"3","source":"claude","ownership_action":"claim","extra":1}"#;
        assert!(serde_json::from_str::<TabAgentReportParams>(bad).is_err());
        // `last_event_at` is server-stamped: a caller cannot smuggle one in.
        let stamped =
            r#"{"tab_id":"3","source":"claude","ownership_action":"claim","last_event_at":5}"#;
        assert!(serde_json::from_str::<TabAgentReportParams>(stamped).is_err());
    }

    #[test]
    fn validate_requires_source_and_attention_payload() {
        let mut r = report("", "", OwnershipAction::Claim);
        assert_eq!(validate_report(&r), Err(ReportError::EmptySource));

        r.source = "claude".into();
        assert_eq!(validate_report(&r), Ok(()));

        r.attention = AttentionOp::Set;
        assert_eq!(validate_report(&r), Err(ReportError::MissingTitle));
        r.title = "Claude Code".into();
        assert_eq!(validate_report(&r), Err(ReportError::MissingBody));
        r.body = "Turn complete".into();
        assert_eq!(validate_report(&r), Ok(()));

        // Ignored when attention is not `set`.
        r.attention = AttentionOp::Clear;
        r.title = String::new();
        r.body = String::new();
        assert_eq!(validate_report(&r), Ok(()));
    }

    #[test]
    fn prompt_mark_drops_lifecycle_but_keeps_the_owner() {
        let current = AgentTabState {
            shell: ShellState::ForegroundProcess,
            lifecycle: AgentLifecycle::Working,
            ownership: owner("claude", "s1"),
        };
        assert!(suppress_raw_osc(&current));

        let after = apply_shell_mark(&current, "D;0").expect("D is a defined mark");
        assert_eq!(after.shell, ShellState::AtPrompt);
        assert_eq!(after.lifecycle, AgentLifecycle::Inactive);
        assert_eq!(after.ownership, current.ownership);
        assert!(is_live(&after), "ownership survives as a label");
        assert!(!suppress_raw_osc(&after), "raw OSC re-opens");
        assert_eq!(effective(&after), TabState::None);
    }

    fn mid_turn(lifecycle: AgentLifecycle, source: &str, session: &str) -> AgentTabState {
        AgentTabState {
            shell: ShellState::ForegroundProcess,
            lifecycle,
            ownership: owner(source, session),
        }
    }

    #[test]
    fn a_nested_agents_whole_session_leaves_the_owner_in_place() {
        let claude = mid_turn(AgentLifecycle::Working, "claude", "outer");

        let claim = TabAgentReportParams {
            lifecycle: Some(AgentLifecycle::Inactive),
            ..report("codex", "inner", OwnershipAction::Claim)
        };
        let stop = TabAgentReportParams {
            lifecycle: Some(AgentLifecycle::Finished),
            attention: AttentionOp::Set,
            title: "Codex".into(),
            body: "Turn complete".into(),
            ..report("codex", "inner", OwnershipAction::Preserve)
        };
        let end = report("codex", "inner", OwnershipAction::Release);
        for (nested, reason) in [
            (&claim, DropReason::NestedClaim),
            (&stop, DropReason::NotOwner),
            (&end, DropReason::NotOwner),
        ] {
            let out = apply_report(&claude, nested, 1_700_000_100);
            assert_eq!(out.dropped, Some(reason), "{:?}", nested.ownership_action);
            assert_eq!(out.state, claude);
            assert_eq!(out.attention, AttentionEffect::Unchanged);
        }

        let next_tool = TabAgentReportParams {
            lifecycle: Some(AgentLifecycle::Working),
            ..report("claude", "outer", OwnershipAction::Preserve)
        };
        let out = apply_report(&claude, &next_tool, 1_700_000_200);
        assert!(out.accepted(), "the outer agent still owns the tab");
        assert_eq!(out.state.ownership.unwrap().last_event_at, 1_700_000_200);
    }

    #[test]
    fn a_claim_while_the_owner_waits_is_nested_too() {
        // An approved tool runs before the owner leaves `Waiting`.
        let claude = mid_turn(AgentLifecycle::Waiting, "claude", "outer");
        let out = apply_report(
            &claude,
            &report("codex", "inner", OwnershipAction::Claim),
            1_700_000_100,
        );
        assert_eq!(out.dropped, Some(DropReason::NestedClaim));
        assert_eq!(out.state, claude);
    }

    #[test]
    fn a_claim_that_is_not_nested_still_supersedes() {
        let cases = [
            (
                "the owner's turn is over",
                mid_turn(AgentLifecycle::Finished, "claude", "outer"),
                report("codex", "next", OwnershipAction::Claim),
            ),
            (
                "the owner's turn failed",
                mid_turn(AgentLifecycle::Failed, "claude", "outer"),
                report("codex", "next", OwnershipAction::Claim),
            ),
            (
                "the owner is a label left by the prompt mark",
                AgentTabState {
                    shell: ShellState::AtPrompt,
                    lifecycle: AgentLifecycle::Inactive,
                    ownership: owner("claude", "outer"),
                },
                report("codex", "next", OwnershipAction::Claim),
            ),
            (
                "no shell integration ever resets a dead owner",
                AgentTabState {
                    shell: ShellState::Unknown,
                    lifecycle: AgentLifecycle::Working,
                    ownership: owner("claude", "outer"),
                },
                report("codex", "next", OwnershipAction::Claim),
            ),
            (
                "the same agent switches sessions mid-turn",
                mid_turn(AgentLifecycle::Waiting, "claude", "outer"),
                report("claude", "cleared", OwnershipAction::Claim),
            ),
            (
                "a human takes the wheel",
                mid_turn(AgentLifecycle::Working, "claude", "outer"),
                report(SOURCE_MANUAL, "", OwnershipAction::Claim),
            ),
            (
                "the owner is a human, not an agent",
                mid_turn(AgentLifecycle::Working, SOURCE_MANUAL, ""),
                report("claude", "outer", OwnershipAction::Claim),
            ),
            (
                "nobody owns the tab",
                AgentTabState {
                    shell: ShellState::ForegroundProcess,
                    lifecycle: AgentLifecycle::Working,
                    ownership: None,
                },
                report("codex", "next", OwnershipAction::Claim),
            ),
        ];
        for (why, current, claim) in cases {
            let out = apply_report(&current, &claim, 1_700_000_100);
            assert!(out.accepted(), "{why}");
            let new_owner = out.state.ownership.expect(why);
            assert_eq!(
                (new_owner.source.as_str(), new_owner.session_id.as_str()),
                (claim.source.as_str(), claim.session_id.as_str()),
                "{why}"
            );
        }
    }

    #[test]
    fn dropped_reports_serialize_in_the_documented_shape() {
        let dropped = DroppedReports {
            count: 9,
            last: DroppedReport {
                source: "codex".into(),
                session_id: "inner".into(),
                ownership_action: OwnershipAction::Release,
                reason: DropReason::NotOwner,
                at: 1_791_648_366,
            },
        };
        assert_eq!(
            serde_json::to_value(&dropped).unwrap(),
            serde_json::json!({"count": 9, "last": {
                "source": "codex", "session_id": "inner", "ownership_action": "release",
                "reason": "not_owner", "at": 1_791_648_366,
            }})
        );
        assert_eq!(
            serde_json::to_string(&DropReason::NestedClaim).unwrap(),
            "\"nested_claim\""
        );
    }

    #[test]
    fn undefined_shell_marks_are_no_change() {
        let current = AgentTabState::default();
        assert!(apply_shell_mark(&current, "").is_none());
        assert!(apply_shell_mark(&current, "Z").is_none());
    }

    #[test]
    fn attention_effect_round_trips() {
        let set = AttentionEffect::Set {
            title: "Claude Code".into(),
            body: "Needs your permission".into(),
            severity: Severity::Warn,
        };
        let json = serde_json::to_string(&set).unwrap();
        assert!(json.contains("\"kind\":\"set\""), "got: {json}");
        assert_eq!(serde_json::from_str::<AttentionEffect>(&json).unwrap(), set);
        assert_eq!(
            serde_json::to_string(&AttentionEffect::Unchanged).unwrap(),
            "{\"kind\":\"unchanged\"}"
        );
    }
}
