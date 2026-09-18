//! `roostctl tab report` — a thin wrapper over `tab.agent_report` for an
//! agent with no adapter yet, or a script driving the op by hand (plan
//! 067 §3.7).
//!
//! The CLI flags map one-to-one onto
//! [`roost_ipc::agent::TabAgentReportParams`]; this module holds the
//! parsing, the client-side guards that mirror the server's own
//! [`validate_report`], [`build_params`] that assembles the two into a
//! validated struct, and the two human-readable output lines. It stays
//! a thin layer over the typed struct rather than hand-rolled JSON,
//! same as every other verb in `main.rs`.

use std::collections::BTreeMap;

use roost_ipc::agent::{
    validate_report, AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
    SOURCE_LEGACY, SOURCE_MANUAL,
};
use roost_ipc::messages::Tab;

use crate::error::CliError;

/// `--lifecycle` / `--if`: clap's `value_parser` already restricts the
/// input to these five spellings, so the `other` arm is unreachable in
/// practice — kept as a returned error, not a panic, so a future
/// mismatch between the two lists fails a request instead of the
/// process.
fn parse_lifecycle(s: &str) -> Result<AgentLifecycle, CliError> {
    Ok(match s {
        "inactive" => AgentLifecycle::Inactive,
        "working" => AgentLifecycle::Working,
        "waiting" => AgentLifecycle::Waiting,
        "finished" => AgentLifecycle::Finished,
        "failed" => AgentLifecycle::Failed,
        other => return Err(CliError::Usage(format!("unknown lifecycle '{other}'"))),
    })
}

fn parse_attention(s: &str) -> Result<AttentionOp, CliError> {
    Ok(match s {
        "set" => AttentionOp::Set,
        "clear" => AttentionOp::Clear,
        "preserve" => AttentionOp::Preserve,
        other => return Err(CliError::Usage(format!("unknown attention '{other}'"))),
    })
}

fn parse_severity(s: &str) -> Result<Severity, CliError> {
    Ok(match s {
        "info" => Severity::Info,
        "warn" => Severity::Warn,
        "error" => Severity::Error,
        other => return Err(CliError::Usage(format!("unknown severity '{other}'"))),
    })
}

/// Exactly one of `--claim` / `--preserve` / `--release` is required:
/// [`OwnershipAction`] has no `Default` (see its doc) — a report's
/// ownership intent must always be explicit. clap's `conflicts_with_all`
/// on the three flags already refuses two at once; this catches zero.
fn ownership_action(
    claim: bool,
    preserve: bool,
    release: bool,
) -> Result<OwnershipAction, CliError> {
    match (claim, preserve, release) {
        (true, false, false) => Ok(OwnershipAction::Claim),
        (false, true, false) => Ok(OwnershipAction::Preserve),
        (false, false, true) => Ok(OwnershipAction::Release),
        _ => Err(CliError::Usage(
            "exactly one of --claim, --preserve, --release is required".into(),
        )),
    }
}

/// Refuse a `--source` this binary already knows as an agent's own
/// (plus the two manual/legacy sources), so `tab report` cannot be used
/// to impersonate an adapter. Read off the one inventory
/// (`roost_agent::ALL_AGENTS` + the two constants beside
/// `TabAgentReportParams`) rather than a second list kept in step by
/// hand.
fn check_source(source: &str) -> Result<(), CliError> {
    let reserved = roost_agent::ALL_AGENTS
        .iter()
        .map(|agent| agent.source())
        .chain([SOURCE_MANUAL, SOURCE_LEGACY])
        .any(|candidate| candidate == source);
    if reserved {
        return Err(CliError::Usage(format!(
            "--source '{source}' is reserved for a Roost agent adapter (or `manual`/`legacy`); \
             pick a different source"
        )));
    }
    Ok(())
}

/// `--metadata KEY=VALUE`, repeatable. Splits on the first `=`; an empty
/// key or a repeated key is `usage` — the server merges maps, so a
/// duplicate on one command line is a mistake, not an order.
fn parse_metadata(entries: &[String]) -> Result<BTreeMap<String, String>, CliError> {
    let mut metadata = BTreeMap::new();
    for entry in entries {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(CliError::Usage(format!(
                "--metadata '{entry}' is not KEY=VALUE"
            )));
        };
        if key.is_empty() {
            return Err(CliError::Usage(format!(
                "--metadata '{entry}' has an empty key"
            )));
        }
        if metadata
            .insert(key.to_string(), value.to_string())
            .is_some()
        {
            return Err(CliError::Usage(format!(
                "--metadata key '{key}' given more than once"
            )));
        }
    }
    Ok(metadata)
}

/// The same shape check the server runs before it touches ownership
/// (`validate_report`, reused rather than re-derived so the two cannot
/// drift): empty source, and `attention: set` without a title or body.
fn validate(params: &TabAgentReportParams) -> Result<(), CliError> {
    validate_report(params).map_err(|e| CliError::Usage(e.to_string()))
}

/// Turns the raw `tab report` flags into a validated
/// `TabAgentReportParams`, in the same order `run_on_ui` used to inline:
/// ownership intent, source, metadata, lifecycle(s), then the server's
/// own shape check. Keeps the dispatch arm to call-then-print, same as
/// `dump_tab` for `tab dump`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_params(
    tab_id: i64,
    source: String,
    session_id: String,
    claim: bool,
    preserve: bool,
    release: bool,
    lifecycle: Option<&str>,
    lifecycle_if: &[String],
    attention: &str,
    severity: &str,
    title: String,
    body: String,
    detail: String,
    metadata: &[String],
) -> Result<TabAgentReportParams, CliError> {
    let ownership_action = ownership_action(claim, preserve, release)?;
    check_source(&source)?;
    let metadata = parse_metadata(metadata)?;
    let lifecycle = lifecycle.map(parse_lifecycle).transpose()?;
    let lifecycle_if = if lifecycle_if.is_empty() {
        None
    } else {
        Some(
            lifecycle_if
                .iter()
                .map(|s| parse_lifecycle(s))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    let params = TabAgentReportParams {
        tab_id,
        source,
        session_id,
        ownership_action,
        lifecycle,
        lifecycle_if,
        attention: parse_attention(attention)?,
        severity: parse_severity(severity)?,
        title,
        body,
        detail,
        metadata,
    };
    validate(&params)?;
    Ok(params)
}

/// `accepted: true` — the **returned** lifecycle, since `release` forces
/// `inactive` in the reply even with no `--lifecycle` given.
pub(crate) fn accepted_line(tab_id: i64, tab: &Tab) -> String {
    format!(
        "reported: tab {tab_id} {}",
        lifecycle_word(tab.agent_lifecycle)
    )
}

/// `accepted: false` — the report lost the ownership check; say who
/// holds the tab, or that nobody does.
pub(crate) fn not_accepted_line(tab_id: i64, tab: &Tab) -> String {
    match &tab.ownership {
        Some(owner) => format!(
            "not accepted: tab {tab_id} is owned by {}/{}",
            owner.source, owner.session_id
        ),
        None => format!("not accepted: tab {tab_id} has no owner"),
    }
}

fn lifecycle_word(lifecycle: AgentLifecycle) -> &'static str {
    match lifecycle {
        AgentLifecycle::Inactive => "inactive",
        AgentLifecycle::Working => "working",
        AgentLifecycle::Waiting => "waiting",
        AgentLifecycle::Finished => "finished",
        AgentLifecycle::Failed => "failed",
    }
}
