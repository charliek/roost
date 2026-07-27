//! Agent adapters — the seam between a coding agent's own event
//! vocabulary and Roost's one op set.
//!
//! An adapter is a **pure function**: hook event JSON in, a list of
//! [`roost_ipc::agent::TabAgentReportParams`] out. No I/O, no socket, no
//! clap, no tokio. `roostctl` owns dialing the UI; this crate owns the
//! policy, so the mapping is unit-testable without a running Roost and a
//! second agent costs an afternoon (plan 002 §3.10).
//!
//! Adapters deliberately do **not** know which session currently owns a
//! tab. Session scoping is enforced downstream by `Workspace` via
//! [`roost_ipc::agent::apply_report`], which matches the report's
//! `(source, session_id)` pair against the live owner. A pure adapter
//! cannot know the current owner, and pretending otherwise is what
//! forced the explicit patch semantics on the op (plan §3.3, §3.6).

#![deny(unsafe_op_in_unsafe_fn)]

pub mod claude;

pub use claude::claude_event_to_reports;
