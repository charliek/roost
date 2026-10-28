//! OSC 7501 program-status reports and the OSC 133 prompt marks that
//! travel with them, as plain owned types.
//!
//! `roost-vt` delivers these from libghostty's callbacks in byte order;
//! `roost-engine`'s composition layer consumes them. They live here, not
//! in either, so the composition layer compiles without libghostty and the
//! wire types that summarise it sit beside the rest of the protocol.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::{self, AgentLifecycle, AgentTabState};
use crate::messages::TabState;

/// What a program says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProgramStatusState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    /// Not a state: remove the record with this id and its subtree, or
    /// every record when the id is empty (also what RIS reports).
    Clear,
}

/// What a blocked program needs from the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

/// An OSC 133 step. `PromptStart` is a primary prompt only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMark {
    PromptStart,
    InputStart,
    OutputStart,
    CommandEnd,
}

/// One OSC 7501 report. Text the program didn't send is empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramStatusReport {
    pub state: ProgramStatusState,
    #[serde(default)]
    pub kind: Option<ProgramStatusKind>,
    /// 0 through 100.
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub msg: String,
}

/// One entry of the ordered stream a tab's authoritative terminal
/// produces: reports, prompt marks and desktop-notification places, in
/// the order their bytes arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusEvent {
    Report(ProgramStatusReport),
    Prompt(PromptMark),
    /// An OSC 9 / 777 notification, where it fell. It changes nothing; it
    /// marks the point the raw notification's suppression is judged at.
    /// OSC 9 has no title, so its text is the body.
    Notification {
        title: String,
        body: String,
    },
}

/// Who decided a tab's composed lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecidedBy {
    Records,
    /// A root `done`/`error` a prompt mark ended the authority of, still
    /// displayed because no hook drives the tab.
    KeptRecord,
    Hooks,
}

/// `program_status` on `Tab` and `agent_report.changed`: what a tab's
/// program-status records and its hook layer compose to. Present while the
/// tab has any record, absent otherwise.
///
/// An open shape whose enums are open values: a client that can't read a
/// newer server's value reads the whole object as absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProgramStatusSummary {
    /// What the tab shows. `Tab.state` is its legacy projection.
    pub effective_lifecycle: AgentLifecycle,
    pub decided_by: DecidedBy,
    /// Records other than the root. Counted, not listed.
    pub children: u32,
    /// The deciding record. Absent when the hooks decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<ProgramStatusRecordView>,
}

/// The record that decides a tab, as the wire carries it. Text the program
/// didn't send is omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProgramStatusRecordView {
    pub state: ProgramStatusState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ProgramStatusKind>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub msg: String,
    /// Inherited from the nearest ancestor along `/` boundaries when the
    /// record names none itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// 0 through 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    /// `/`-separated; empty (omitted) is the root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Unix seconds of the record's latest report.
    pub updated_at: i64,
}

/// The lifecycle a tab presents: `summary`'s composed one when the tab has
/// program-status records, else the hook layer's.
pub fn effective_lifecycle(
    hooks: &AgentTabState,
    summary: Option<&ProgramStatusSummary>,
) -> AgentLifecycle {
    summary.map_or_else(
        || agent::effective_lifecycle(hooks),
        |status| status.effective_lifecycle,
    )
}

/// `Tab.state`: [`effective_lifecycle`] projected as [`agent::effective`]
/// projects the hook layer's.
pub fn effective(hooks: &AgentTabState, summary: Option<&ProgramStatusSummary>) -> TabState {
    tab_state(effective_lifecycle(hooks, summary))
}

/// [`agent::effective`]'s projection, for a lifecycle already composed.
pub fn tab_state(lifecycle: AgentLifecycle) -> TabState {
    match lifecycle {
        AgentLifecycle::Inactive => TabState::None,
        AgentLifecycle::Working => TabState::Running,
        AgentLifecycle::Waiting | AgentLifecycle::Failed => TabState::NeedsInput,
        AgentLifecycle::Finished => TabState::Idle,
    }
}

/// Whether raw OSC 9 / 99 / 777 notifications are dropped: a hook agent
/// drives the tab, or its program-status records decide it.
pub fn suppress_raw_osc(hooks: &AgentTabState, summary: Option<&ProgramStatusSummary>) -> bool {
    agent::suppress_raw_osc(hooks)
        || summary.is_some_and(|status| status.decided_by == DecidedBy::Records)
}

/// A known OSC 7501 `app`'s display name: a record banner's title and a
/// status-only agent row's name.
pub fn app_label(app: &str) -> Option<&'static str> {
    match app {
        "claude-code" => Some("Claude Code"),
        "codex" => Some("Codex"),
        "grok-build" => Some("Grok"),
        "opencode" => Some("OpenCode"),
        "crush" => Some("Crush"),
        "pi" => Some("Pi"),
        _ => None,
    }
}
