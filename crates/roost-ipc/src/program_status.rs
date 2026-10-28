//! OSC 7501 program-status reports and the OSC 133 prompt marks that
//! travel with them, as plain owned types.
//!
//! `roost-vt` delivers these from libghostty's callbacks in byte order;
//! `roost-engine`'s composition layer consumes them. They live here, not
//! in either, so the composition layer compiles without libghostty and the
//! wire types that summarise it sit beside the rest of the protocol.

use serde::{Deserialize, Serialize};

/// What a program says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
/// produces: reports and prompt marks, in the order their bytes arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusEvent {
    Report(ProgramStatusReport),
    Prompt(PromptMark),
}
