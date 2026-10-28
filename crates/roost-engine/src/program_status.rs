//! Program status (OSC 7501): a fourth per-tab axis, composed above the
//! hook layer's pure functions in [`roost_ipc::agent`] (plan 082 §3.5).
//!
//! Everything here is pure. The workspace keeps a tab's
//! [`ProgramStatusRecords`] beside its hook state and calls in with that
//! state, the terminal's ordered events and the clock; these functions say
//! what the tab shows, what to announce, and what a hook report may change.

use std::collections::BTreeMap;

use roost_ipc::agent::{
    self, AgentLifecycle, AgentTabState, ApplyOutcome, AttentionEffect, AttentionOp, DropReason,
    OwnershipAction, TabAgentReportParams,
};
use roost_ipc::format_chars::is_format;
use roost_ipc::messages::TabState;
pub use roost_ipc::program_status::{app_label, DecidedBy};
use roost_ipc::program_status::{
    ProgramStatusKind, ProgramStatusRecordView, ProgramStatusReport, ProgramStatusState,
    ProgramStatusSummary, PromptMark, StatusEvent,
};

/// Child records a tab keeps beside its root. The root is always accepted.
pub const MAX_CHILDREN: usize = 255;

/// How long after an agent's first record a Finished banner is discarded
/// rather than raised. Codex flashes `idle, working, idle` at startup; a
/// variant that ends in `done` must not announce a turn nobody ran.
/// NeedsInput is never held: Grok's trust prompt is `blocked` at 0.1 s.
pub const STARTUP_GUARD_MS: i64 = 1000;

const TEXT_MAX_BYTES: usize = 2048;
const NAME_MAX_BYTES: usize = 256;
const ROOT: &str = "";

/// One program's latest report. Its state is never `Clear`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub state: ProgramStatusState,
    pub kind: Option<ProgramStatusKind>,
    pub progress: Option<u8>,
    /// As reported; [`RecordView::app`] is the inherited one.
    pub app: String,
    pub title: String,
    pub msg: String,
    /// The tab's report count at this record's latest report.
    pub updated_seq: u64,
    /// Unix seconds.
    pub updated_at: i64,
    /// Whether this report has had its banner, raised or held.
    pub announced: bool,
}

/// A tab's program-status records, keyed by id (`""` is the root). Not
/// persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramStatusRecords {
    records: BTreeMap<String, Record>,
    /// A prompt mark arrived since the root was last set.
    root_prompted: bool,
    seq: u64,
    /// The first record since the tab opened or a `clear` emptied the
    /// records: where the startup guard counts from. Prompt marks emptying
    /// the records don't move it, since that isn't an agent starting.
    first_record_at_ms: Option<i64>,
    /// The cap refused a child and said so. Cleared once the children
    /// drop below the cap, so a burst of refusals costs one warning.
    cap_warned: bool,
}

/// The deciding record, with the app it inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordView<'a> {
    pub id: &'a str,
    /// The record's own `app`, else its nearest ancestor's along `/`
    /// boundaries, the root last.
    pub app: &'a str,
    pub record: &'a Record,
}

/// What a tab shows once program status and the hook layer are composed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Composed<'a> {
    pub lifecycle: AgentLifecycle,
    pub decided_by: DecidedBy,
    /// Absent when the hooks decide.
    pub record: Option<RecordView<'a>>,
}

impl Composed<'_> {
    /// The legacy `Tab.state`, mapped as [`agent::effective`] maps it.
    pub fn tab_state(&self) -> TabState {
        roost_ipc::program_status::tab_state(self.lifecycle)
    }
}

/// A notification the records ask for. The workspace still drops it for a
/// tab the user is watching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    /// Empty when the record has no title and no app anywhere in its
    /// ancestry; the workspace then raises it under the tab's title.
    pub title: String,
    pub body: String,
}

/// What one batch of status events asks of the workspace beyond keeping
/// the records.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StatusApplied {
    pub banners: Vec<Banner>,
    /// Children the cap refused, one per warning owed.
    refused: Vec<String>,
}

impl StatusApplied {
    /// Log what the batch refused. Call it once the workspace lock is
    /// released.
    pub fn emit_logs(&self, tab_id: i64) {
        for id in &self.refused {
            tracing::warn!(
                tab_id,
                id = ?id,
                max_children = MAX_CHILDREN,
                "program-status record refused: the tab is at its child-record cap"
            );
        }
    }
}

/// What a hook report did, with program status in front of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    /// As [`agent::apply_report`] returns it, or dropped as
    /// [`DropReason::ForeignApp`].
    pub outcome: ApplyOutcome,
    /// The report's stream, when it was accepted but the deciding record
    /// overrode a lifecycle change or a notification it would otherwise
    /// have made. `Tab.dropped_reports` counts the streams that had such a
    /// veto, each once, and leaves `last` alone.
    pub vetoed: Option<VetoStream>,
}

/// A hook report's stream: who sent it, in which session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VetoStream {
    pub source: String,
    pub session_id: String,
}

impl ProgramStatusRecords {
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Non-root records.
    pub fn children(&self) -> usize {
        self.records.len() - usize::from(self.records.contains_key(ROOT))
    }

    /// Whether program status decides the tab: some `working` or `blocked`
    /// record exists, or a root with no prompt mark since it was set.
    pub fn is_authoritative(&self) -> bool {
        self.deciding().is_some()
    }

    /// Apply one ordered batch from the tab's authoritative terminal.
    ///
    /// Banners are decided after each event, against the composed
    /// lifecycle before it. The caller hands each of the batch's OSC 133
    /// marks to the hook layer only after this returns, so composition
    /// reads a projection of `agent` that each prompt mark moves exactly as
    /// that later [`agent::apply_shell_mark`] will. Every event then sees
    /// the hook state it follows in the byte stream, and the result is the
    /// same however the stream was batched. `agent` itself is untouched.
    pub fn apply(
        &mut self,
        agent: &AgentTabState,
        events: impl IntoIterator<Item = StatusEvent>,
        now_ms: i64,
    ) -> StatusApplied {
        let mut applied = StatusApplied::default();
        let mut hooks = agent.clone();
        let mut before = compose(&hooks, self).lifecycle;
        for event in events {
            match event {
                StatusEvent::Report(report) => self.report(&report, now_ms, &mut applied),
                StatusEvent::Prompt(mark) => {
                    self.prompt(mark);
                    if let Some(next) = agent::apply_shell_mark(&hooks, osc133_body(mark)) {
                        hooks = next;
                    }
                }
            }
            if self.children() < MAX_CHILDREN {
                self.cap_warned = false;
            }
            before = self.announce(&hooks, before, now_ms, &mut applied);
        }
        applied
    }

    fn report(&mut self, report: &ProgramStatusReport, now_ms: i64, applied: &mut StatusApplied) {
        let id = clean(&report.id, NAME_MAX_BYTES);
        if report.state == ProgramStatusState::Clear {
            self.records.retain(|key, _| !in_subtree(key, &id));
            if self.records.is_empty() {
                self.first_record_at_ms = None;
            }
            return;
        }
        if id != ROOT && !self.records.contains_key(&id) && self.children() >= MAX_CHILDREN {
            if !std::mem::replace(&mut self.cap_warned, true) {
                applied.refused.push(id);
            }
            return;
        }
        self.first_record_at_ms.get_or_insert(now_ms);
        if id == ROOT {
            self.root_prompted = false;
        }
        self.seq += 1;
        let record = Record {
            state: report.state,
            kind: report.kind,
            progress: report.progress,
            app: clean(&report.app, NAME_MAX_BYTES),
            title: clean(&report.title, TEXT_MAX_BYTES),
            msg: clean(&report.msg, TEXT_MAX_BYTES),
            updated_seq: self.seq,
            updated_at: now_ms.div_euclid(1000),
            announced: false,
        };
        self.records.insert(id, record);
    }

    /// A, B and D all mean the shell is back at a prompt (plan §4.1a). C
    /// after one means the user started another command, having seen a
    /// kept result (§4.1b).
    fn prompt(&mut self, mark: PromptMark) {
        match mark {
            PromptMark::OutputStart => {
                if self.kept_root().is_some() {
                    self.records.remove(ROOT);
                }
            }
            PromptMark::PromptStart | PromptMark::InputStart | PromptMark::CommandEnd => {
                self.records.retain(|_, record| !is_live(record.state));
                self.root_prompted = true;
            }
        }
    }

    fn announce(
        &mut self,
        agent: &AgentTabState,
        before: AgentLifecycle,
        now_ms: i64,
        applied: &mut StatusApplied,
    ) -> AgentLifecycle {
        let composed = compose(agent, self);
        let after = composed.lifecycle;
        let Some(view) = composed.record.filter(|view| {
            after != before && composed.decided_by == DecidedBy::Records && !view.record.announced
        }) else {
            return after;
        };
        let fallback = match after {
            AgentLifecycle::Waiting => "Needs your input",
            AgentLifecycle::Failed => "Failed",
            AgentLifecycle::Finished => "Finished",
            AgentLifecycle::Inactive | AgentLifecycle::Working => return after,
        };
        let held = after == AgentLifecycle::Finished
            && self
                .first_record_at_ms
                .is_some_and(|first| now_ms - first < STARTUP_GUARD_MS);
        if !held {
            applied.banners.push(Banner {
                title: banner_title(view),
                body: non_empty(&view.record.msg).unwrap_or(fallback).to_owned(),
            });
        }
        let id = view.id.to_owned();
        if let Some(record) = self.records.get_mut(&id) {
            record.announced = true;
        }
        after
    }

    /// While authoritative, the record that decides: the highest-ranked
    /// `working`/`blocked` record, ties to the root and then to the latest
    /// report; with none, a root no prompt mark has followed.
    fn deciding(&self) -> Option<RecordView<'_>> {
        self.records
            .iter()
            .filter(|(_, record)| is_live(record.state))
            .max_by_key(|(id, record)| {
                (
                    agent::rank(lifecycle(record.state)),
                    id.as_str() == ROOT,
                    record.updated_seq,
                )
            })
            .or_else(|| {
                self.records
                    .get_key_value(ROOT)
                    .filter(|_| !self.root_prompted)
            })
            .map(|(id, record)| self.view(id, record))
    }

    fn kept_root(&self) -> Option<&Record> {
        self.records.get(ROOT).filter(|root| {
            self.root_prompted
                && matches!(
                    root.state,
                    ProgramStatusState::Done | ProgramStatusState::Error
                )
        })
    }

    fn view<'a>(&'a self, id: &'a str, record: &'a Record) -> RecordView<'a> {
        RecordView {
            id,
            app: self.inherited_app(id),
            record,
        }
    }

    fn inherited_app(&self, id: &str) -> &str {
        let mut at = id;
        loop {
            if let Some(app) = self.records.get(at).and_then(|r| non_empty(&r.app)) {
                return app;
            }
            if at == ROOT {
                return "";
            }
            at = at.rfind('/').map_or(ROOT, |slash| &at[..slash]);
        }
    }
}

/// What the tab shows (plan §3.5 D5.4): the deciding record while records
/// are authoritative; a kept root `done`/`error` when no hook drives the
/// tab; otherwise the hook layer, exactly as [`agent::effective_lifecycle`]
/// has it.
pub fn compose<'a>(agent: &AgentTabState, records: &'a ProgramStatusRecords) -> Composed<'a> {
    if let Some(view) = records.deciding() {
        return Composed {
            lifecycle: lifecycle(view.record.state),
            decided_by: DecidedBy::Records,
            record: Some(view),
        };
    }
    if let Some(root) = records.kept_root().filter(|_| !agent::agent_drives(agent)) {
        return Composed {
            lifecycle: lifecycle(root.state),
            decided_by: DecidedBy::KeptRecord,
            record: Some(records.view(ROOT, root)),
        };
    }
    Composed {
        lifecycle: agent::effective_lifecycle(agent),
        decided_by: DecidedBy::Hooks,
        record: None,
    }
}

/// `program_status` as the wire carries it: [`compose`]'s result with the
/// deciding record's text, or `None` for a tab with no records.
pub fn summary(
    agent: &AgentTabState,
    records: &ProgramStatusRecords,
) -> Option<ProgramStatusSummary> {
    if records.is_empty() {
        return None;
    }
    let composed = compose(agent, records);
    Some(ProgramStatusSummary {
        effective_lifecycle: composed.lifecycle,
        decided_by: composed.decided_by,
        children: u32::try_from(records.children()).unwrap_or(u32::MAX),
        record: composed.record.map(|view| ProgramStatusRecordView {
            state: view.record.state,
            kind: view.record.kind,
            msg: view.record.msg.clone(),
            app: view.app.to_owned(),
            title: view.record.title.clone(),
            progress: view.record.progress,
            id: view.id.to_owned(),
            updated_at: view.record.updated_at,
        }),
    })
}

/// [`agent::apply_report`] with program status in front of it (rule 5).
///
/// While records are authoritative, a claim from an agent other than the
/// deciding record's app is dropped, and every other agent report keeps
/// its identity effects but loses its lifecycle patch and any
/// notification. `manual` and `legacy` are someone taking the wheel: they
/// apply as sent, and the record still decides what the tab shows.
///
/// Once authority ends, an agent's notification that would leave the tab
/// showing a kept, announced root `done`/`error`'s lifecycle again is
/// dropped: that record's banner already said it.
///
/// Authority depends on the records alone, so no hook report can change a
/// records-decided lifecycle, and none raises a record banner.
pub fn apply_hook_report(
    agent: &AgentTabState,
    records: &ProgramStatusRecords,
    report: &TabAgentReportParams,
    now: i64,
) -> HookOutcome {
    if !agent::is_agent_source(&report.source) {
        return HookOutcome {
            outcome: agent::apply_report(agent, report, now),
            vetoed: None,
        };
    }
    let Some(deciding) = records.deciding() else {
        let mut outcome = agent::apply_report(agent, report, now);
        if matches!(outcome.attention, AttentionEffect::Set { .. })
            && repeats_kept_record(agent, &outcome.state, records)
        {
            outcome.attention = AttentionEffect::Unchanged;
        }
        return HookOutcome {
            outcome,
            vetoed: None,
        };
    };
    if report.ownership_action == OwnershipAction::Claim
        && !deciding.app.is_empty()
        && app_for_source(&report.source) != Some(deciding.app)
    {
        return HookOutcome {
            outcome: ApplyOutcome {
                state: agent.clone(),
                dropped: Some(DropReason::ForeignApp),
                ownership_changed: false,
                lifecycle_changed: false,
                attention: AttentionEffect::Unchanged,
            },
            vetoed: None,
        };
    }
    let mut held = report.clone();
    if held.lifecycle.is_some() {
        held.lifecycle_if = Some(Vec::new());
    }
    if held.attention == AttentionOp::Set {
        held.attention = AttentionOp::Preserve;
    }
    let outcome = agent::apply_report(agent, &held, now);
    // Whatever the report's own guard would have vetoed anyway isn't this
    // veto's doing, so compare against the report as sent.
    let as_sent = agent::apply_report(agent, report, now);
    let vetoed = outcome.accepted()
        && (outcome.state.lifecycle != as_sent.state.lifecycle
            || outcome.attention != as_sent.attention);
    HookOutcome {
        outcome,
        vetoed: vetoed.then(|| VetoStream {
            source: report.source.clone(),
            session_id: report.session_id.clone(),
        }),
    }
}

fn repeats_kept_record(
    before: &AgentTabState,
    after: &AgentTabState,
    records: &ProgramStatusRecords,
) -> bool {
    let shown = compose(before, records);
    shown.decided_by == DecidedBy::KeptRecord
        && shown.record.is_some_and(|view| view.record.announced)
        && compose(after, records).lifecycle == shown.lifecycle
}

/// The OSC 7501 `app` a hook source's agent reports (the 2026-10-28
/// captures). gx shares the `grok` source, so it is `grok-build` too.
pub fn app_for_source(source: &str) -> Option<&'static str> {
    match source {
        "claude" => Some("claude-code"),
        "codex" => Some("codex"),
        "grok" => Some("grok-build"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

/// A terminal's status sink, drained, in the composition layer's types.
#[cfg(feature = "vt")]
pub fn from_vt(events: Vec<roost_vt::TerminalEvent>) -> Vec<StatusEvent> {
    use roost_vt::{
        ProgramStatusKind as VtKind, ProgramStatusState as VtState, PromptMark as VtMark,
        TerminalEvent,
    };
    events
        .into_iter()
        .map(|event| match event {
            TerminalEvent::Status(report) => StatusEvent::Report(ProgramStatusReport {
                state: match report.state {
                    VtState::Idle => ProgramStatusState::Idle,
                    VtState::Working => ProgramStatusState::Working,
                    VtState::Done => ProgramStatusState::Done,
                    VtState::Blocked => ProgramStatusState::Blocked,
                    VtState::Error => ProgramStatusState::Error,
                    VtState::Clear => ProgramStatusState::Clear,
                },
                kind: report.kind.map(|kind| match kind {
                    VtKind::Permission => ProgramStatusKind::Permission,
                    VtKind::Question => ProgramStatusKind::Question,
                    VtKind::Auth => ProgramStatusKind::Auth,
                }),
                progress: report.progress,
                id: report.id,
                app: report.app,
                title: report.title,
                msg: report.message,
            }),
            TerminalEvent::Prompt(mark) => StatusEvent::Prompt(match mark {
                VtMark::PromptStart => PromptMark::PromptStart,
                VtMark::InputStart => PromptMark::InputStart,
                VtMark::OutputStart => PromptMark::OutputStart,
                VtMark::CommandEnd => PromptMark::CommandEnd,
            }),
        })
        .collect()
}

/// The OSC 133 body roost-osc hands the hook layer for the byte libghostty
/// reported as `mark`.
fn osc133_body(mark: PromptMark) -> &'static str {
    match mark {
        PromptMark::PromptStart => "A",
        PromptMark::InputStart => "B",
        PromptMark::OutputStart => "C",
        PromptMark::CommandEnd => "D",
    }
}

fn is_live(state: ProgramStatusState) -> bool {
    matches!(
        state,
        ProgramStatusState::Working | ProgramStatusState::Blocked
    )
}

fn lifecycle(state: ProgramStatusState) -> AgentLifecycle {
    match state {
        ProgramStatusState::Working => AgentLifecycle::Working,
        ProgramStatusState::Blocked => AgentLifecycle::Waiting,
        ProgramStatusState::Done => AgentLifecycle::Finished,
        ProgramStatusState::Error => AgentLifecycle::Failed,
        ProgramStatusState::Idle | ProgramStatusState::Clear => AgentLifecycle::Inactive,
    }
}

fn banner_title(view: RecordView<'_>) -> String {
    non_empty(&view.record.title)
        .or_else(|| app_label(view.app))
        .unwrap_or(view.app)
        .to_owned()
}

fn non_empty(text: &str) -> Option<&str> {
    (!text.is_empty()).then_some(text)
}

/// `id` is `of` or under it along `/` boundaries; everything is under the
/// root.
fn in_subtree(id: &str, of: &str) -> bool {
    of == ROOT
        || id
            .strip_prefix(of)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `text` without format characters, clipped to `max` bytes on a char
/// boundary: what reaches the wire, the panel and a banner.
fn clean(text: &str, max: usize) -> String {
    let mut out: String = text.chars().filter(|c| !is_format(*c)).collect();
    out.truncate(out.floor_char_boundary(max));
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use roost_ipc::agent::{Ownership, ShellState, SOURCE_LEGACY, SOURCE_MANUAL};

    use super::*;
    use AgentLifecycle as L;
    use DecidedBy as By;
    use ProgramStatusState as S;

    /// The first record's time; `LATE` is well past the startup guard.
    const T0: i64 = 1_793_158_450_000;
    const LATE: i64 = T0 + 60_000;
    const NOW_S: i64 = LATE / 1000;

    fn status(state: S, id: &str, app: &str, msg: &str) -> ProgramStatusReport {
        ProgramStatusReport {
            state,
            kind: None,
            progress: None,
            id: id.into(),
            app: app.into(),
            title: String::new(),
            msg: msg.into(),
        }
    }

    fn report(state: S, id: &str, app: &str, msg: &str) -> StatusEvent {
        StatusEvent::Report(status(state, id, app, msg))
    }

    fn claude(state: S) -> StatusEvent {
        report(state, "", "claude-code", "")
    }

    fn codex(state: S) -> StatusEvent {
        report(state, "", "codex", "")
    }

    fn clear(id: &str) -> StatusEvent {
        report(S::Clear, id, "", "")
    }

    /// Each OSC 133 mark with the body roost-osc hands the hook layer.
    const MARKS: [(&str, PromptMark); 4] = [
        ("A", PromptMark::PromptStart),
        ("B", PromptMark::InputStart),
        ("C", PromptMark::OutputStart),
        ("D", PromptMark::CommandEnd),
    ];

    fn mark(body: &str) -> PromptMark {
        MARKS
            .iter()
            .find(|(b, _)| *b == body)
            .unwrap_or_else(|| panic!("not a mark this test speaks: {body}"))
            .1
    }

    fn body(mark: PromptMark) -> &'static str {
        MARKS.iter().find(|(_, m)| *m == mark).unwrap().0
    }

    /// `events` split at `cuts`.
    fn batches<'a>(
        events: &'a [StatusEvent],
        cuts: &'a [usize],
    ) -> impl Iterator<Item = &'a [StatusEvent]> {
        let ends = cuts.iter().copied().chain([events.len()]);
        let starts = [0].into_iter().chain(cuts.iter().copied());
        starts.zip(ends).map(|(start, end)| &events[start..end])
    }

    fn hook(source: &str, action: OwnershipAction, lifecycle: Option<L>) -> TabAgentReportParams {
        TabAgentReportParams {
            session_id: "s1".into(),
            ..TabAgentReportParams::sessionless(1, source, action, lifecycle)
        }
    }

    fn session_start(source: &str) -> TabAgentReportParams {
        hook(source, OwnershipAction::Claim, Some(L::Inactive))
    }

    fn prompt_submit(source: &str) -> TabAgentReportParams {
        TabAgentReportParams {
            attention: AttentionOp::Clear,
            ..hook(source, OwnershipAction::Preserve, Some(L::Working))
        }
    }

    fn notifying(mut report: TabAgentReportParams, body: &str) -> TabAgentReportParams {
        report.attention = AttentionOp::Set;
        report.title = "Claude Code".into();
        report.body = body.into();
        report
    }

    fn permission(source: &str) -> TabAgentReportParams {
        notifying(
            hook(source, OwnershipAction::Preserve, Some(L::Waiting)),
            "Needs permission to use `Bash`",
        )
    }

    fn stop(source: &str) -> TabAgentReportParams {
        notifying(
            hook(source, OwnershipAction::Preserve, Some(L::Finished)),
            "Turn complete",
        )
    }

    /// Claude's unknown notification kinds: attention with no lifecycle.
    fn notice(source: &str) -> TabAgentReportParams {
        notifying(
            hook(source, OwnershipAction::Preserve, None),
            "Claude needs input",
        )
    }

    fn banner(title: &str, body: &str) -> Banner {
        Banner {
            title: title.into(),
            body: body.into(),
        }
    }

    /// One tab as the workspace will drive it: the hook layer, the records,
    /// every banner raised by either, and the vetoes and drops counted.
    #[derive(Default)]
    struct Tab {
        agent: AgentTabState,
        records: ProgramStatusRecords,
        banners: Vec<Banner>,
        vetoes: u64,
        veto_streams: BTreeSet<VetoStream>,
        drops: Vec<DropReason>,
    }

    impl Tab {
        /// One batch as both axes see it: the records from libghostty's
        /// ordered stream, then the batch's prompt marks to the hook layer
        /// from roost-osc, the order a session tab's ingest runs them in.
        fn feed(&mut self, events: impl IntoIterator<Item = StatusEvent>, now_ms: i64) {
            let events: Vec<StatusEvent> = events.into_iter().collect();
            let applied = self
                .records
                .apply(&self.agent, events.iter().cloned(), now_ms);
            self.banners.extend(applied.banners);
            for event in &events {
                if let StatusEvent::Prompt(mark) = event {
                    if let Some(next) = agent::apply_shell_mark(&self.agent, body(*mark)) {
                        self.agent = next;
                    }
                }
            }
        }

        fn mark(&mut self, body: &str, now_ms: i64) {
            self.feed([StatusEvent::Prompt(mark(body))], now_ms);
        }

        fn hook(&mut self, report: &TabAgentReportParams) -> HookOutcome {
            let hook = apply_hook_report(&self.agent, &self.records, report, NOW_S);
            match hook.outcome.dropped {
                Some(reason) => self.drops.push(reason),
                None => {
                    self.agent = hook.outcome.state.clone();
                    if let Some(stream) = &hook.vetoed {
                        self.vetoes += 1;
                        self.veto_streams.insert(stream.clone());
                    }
                    if let AttentionEffect::Set { title, body, .. } = &hook.outcome.attention {
                        self.banners.push(banner(title, body));
                    }
                }
            }
            hook
        }

        fn shows(&self) -> (L, By) {
            let composed = compose(&self.agent, &self.records);
            (composed.lifecycle, composed.decided_by)
        }

        fn msg(&self) -> Option<String> {
            compose(&self.agent, &self.records)
                .record
                .map(|view| view.record.msg.clone())
        }
    }

    fn owner(source: &str, lifecycle: L, shell: ShellState) -> AgentTabState {
        AgentTabState {
            shell,
            lifecycle,
            ownership: Some(Ownership {
                source: source.into(),
                session_id: "s1".into(),
                last_event_at: NOW_S,
                detail: String::new(),
                metadata: BTreeMap::new(),
            }),
        }
    }

    #[test]
    fn a_hook_then_a_record_with_the_same_lifecycle_raise_one_banner() {
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.hook(&prompt_submit("claude"));
        tab.hook(&permission("claude"));
        assert_eq!(tab.shows(), (L::Waiting, By::Hooks));

        tab.feed(
            [report(S::Blocked, "", "claude-code", "approve Bash: ls")],
            LATE,
        );
        assert_eq!(tab.shows(), (L::Waiting, By::Records));
        assert_eq!(
            tab.banners,
            [banner("Claude Code", "Needs permission to use `Bash`")]
        );
    }

    #[test]
    fn a_record_then_a_hook_raise_one_banner_and_the_hook_is_vetoed() {
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.feed([claude(S::Idle)], T0);
        tab.hook(&prompt_submit("claude"));
        tab.feed([claude(S::Working)], LATE);
        assert_eq!(tab.shows(), (L::Working, By::Records));

        tab.feed(
            [report(S::Blocked, "", "claude-code", "approve Bash: ls")],
            LATE,
        );
        tab.hook(&permission("claude"));
        assert_eq!(tab.shows(), (L::Waiting, By::Records));
        assert_eq!(tab.banners, [banner("Claude Code", "approve Bash: ls")]);
        assert_eq!(
            tab.agent.lifecycle,
            L::Inactive,
            "both hook lifecycles vetoed"
        );
        assert_eq!(tab.vetoes, 2);
        assert_eq!(tab.veto_streams.len(), 1, "one stream, counted once");
    }

    #[test]
    fn attention_only_hooks_while_authoritative_raise_nothing_in_either_order() {
        let mut record_first = Tab::default();
        record_first.hook(&session_start("claude"));
        record_first.feed([claude(S::Working)], T0);
        record_first.feed([claude(S::Blocked)], LATE);
        for _ in 0..3 {
            let hook = record_first.hook(&notice("claude"));
            assert_eq!(hook.outcome.attention, AttentionEffect::Unchanged);
            assert!(hook.vetoed.is_some());
        }
        assert_eq!(
            record_first.banners,
            [banner("Claude Code", "Needs your input")]
        );
        assert_eq!(record_first.vetoes, 3);

        let mut hook_first = Tab::default();
        hook_first.hook(&session_start("claude"));
        hook_first.feed([claude(S::Working)], T0);
        hook_first.hook(&notice("claude"));
        hook_first.hook(&notice("claude"));
        assert!(hook_first.banners.is_empty());
        hook_first.feed([claude(S::Blocked)], LATE);
        assert_eq!(
            hook_first.banners,
            [banner("Claude Code", "Needs your input")]
        );
        assert_eq!(hook_first.vetoes, 2);
    }

    #[test]
    fn a_late_hook_after_authority_ends_applies() {
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.hook(&prompt_submit("claude"));
        tab.feed([claude(S::Working)], T0);
        tab.feed([claude(S::Done)], LATE);
        assert!(tab.hook(&stop("claude")).vetoed.is_some());
        assert_eq!(tab.banners, [banner("Claude Code", "Finished")]);

        tab.mark("D", LATE);
        assert_eq!(tab.shows(), (L::Finished, By::KeptRecord));
        let late = tab.hook(&stop("claude"));
        assert!(late.outcome.accepted() && late.vetoed.is_none());
        assert_eq!(tab.agent.lifecycle, L::Finished);
        assert_eq!(tab.shows(), (L::Finished, By::Hooks));
    }

    #[test]
    fn a_kept_announced_record_mutes_a_hook_banner_that_repeats_its_lifecycle() {
        let kept_done = |hooks: bool| {
            let mut tab = screen_reader_claude(hooks);
            tab.feed([claude(S::Working)], LATE);
            tab.feed([claude(S::Done)], LATE + 10);
            tab.mark("C", LATE + 20);
            tab.mark("D", LATE + 20);
            assert_eq!(tab.shows(), (L::Finished, By::KeptRecord));
            tab
        };

        let mut late_stop = kept_done(true);
        let muted = late_stop.hook(&stop("claude"));
        assert!(muted.outcome.accepted());
        assert_eq!(muted.outcome.attention, AttentionEffect::Unchanged);
        assert_eq!(late_stop.shows(), (L::Finished, By::Hooks));
        assert_eq!(late_stop.banners, [banner("Claude Code", "Finished")]);

        let mut ask = kept_done(true);
        ask.hook(&permission("claude"));
        assert_eq!(ask.shows(), (L::Waiting, By::Hooks));
        assert_eq!(
            ask.banners,
            [
                banner("Claude Code", "Finished"),
                banner("Claude Code", "Needs permission to use `Bash`")
            ]
        );

        // A kept done that never announced doesn't mute anything.
        let mut quiet = screen_reader_claude(true);
        quiet.feed([claude(S::Working)], LATE);
        quiet.feed([report(S::Working, "bg", "", "")], LATE);
        quiet.feed([claude(S::Done)], LATE + 10);
        quiet.mark("D", LATE + 20);
        assert_eq!(quiet.shows(), (L::Finished, By::KeptRecord));
        quiet.hook(&stop("claude"));
        assert_eq!(quiet.banners, [banner("Claude Code", "Turn complete")]);

        // Nor does one C retired.
        let mut retired = kept_done(true);
        retired.mark("C", LATE + 30);
        retired.hook(&stop("claude"));
        assert_eq!(
            retired.banners,
            [
                banner("Claude Code", "Finished"),
                banner("Claude Code", "Turn complete")
            ]
        );
    }

    #[test]
    fn a_root_idle_under_a_foreground_process_shows_none() {
        let mut tab = Tab::default();
        tab.agent.shell = ShellState::ForegroundProcess;
        assert_eq!(tab.shows(), (L::Working, By::Hooks));

        tab.feed([codex(S::Idle)], T0);
        let composed = compose(&tab.agent, &tab.records);
        assert_eq!(
            (composed.lifecycle, composed.decided_by),
            (L::Inactive, By::Records)
        );
        assert_eq!(composed.tab_state(), TabState::None);
        assert_eq!(composed.record.map(|view| view.app), Some("codex"));
    }

    #[test]
    fn a_claim_from_a_foreign_app_is_dropped() {
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.feed([claude(S::Working)], T0);
        let before = tab.agent.clone();

        for source in ["codex", "cursor"] {
            let hook = tab.hook(&session_start(source));
            assert_eq!(
                hook.outcome.dropped,
                Some(DropReason::ForeignApp),
                "{source}"
            );
            assert!(!hook.outcome.accepted() && hook.vetoed.is_none());
            assert_eq!(hook.outcome.state, before);
        }
        // Only a claim can be foreign; anything else from a stranger is
        // dropped by the hook layer's own rule.
        let preserve = tab.hook(&prompt_submit("codex"));
        assert_eq!(preserve.outcome.dropped, Some(DropReason::NotOwner));
        assert_eq!(
            tab.drops,
            [
                DropReason::ForeignApp,
                DropReason::ForeignApp,
                DropReason::NotOwner
            ]
        );
        assert_eq!(tab.agent, before);
    }

    #[test]
    fn a_claim_from_the_same_app_changes_identity_but_not_the_lifecycle() {
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.hook(&prompt_submit("claude"));
        tab.feed([claude(S::Working)], T0);

        let next_session = TabAgentReportParams {
            session_id: "s2".into(),
            ..session_start("claude")
        };
        let hook = tab.hook(&next_session);
        assert!(hook.outcome.accepted() && hook.outcome.ownership_changed);
        assert!(hook.vetoed.is_some(), "its Inactive was suppressed");
        let owner = tab.agent.ownership.as_ref().unwrap();
        assert_eq!(
            (owner.source.as_str(), owner.session_id.as_str()),
            ("claude", "s2")
        );
        assert_eq!(tab.agent.lifecycle, L::Working);
        assert_eq!(tab.shows(), (L::Working, By::Records));

        // gx shares the grok source, so it is the same app as grok-build.
        let mut grok = Tab::default();
        grok.feed([report(S::Working, "", "grok-build", "")], T0);
        assert!(grok.hook(&session_start("grok")).outcome.accepted());
    }

    #[test]
    fn a_record_with_no_app_makes_no_claim_foreign() {
        let mut tab = Tab::default();
        tab.feed([report(S::Working, "", "", "")], T0);
        let hook = tab.hook(&session_start("codex"));
        assert!(hook.outcome.accepted());
        assert_eq!(tab.agent.ownership.as_ref().unwrap().source, "codex");
        assert_eq!(tab.shows(), (L::Working, By::Records));

        // A child's app counts even when the root has none.
        tab.feed([report(S::Blocked, "build", "make", "")], LATE);
        let hook = tab.hook(&session_start("claude"));
        assert_eq!(hook.outcome.dropped, Some(DropReason::ForeignApp));
    }

    #[test]
    fn manual_and_legacy_apply_to_the_hook_layer_and_the_record_still_decides() {
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.feed([claude(S::Working)], T0);

        let manual = tab.hook(&TabAgentReportParams::sessionless(
            1,
            SOURCE_MANUAL,
            OwnershipAction::Claim,
            Some(L::Waiting),
        ));
        assert!(manual.outcome.accepted() && manual.vetoed.is_none());
        assert_eq!(tab.agent.ownership.as_ref().unwrap().source, SOURCE_MANUAL);
        assert_eq!(tab.agent.lifecycle, L::Waiting);
        assert_eq!(tab.shows(), (L::Working, By::Records));

        let legacy = tab.hook(&TabAgentReportParams::sessionless(
            1,
            SOURCE_LEGACY,
            OwnershipAction::Claim,
            None,
        ));
        assert!(legacy.outcome.accepted());
        assert_eq!(tab.agent.ownership.as_ref().unwrap().source, SOURCE_LEGACY);
        assert_eq!(tab.shows(), (L::Working, By::Records));
    }

    #[test]
    fn blocked_then_error_raises_failed() {
        let mut tab = Tab::default();
        tab.feed([report(S::Idle, "", "crush", "")], T0);
        tab.feed([report(S::Working, "", "crush", "")], LATE);
        tab.feed([report(S::Blocked, "", "crush", "")], LATE);
        tab.feed([report(S::Error, "", "crush", "")], LATE);
        assert_eq!(tab.shows(), (L::Failed, By::Records));
        assert_eq!(
            tab.banners,
            [
                banner("Crush", "Needs your input"),
                banner("Crush", "Failed")
            ]
        );
    }

    #[test]
    fn each_record_announces_once() {
        let mut tab = Tab::default();
        tab.feed([claude(S::Working)], T0);
        tab.feed([claude(S::Done)], LATE);
        tab.feed([report(S::Working, "bg", "", "")], LATE);
        assert_eq!(tab.shows(), (L::Working, By::Records));
        tab.feed([clear("bg")], LATE);
        assert_eq!(tab.shows(), (L::Finished, By::Records));
        assert_eq!(tab.banners, [banner("Claude Code", "Finished")]);
    }

    #[test]
    fn a_kept_record_never_announces() {
        let mut tab = Tab::default();
        tab.feed([claude(S::Working)], T0);
        tab.feed([report(S::Working, "bg", "", "")], LATE);
        tab.feed([claude(S::Done)], LATE);
        assert_eq!(tab.shows(), (L::Working, By::Records));
        tab.mark("D", LATE);
        assert_eq!(tab.shows(), (L::Finished, By::KeptRecord));
        assert!(tab.banners.is_empty(), "{:?}", tab.banners);
    }

    #[test]
    fn the_next_command_retires_a_kept_done_without_announcing_it_again() {
        let mut tab = Tab::default();
        tab.feed([claude(S::Working)], T0);
        tab.feed([claude(S::Done)], LATE);
        tab.mark("D", LATE);
        assert_eq!(tab.shows(), (L::Finished, By::KeptRecord));

        tab.mark("C", LATE);
        assert!(tab.records.is_empty(), "C retires the kept done");
        assert_eq!(tab.shows(), (L::Working, By::Hooks));
        tab.mark("D", LATE);
        assert_eq!(tab.shows(), (L::Inactive, By::Hooks));
        assert_eq!(tab.banners, [banner("Claude Code", "Finished")]);

        // A kept error is retired the same way.
        let mut error = Tab::default();
        error.feed([claude(S::Error)], LATE);
        error.mark("D", LATE);
        assert_eq!(error.shows(), (L::Failed, By::KeptRecord));
        error.mark("C", LATE);
        assert!(error.records.is_empty());
    }

    #[test]
    fn the_startup_guard_holds_finished_but_not_the_state_or_needs_input() {
        // A Codex flash variant that ends in done: held and discarded.
        let mut flash = Tab::default();
        flash.feed([codex(S::Idle)], T0);
        flash.feed([codex(S::Working)], T0 + 300);
        flash.feed([codex(S::Done)], T0 + 999);
        assert_eq!(flash.shows(), (L::Finished, By::Records), "never delayed");
        assert!(flash.records.records[ROOT].announced);
        flash.feed([report(S::Idle, "later", "", "")], T0 + 5000);
        assert!(flash.banners.is_empty(), "never delivered later");

        // The flash Codex really sends can't raise one at all.
        let mut observed = Tab::default();
        observed.feed([codex(S::Idle)], T0);
        observed.feed([codex(S::Working)], T0 + 300);
        observed.feed([codex(S::Idle)], T0 + 600);
        assert_eq!(observed.shows(), (L::Inactive, By::Records));
        assert!(observed.banners.is_empty());

        let mut at_the_edge = Tab::default();
        at_the_edge.feed([codex(S::Working)], T0);
        at_the_edge.feed([codex(S::Done)], T0 + STARTUP_GUARD_MS);
        assert_eq!(at_the_edge.banners, [banner("Codex", "Finished")]);

        // Grok's trust prompt is blocked at 0.1 s, and must banner.
        let mut trust = Tab::default();
        trust.feed(
            [report(S::Blocked, "", "grok-build", "Trust this folder?")],
            T0 + 100,
        );
        assert_eq!(trust.banners, [banner("Grok", "Trust this folder?")]);

        let mut early_error = Tab::default();
        early_error.feed([report(S::Working, "", "crush", "")], T0);
        early_error.feed([report(S::Error, "", "crush", "")], T0 + 200);
        assert_eq!(early_error.banners, [banner("Crush", "Failed")]);
    }

    #[test]
    fn a_clear_restarts_the_startup_window_and_a_prompt_mark_does_not() {
        // A new agent after the last one's clear (or RIS, an empty-id
        // clear), whether the clear took everything or the last subtree.
        for (first, cleared) in [("", ""), ("job", "job")] {
            let mut tab = Tab::default();
            tab.feed([report(S::Working, first, "codex", "")], T0);
            tab.feed([clear(cleared)], T0 + 5000);
            assert_eq!(tab.records.first_record_at_ms, None);
            tab.feed([codex(S::Working)], T0 + 6000);
            tab.feed([codex(S::Done)], T0 + 6500);
            assert_eq!(tab.shows(), (L::Finished, By::Records));
            assert!(tab.banners.is_empty(), "{cleared:?}: {:?}", tab.banners);
        }

        let mut marked = Tab::default();
        marked.feed([codex(S::Working)], T0);
        marked.mark("D", T0 + 5000);
        assert!(marked.records.is_empty());
        marked.feed([codex(S::Working)], T0 + 6000);
        marked.feed([codex(S::Done)], T0 + 6500);
        assert_eq!(marked.banners, [banner("Codex", "Finished")]);
    }

    #[test]
    fn the_cap_refuses_the_256th_child_accepts_the_root_and_warns_once_per_burst() {
        let agent = AgentTabState::default();
        let mut records = ProgramStatusRecords::default();
        let child = |i: usize| report(S::Working, &format!("c{i}"), "", "");

        let applied = records.apply(&agent, (0..MAX_CHILDREN).map(child), T0);
        assert!(applied.refused.is_empty());
        assert_eq!(records.children(), 255);

        let applied = records.apply(&agent, [child(255)], T0);
        assert_eq!(applied.refused, ["c255"]);
        assert!(!records.records.contains_key("c255"));
        let applied = records.apply(&agent, [child(256)], T0);
        assert!(applied.refused.is_empty(), "warned once");
        assert_eq!(records.children(), 255);

        records.apply(&agent, [report(S::Done, "c0", "", "")], T0);
        assert_eq!(
            records.records["c0"].state,
            S::Done,
            "a known child updates"
        );
        records.apply(&agent, [claude(S::Working)], T0);
        assert!(
            records.records.contains_key(ROOT),
            "the root is always accepted"
        );
        assert_eq!(records.records.len(), 256);

        let applied = records.apply(&agent, [clear("c1"), child(257), child(258)], T0);
        assert!(records.records.contains_key("c257"));
        assert_eq!(applied.refused, ["c258"], "warned again after dropping");
    }

    #[test]
    fn setting_the_root_resets_root_prompted() {
        let mut tab = Tab::default();
        tab.feed([claude(S::Working)], T0);
        tab.feed([claude(S::Done)], LATE);
        tab.mark("D", LATE);
        assert_eq!(tab.shows(), (L::Finished, By::KeptRecord));

        tab.feed([claude(S::Idle)], LATE);
        assert_eq!(tab.shows(), (L::Inactive, By::Records));
        tab.feed([claude(S::Done)], LATE);
        assert_eq!(tab.shows(), (L::Finished, By::Records));
        assert_eq!(tab.banners.len(), 2);
    }

    #[test]
    fn ties_go_to_the_root_then_to_the_latest_report() {
        let mut tab = Tab::default();
        tab.feed(
            [
                report(S::Blocked, "", "opencode", "root"),
                report(S::Blocked, "a", "", "a"),
                report(S::Blocked, "b", "", "b"),
            ],
            T0,
        );
        assert_eq!(tab.msg().as_deref(), Some("root"));

        let mut children = Tab::default();
        children.feed([report(S::Working, "", "opencode", "2 sessions")], T0);
        children.feed([report(S::Blocked, "a", "", "a")], T0);
        children.feed([report(S::Blocked, "b", "", "b")], T0);
        assert_eq!(children.msg().as_deref(), Some("b"), "rank beats the root");
        children.feed([report(S::Blocked, "a", "", "a again")], T0);
        assert_eq!(children.msg().as_deref(), Some("a again"));
    }

    #[test]
    fn completed_children_alone_are_not_authoritative() {
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.hook(&prompt_submit("claude"));
        tab.feed(
            [
                report(S::Done, "a", "claude-code", ""),
                report(S::Error, "b", "claude-code", ""),
                report(S::Idle, "c", "claude-code", ""),
            ],
            LATE,
        );
        assert!(!tab.records.is_authoritative());
        let composed = compose(&tab.agent, &tab.records);
        assert_eq!(
            (composed.lifecycle, composed.decided_by),
            (L::Working, By::Hooks)
        );
        assert_eq!(composed.record, None);

        let stop = tab.hook(&stop("claude"));
        assert!(stop.vetoed.is_none());
        assert_eq!(tab.shows(), (L::Finished, By::Hooks));
        assert_eq!(tab.banners, [banner("Claude Code", "Turn complete")]);
    }

    #[test]
    fn clear_takes_a_subtree_and_app_inherits_along_slash_boundaries() {
        let mut tab = Tab::default();
        tab.feed(
            [
                report(S::Idle, "", "claude-code", ""),
                report(S::Working, "build", "make", ""),
                report(S::Working, "build/test", "", ""),
                report(S::Working, "build/test/unit", "", ""),
                report(S::Working, "builder", "", ""),
            ],
            T0,
        );
        assert_eq!(tab.records.inherited_app("build/test/unit"), "make");
        assert_eq!(tab.records.inherited_app("builder"), "claude-code");

        tab.feed([clear("build")], T0);
        let left: Vec<&str> = tab.records.records.keys().map(String::as_str).collect();
        assert_eq!(left, ["", "builder"]);
    }

    #[test]
    fn text_loses_format_characters_and_is_clipped_on_a_char_boundary() {
        let mut tab = Tab::default();
        let long = format!("x{}", "é".repeat(1500));
        tab.feed(
            [StatusEvent::Report(ProgramStatusReport {
                state: S::Blocked,
                kind: Some(ProgramStatusKind::Permission),
                progress: Some(40),
                id: format!("x\u{202e}{}", "é".repeat(200)),
                app: "a".repeat(300),
                title: "\u{202e}Deploy\u{2066} \u{200b}prod\u{feff}\u{ad}".into(),
                msg: long,
            })],
            LATE,
        );
        let (id, record) = tab.records.records.iter().next().unwrap();
        assert_eq!(record.title, "Deploy prod");
        assert_eq!(record.msg.len(), 2047, "2048 splits an é");
        assert!(record.msg.ends_with('é'));
        assert_eq!(record.app.len(), 256);
        assert!(!id.contains('\u{202e}'));
        assert_eq!(id.len(), 255, "x and 127 é, the override gone");
        assert_eq!(tab.banners.len(), 1);
        assert_eq!(tab.banners[0].title, "Deploy prod");
        assert_eq!(tab.banners[0].body, record.msg);
    }

    #[test]
    fn codex_ctrl_z_after_clear_hands_back_to_the_hooks_then_the_shell() {
        let mut tab = Tab::default();
        tab.mark("C", T0);
        tab.hook(&session_start("codex"));
        tab.feed([codex(S::Idle)], T0);
        tab.feed([codex(S::Working)], T0 + 300);
        tab.feed([codex(S::Idle)], T0 + 600);
        tab.hook(&prompt_submit("codex"));
        tab.feed([codex(S::Working)], LATE);
        tab.hook(&stop("codex"));
        tab.feed([codex(S::Done)], LATE);
        tab.feed([codex(S::Idle)], LATE);
        assert_eq!(tab.shows(), (L::Inactive, By::Records));
        assert_eq!(tab.agent.lifecycle, L::Inactive, "vetoes froze the hooks");

        // Ctrl-Z: clear, then the shell's prompt.
        tab.feed([clear("")], LATE);
        assert!(tab.records.is_empty());
        assert_eq!(
            tab.shows(),
            (L::Working, By::Hooks),
            "the shell's foreground"
        );
        tab.mark("D", LATE);
        assert_eq!(tab.shows(), (L::Inactive, By::Hooks));

        // fg, then SIGCONT's idle.
        tab.mark("C", LATE);
        assert_eq!(tab.shows(), (L::Working, By::Hooks));
        tab.feed([codex(S::Idle)], LATE);
        assert_eq!(tab.shows(), (L::Inactive, By::Records));

        assert_eq!(tab.agent.ownership.as_ref().unwrap().source, "codex");
        assert_eq!(tab.banners, [banner("Codex", "Finished")]);
    }

    /// Claude Code in Screen Reader Mode, launched from the shell, at its
    /// startup `idle`.
    fn screen_reader_claude(hooks: bool) -> Tab {
        let mut tab = Tab::default();
        tab.mark("C", T0);
        if hooks {
            tab.hook(&session_start("claude"));
        }
        tab.feed([claude(S::Idle)], T0);
        if hooks {
            tab.hook(&prompt_submit("claude"));
        }
        tab
    }

    #[test]
    fn screen_reader_working_a_working_stays_working_for_any_split() {
        let events = [
            claude(S::Working),
            StatusEvent::Prompt(PromptMark::PromptStart),
            report(S::Working, "", "claude-code", "Reading src/main.rs"),
        ];
        for hooks in [false, true] {
            for cuts in [&[][..], &[1], &[2], &[1, 2]] {
                let mut tab = screen_reader_claude(hooks);
                for batch in batches(&events, cuts) {
                    tab.feed(batch.to_vec(), LATE);
                }
                assert_eq!(tab.shows(), (L::Working, By::Records), "{hooks} {cuts:?}");
                assert_eq!(tab.msg().as_deref(), Some("Reading src/main.rs"));
                assert!(tab.banners.is_empty());
            }
        }
    }

    #[test]
    fn screen_reader_lone_working_a_falls_back_to_the_hooks_and_the_shell() {
        for hooks in [false, true] {
            let mut tab = screen_reader_claude(hooks);
            tab.feed([claude(S::Working)], LATE);
            tab.mark("A", LATE);
            assert!(tab.records.is_empty());
            assert_eq!(tab.shows(), (L::Inactive, By::Hooks), "hooks: {hooks}");
            assert_eq!(
                compose(&tab.agent, &tab.records).tab_state(),
                TabState::None
            );

            tab.feed([claude(S::Working)], LATE + 50);
            assert_eq!(tab.shows(), (L::Working, By::Records), "hooks: {hooks}");
        }
    }

    #[test]
    fn screen_reader_turn_end_shows_finished_in_both_orders() {
        for hooks in [false, true] {
            let mut done_c_d = screen_reader_claude(hooks);
            done_c_d.feed([claude(S::Working)], LATE);
            done_c_d.feed([claude(S::Done)], LATE + 10);
            if hooks {
                assert!(done_c_d.hook(&stop("claude")).vetoed.is_some());
            }
            done_c_d.mark("C", LATE + 20);
            assert_eq!(
                done_c_d.shows(),
                (L::Finished, By::Records),
                "hooks: {hooks}"
            );
            done_c_d.mark("D", LATE + 20);
            assert_eq!(
                done_c_d.shows(),
                (L::Finished, By::KeptRecord),
                "hooks: {hooks}"
            );
            assert_eq!(done_c_d.banners, [banner("Claude Code", "Finished")]);

            let mut c_d_done = screen_reader_claude(hooks);
            c_d_done.feed([claude(S::Working)], LATE);
            c_d_done.mark("C", LATE + 10);
            c_d_done.mark("D", LATE + 10);
            c_d_done.feed([claude(S::Done)], LATE + 20);
            assert_eq!(
                c_d_done.shows(),
                (L::Finished, By::Records),
                "hooks: {hooks}"
            );
            assert_eq!(
                c_d_done.banners,
                [banner("Claude Code", "Finished")],
                "hooks: {hooks}"
            );
        }
    }

    #[test]
    fn without_shell_integration_a_dead_agents_root_idle_stays_authoritative() {
        // §9 R7: no prompt mark ever arrives, and the agent died without
        // its clear.
        let mut tab = Tab::default();
        tab.hook(&session_start("claude"));
        tab.feed([claude(S::Idle)], T0);
        assert_eq!(tab.shows(), (L::Inactive, By::Records));

        let codex = tab.hook(&session_start("codex"));
        assert_eq!(codex.outcome.dropped, Some(DropReason::ForeignApp));
        assert_eq!(tab.agent.ownership.as_ref().unwrap().source, "claude");
        assert_eq!(tab.shows(), (L::Inactive, By::Records));
    }

    #[test]
    fn a_veto_is_flagged_only_when_it_suppressed_something() {
        let mut records = ProgramStatusRecords::default();
        records.apply(&AgentTabState::default(), [claude(S::Working)], T0);
        let agent = owner("claude", L::Working, ShellState::ForegroundProcess);
        let apply =
            |report: TabAgentReportParams| apply_hook_report(&agent, &records, &report, NOW_S);

        let same = apply(prompt_submit("claude"));
        assert!(same.outcome.accepted() && same.vetoed.is_none());
        assert_eq!(
            same.outcome.attention,
            AttentionEffect::Clear,
            "Clear passes"
        );

        let finished = apply(stop("claude"));
        assert!(finished.outcome.accepted() && finished.vetoed.is_some());
        assert_eq!(finished.outcome.state.lifecycle, L::Working);
        assert_eq!(finished.outcome.attention, AttentionEffect::Unchanged);

        assert!(apply(notice("claude")).vetoed.is_some());

        // Its own guard vetoes this one already.
        let nag = apply(TabAgentReportParams {
            lifecycle_if: Some(vec![L::Waiting]),
            ..stop("claude")
        });
        assert!(nag.outcome.accepted() && nag.vetoed.is_none());

        let release = apply(hook("claude", OwnershipAction::Release, None));
        assert!(release.outcome.accepted() && release.vetoed.is_none());
        assert_eq!(release.outcome.state.ownership, None);
        assert_eq!(release.outcome.state.lifecycle, L::Inactive);

        let stranger = apply(TabAgentReportParams {
            session_id: "s2".into(),
            ..stop("claude")
        });
        assert_eq!(stranger.outcome.dropped, Some(DropReason::NotOwner));
        assert!(stranger.vetoed.is_none());

        let stream = |session: &str| {
            Some(VetoStream {
                source: "claude".into(),
                session_id: session.into(),
            })
        };
        assert_eq!(finished.vetoed, stream("s1"));
        let next_session = apply(TabAgentReportParams {
            session_id: "s2".into(),
            ..session_start("claude")
        });
        assert_eq!(next_session.vetoed, stream("s2"));
    }

    #[test]
    fn no_hook_report_moves_a_tab_the_records_decide() {
        let reports = [
            session_start("claude"),
            prompt_submit("claude"),
            permission("claude"),
            stop("claude"),
            notice("claude"),
            hook("claude", OwnershipAction::Release, None),
            TabAgentReportParams::sessionless(
                1,
                SOURCE_MANUAL,
                OwnershipAction::Claim,
                Some(L::Failed),
            ),
        ];
        for state in [S::Idle, S::Working, S::Blocked, S::Done, S::Error] {
            let mut records = ProgramStatusRecords::default();
            records.apply(&AgentTabState::default(), [claude(state)], T0);
            let shown = compose(&AgentTabState::default(), &records);
            assert_eq!(shown.decided_by, By::Records);
            for shell in [
                ShellState::Unknown,
                ShellState::AtPrompt,
                ShellState::ForegroundProcess,
            ] {
                for lifecycle in [L::Inactive, L::Working, L::Waiting, L::Finished, L::Failed] {
                    let agent = owner("claude", lifecycle, shell);
                    assert_eq!(compose(&agent, &records), shown);
                    for report in &reports {
                        let next = apply_hook_report(&agent, &records, report, NOW_S);
                        assert_eq!(
                            compose(&next.outcome.state, &records),
                            shown,
                            "{state:?} {shell:?} {lifecycle:?} {report:?}"
                        );
                    }
                }
            }
        }
    }

    /// A deterministic xorshift, for the random batches.
    fn cuts_from(seed: u64, len: usize) -> Vec<usize> {
        let mut state = seed;
        let mut cuts = Vec::new();
        for at in 1..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if state.is_multiple_of(3) {
                cuts.push(at);
            }
        }
        cuts
    }

    /// The review's sequence: a hook layer that still says Waiting under a
    /// working record, then a prompt mark and a blocked record in one read.
    #[test]
    fn a_prompt_mark_and_a_record_in_one_batch_announce_as_in_two() {
        let run = |cuts: &[usize]| {
            let mut tab = Tab::default();
            tab.mark("C", 0);
            tab.hook(&hook("claude", OwnershipAction::Claim, Some(L::Waiting)));
            tab.feed([claude(S::Working)], 0);
            assert_eq!(tab.agent.lifecycle, L::Waiting);
            assert_eq!(tab.shows(), (L::Working, By::Records));

            let events = [
                StatusEvent::Prompt(mark("A")),
                report(S::Blocked, "", "claude-code", "approve"),
            ];
            for batch in batches(&events, cuts) {
                tab.feed(batch.to_vec(), 2000);
            }
            (tab.banners, tab.records, tab.agent)
        };
        let whole = run(&[]);
        assert_eq!(whole.0, [banner("Claude Code", "approve")]);
        assert!(whole.1.records[ROOT].announced);
        assert_eq!(run(&[1]), whole, "split at the mark");
    }

    #[test]
    fn the_same_events_give_the_same_result_however_they_are_batched() {
        let p = StatusEvent::Prompt;
        let events = vec![
            claude(S::Working),
            p(PromptMark::PromptStart),
            report(S::Working, "", "claude-code", "Reading"),
            report(S::Working, "bg", "", ""),
            report(S::Blocked, "", "claude-code", "approve Bash: ls"),
            report(S::Blocked, "", "claude-code", "approve Bash: ls -l"),
            claude(S::Working),
            report(S::Done, "bg", "", ""),
            p(PromptMark::OutputStart),
            claude(S::Done),
            claude(S::Done),
            p(PromptMark::CommandEnd),
            p(PromptMark::OutputStart),
            p(PromptMark::CommandEnd),
            claude(S::Idle),
            report(S::Working, "", "claude-code", "Again"),
            report(S::Blocked, "job/1", "", "job one"),
            report(S::Error, "", "claude-code", "Provider failed"),
            clear("job"),
            p(PromptMark::InputStart),
            p(PromptMark::OutputStart),
            claude(S::Blocked),
            clear("bg"),
        ];
        // The hook layer starts out driving Waiting, so a composition that
        // reads it stale after a prompt mark shows Waiting where the real
        // tab shows the shell, and misses the last record's banner.
        let run = |cuts: &[usize]| {
            let mut tab = Tab {
                agent: owner("claude", L::Waiting, ShellState::ForegroundProcess),
                ..Tab::default()
            };
            tab.feed([report(S::Idle, "anchor", "", "")], T0);
            for batch in batches(&events, cuts) {
                tab.feed(batch.to_vec(), LATE);
            }
            let composed = compose(&tab.agent, &tab.records);
            let shown = (
                composed.lifecycle,
                composed.decided_by,
                composed
                    .record
                    .map(|view| (view.id.to_owned(), view.record.clone())),
            );
            (tab.records.clone(), tab.agent.clone(), shown, tab.banners)
        };

        let whole = run(&[]);
        assert_eq!(
            whole.3,
            [
                banner("Claude Code", "approve Bash: ls"),
                banner("Claude Code", "Finished"),
                banner("Claude Code", "job one"),
                banner("Claude Code", "Provider failed"),
                banner("Claude Code", "Needs your input"),
            ]
        );
        assert_eq!((whole.2).0, L::Waiting);
        assert!(whole.0.records[ROOT].announced);

        let singles: Vec<usize> = (1..events.len()).collect();
        assert_eq!(run(&singles), whole, "one event per call");
        for seed in 1..=32 {
            let cuts = cuts_from(seed, events.len());
            assert_eq!(run(&cuts), whole, "cuts {cuts:?}");
        }
    }

    #[test]
    fn records_map_onto_the_lifecycle_and_tab_state() {
        for (state, lifecycle, tab_state) in [
            (S::Working, L::Working, TabState::Running),
            (S::Blocked, L::Waiting, TabState::NeedsInput),
            (S::Done, L::Finished, TabState::Idle),
            (S::Error, L::Failed, TabState::NeedsInput),
            (S::Idle, L::Inactive, TabState::None),
        ] {
            let mut records = ProgramStatusRecords::default();
            records.apply(&AgentTabState::default(), [claude(state)], T0);
            let composed = compose(&AgentTabState::default(), &records);
            assert_eq!(composed.lifecycle, lifecycle, "{state:?}");
            assert_eq!(composed.tab_state(), tab_state, "{state:?}");
        }
        let none = ProgramStatusRecords::default();
        for lifecycle in [L::Inactive, L::Working, L::Waiting, L::Finished, L::Failed] {
            let agent = owner("claude", lifecycle, ShellState::AtPrompt);
            let composed = compose(&agent, &none);
            assert_eq!(
                composed.tab_state(),
                agent::effective(&agent),
                "{lifecycle:?}"
            );
        }
    }

    #[test]
    fn a_banner_names_the_title_then_the_app_label_then_the_raw_app() {
        let blocked = |id: &str, app: &str, title: &str, msg: &str| {
            StatusEvent::Report(ProgramStatusReport {
                title: title.into(),
                ..status(S::Blocked, id, app, msg)
            })
        };
        let first_banner = |events: Vec<StatusEvent>| {
            let mut tab = Tab::default();
            tab.feed(events, LATE);
            tab.banners.into_iter().next().unwrap()
        };
        assert_eq!(
            first_banner(vec![blocked("", "claude-code", "Deploy", "approve")]),
            banner("Deploy", "approve")
        );
        for (app, label) in [
            ("claude-code", "Claude Code"),
            ("codex", "Codex"),
            ("grok-build", "Grok"),
            ("opencode", "OpenCode"),
            ("crush", "Crush"),
            ("pi", "Pi"),
            ("my-tool", "my-tool"),
        ] {
            assert_eq!(
                first_banner(vec![blocked("", app, "", "")]),
                banner(label, "Needs your input")
            );
        }
        assert_eq!(
            first_banner(vec![
                report(S::Idle, "", "opencode", ""),
                blocked("h1", "", "", "approve edit")
            ]),
            banner("OpenCode", "approve edit"),
            "a child inherits the root's app"
        );
        assert_eq!(
            first_banner(vec![blocked("", "", "", "")]),
            banner("", "Needs your input"),
            "the workspace names it after the tab"
        );
        assert_eq!(
            first_banner(vec![
                report(S::Idle, "", "", ""),
                blocked("job/1", "", "", "approve")
            ]),
            banner("", "approve")
        );
    }

    #[test]
    fn the_source_to_app_table_matches_the_captures() {
        for (source, app) in [
            ("claude", Some("claude-code")),
            ("codex", Some("codex")),
            ("grok", Some("grok-build")),
            ("opencode", Some("opencode")),
            ("cursor", None),
            (SOURCE_MANUAL, None),
        ] {
            assert_eq!(app_for_source(source), app, "{source}");
        }
    }

    #[test]
    fn prompt_marks_drop_working_and_blocked_and_c_drops_nothing() {
        let seeded = || {
            let mut records = ProgramStatusRecords::default();
            records.apply(
                &AgentTabState::default(),
                [
                    claude(S::Working),
                    report(S::Blocked, "a", "", ""),
                    report(S::Done, "b", "", ""),
                    report(S::Error, "c", "", ""),
                    report(S::Idle, "d", "", ""),
                ],
                T0,
            );
            records
        };
        for body in ["A", "B", "D"] {
            let mut records = seeded();
            records.apply(
                &AgentTabState::default(),
                [StatusEvent::Prompt(mark(body))],
                T0,
            );
            let left: Vec<&str> = records.records.keys().map(String::as_str).collect();
            assert_eq!(left, ["b", "c", "d"], "{body}");
            assert!(records.root_prompted, "{body}");
        }
        let mut records = seeded();
        records.apply(
            &AgentTabState::default(),
            [StatusEvent::Prompt(mark("C"))],
            T0,
        );
        assert_eq!(records, seeded());
    }

    #[test]
    fn a_claude_turn_with_hooks_announces_each_transition_once() {
        let mut tab = Tab::default();
        tab.mark("C", T0);
        tab.hook(&session_start("claude"));
        tab.feed([claude(S::Idle)], T0);
        tab.hook(&prompt_submit("claude"));
        tab.feed([claude(S::Working)], LATE);
        tab.feed(
            [report(S::Working, "", "claude-code", "Reading src/main.rs")],
            LATE,
        );
        tab.hook(&hook("claude", OwnershipAction::Preserve, Some(L::Working)));
        tab.hook(&permission("claude"));
        tab.feed(
            [report(S::Blocked, "", "claude-code", "approve Bash: ls")],
            LATE,
        );
        assert_eq!(tab.shows(), (L::Waiting, By::Records));
        tab.feed([claude(S::Working)], LATE);
        tab.hook(&stop("claude"));
        tab.feed([claude(S::Done)], LATE);
        assert_eq!(tab.shows(), (L::Finished, By::Records));
        tab.feed([claude(S::Idle)], LATE);
        assert_eq!(tab.shows(), (L::Inactive, By::Records));

        // Exit: three clears, SessionEnd, then the shell's prompt.
        tab.feed([clear(""), clear(""), clear("")], LATE);
        tab.hook(&hook("claude", OwnershipAction::Release, None));
        tab.mark("D", LATE);
        assert_eq!(tab.shows(), (L::Inactive, By::Hooks));
        assert_eq!(
            tab.banners,
            [
                banner("Claude Code", "approve Bash: ls"),
                banner("Claude Code", "Finished")
            ]
        );
        assert!(tab.drops.is_empty());
    }

    #[test]
    fn pi_probes_idles_and_clears_without_a_banner() {
        let mut tab = Tab::default();
        tab.feed([report(S::Idle, "", "pi", "")], T0);
        assert_eq!(tab.shows(), (L::Inactive, By::Records));
        tab.feed([clear("")], LATE);
        assert!(tab.records.is_empty() && tab.banners.is_empty());
    }

    #[cfg(feature = "vt")]
    #[test]
    fn from_vt_maps_every_state_kind_and_mark() {
        use roost_vt::{
            ProgramStatusKind as VtKind, ProgramStatusReport as VtReport,
            ProgramStatusState as VtState, PromptMark as VtMark, TerminalEvent,
        };
        let vt = |state, kind| {
            TerminalEvent::Status(VtReport {
                state,
                kind,
                progress: Some(40),
                id: "build/test".into(),
                app: "opencode".into(),
                title: "Build".into(),
                message: "approve edit".into(),
            })
        };
        let ours = |state, kind| {
            StatusEvent::Report(ProgramStatusReport {
                state,
                kind,
                progress: Some(40),
                id: "build/test".into(),
                app: "opencode".into(),
                title: "Build".into(),
                msg: "approve edit".into(),
            })
        };
        let converted = from_vt(vec![
            vt(VtState::Idle, None),
            vt(VtState::Working, None),
            vt(VtState::Done, None),
            vt(VtState::Blocked, Some(VtKind::Permission)),
            vt(VtState::Blocked, Some(VtKind::Question)),
            vt(VtState::Blocked, Some(VtKind::Auth)),
            vt(VtState::Error, None),
            vt(VtState::Clear, None),
            TerminalEvent::Prompt(VtMark::PromptStart),
            TerminalEvent::Prompt(VtMark::InputStart),
            TerminalEvent::Prompt(VtMark::OutputStart),
            TerminalEvent::Prompt(VtMark::CommandEnd),
        ]);
        assert_eq!(
            converted,
            [
                ours(S::Idle, None),
                ours(S::Working, None),
                ours(S::Done, None),
                ours(S::Blocked, Some(ProgramStatusKind::Permission)),
                ours(S::Blocked, Some(ProgramStatusKind::Question)),
                ours(S::Blocked, Some(ProgramStatusKind::Auth)),
                ours(S::Error, None),
                ours(S::Clear, None),
                StatusEvent::Prompt(PromptMark::PromptStart),
                StatusEvent::Prompt(PromptMark::InputStart),
                StatusEvent::Prompt(PromptMark::OutputStart),
                StatusEvent::Prompt(PromptMark::CommandEnd),
            ]
        );
    }
}
