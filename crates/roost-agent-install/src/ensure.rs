//! `ensure`, `install`, `uninstall`, `status` — the four things the
//! callers actually do.
//!
//! Each of the first three takes the lock, plans every agent it is
//! responsible for, applies what it planned, and updates the record —
//! in that order, once, under one lock. Planning inside the lock is the
//! point: an atomic rename stops a torn file, but two ensures that both
//! *read* before either *wrote* would still lose one of the two writes.
//!
//! Nothing here decides policy. `mode` and `skip` arrive as parameters
//! rather than as a config read, so `roost-cli` does not have to grow a
//! dependency on the config parser to call it, and the UI, the CLI and a
//! host session can all pass the same values from wherever they hold
//! them.

use roost_agent::Agent;

use crate::command::INTEGRATION_VERSION;
use crate::error::{AgentError, AgentSkip, AgentWarning, InstallError, SkipReason};
use crate::home::{Home, ALL_AGENTS};
use crate::plan::{apply, Guard, InstallPlan, Intent};
use crate::state::{self, Record};
use crate::{claude, codex, cursor, grok, opencode};

/// Whether Roost wires agents on this machine at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Auto,
    Off,
}

/// Resolve `agent-hooks-skip`'s names against the agents this crate can
/// wire: the ones it recognises, and the spellings it does not.
///
/// The unknown half is returned rather than dropped because the only
/// thing a user gets from a typo'd skip entry is an agent that keeps
/// being wired, with nothing to say why. Every caller reports the list
/// on its own surface; none of them treats it as fatal, so a name added
/// by a newer Roost does not break an older one's config.
pub fn skip_list<'a>(names: impl IntoIterator<Item = &'a str>) -> (Vec<Agent>, Vec<String>) {
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        match Agent::parse(trimmed) {
            Some(agent) if !known.contains(&agent) => known.push(agent),
            Some(_) => {}
            None => unknown.push(trimmed.to_string()),
        }
    }
    (known, unknown)
}

/// The agent names [`skip_list`] accepts, for a caller's error message.
pub fn agent_names() -> String {
    ALL_AGENTS
        .iter()
        .map(|agent| agent.source())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What one `ensure` / `install` / `uninstall` did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Wired for the first time **in this run**.
    pub wired: Vec<Agent>,
    /// Wired on this machine and never announced — every record entry
    /// whose `noticed` is false, after this run's writes. **This is the
    /// toast list**, and it is deliberately not [`Self::wired`]: what
    /// the user has been told is a property of the machine, not of the
    /// run that happens to be looking (plan 046 §3.3).
    ///
    /// Two cases `wired` gets wrong, and this one gets right. The Mac
    /// app wires through `roostctl` and has no transient status surface,
    /// so it leaves `noticed` false for the first iced launch to say —
    /// which that launch would classify as `current` and never mention.
    /// And a `mark_noticed` that fails leaves the flag false, which is
    /// the whole retry the design promises.
    ///
    /// Only ever set by [`ensure`]; the explicit verbs do not toast.
    pub unnoticed: Vec<Agent>,
    /// Already wired, brought up to the current integration version.
    pub refreshed: Vec<Agent>,
    /// Wired and current; nothing to do.
    pub current: Vec<Agent>,
    /// Roost's entries taken back out.
    pub removed: Vec<Agent>,
    pub skipped: Vec<AgentSkip>,
    pub errors: Vec<AgentError>,
    pub warnings: Vec<AgentWarning>,
    /// Whether anything at all was written, including the record.
    pub wrote: bool,
}

impl Outcome {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Where an agent stands right now, without changing anything.
#[derive(Debug)]
pub struct Status {
    pub agent: Agent,
    pub present: bool,
    /// The integration version the record says is wired.
    pub wired: Option<u32>,
    /// True when a fresh `plan` would make no edits.
    pub up_to_date: bool,
    pub noticed: bool,
    pub skipped: Option<SkipReason>,
    pub warnings: Vec<crate::error::Warning>,
}

/// An uninstall reads the state record as well as the disk: it is what
/// says which files Roost **created** (and so may delete) and what
/// codex's `[features] hooks` was before Roost set it. Loading it here
/// rather than threading it through every signature keeps
/// [`plan`]'s shape for the callers that only want to look.
fn plan_for(agent: Agent, home: &Home, intent: Intent) -> Result<InstallPlan, InstallError> {
    match (agent, intent) {
        (Agent::Claude, Intent::Install) => claude::plan_install(home),
        (Agent::Claude, Intent::Uninstall) => {
            claude::plan_uninstall(home, &state::prior(home, agent)?)
        }
        (Agent::Codex, Intent::Install) => codex::plan_install(home),
        (Agent::Codex, Intent::Uninstall) => {
            codex::plan_uninstall(home, &state::prior(home, agent)?)
        }
        (Agent::Grok, Intent::Install) => grok::plan_install(home),
        (Agent::Grok, Intent::Uninstall) => grok::plan_uninstall(home),
        (Agent::Cursor, Intent::Install) => cursor::plan_install(home),
        (Agent::Cursor, Intent::Uninstall) => {
            cursor::plan_uninstall(home, &state::prior(home, agent)?)
        }
        (Agent::Opencode, Intent::Install) => opencode::plan_install(home),
        (Agent::Opencode, Intent::Uninstall) => opencode::plan_uninstall(home),
    }
}

/// Plan one agent without holding a lock or writing anything.
///
/// The public half of the plan/apply split: callers that want to *see*
/// the edits — a dry run, a doctor check — use this and never call
/// [`apply`].
pub fn plan(agent: Agent, home: &Home, mode: Mode) -> Result<InstallPlan, InstallError> {
    match mode {
        Mode::Off => plan_for(agent, home, Intent::Uninstall),
        Mode::Auto if !home.is_present(agent) => Ok(InstallPlan::skip(
            agent,
            Intent::Install,
            SkipReason::NotPresent,
        )),
        Mode::Auto => plan_for(agent, home, Intent::Install),
    }
}

/// What the UIs and `roostctl agent ensure` run.
///
/// `auto` wires every present agent that is not skipped; `off` removes
/// Roost's entries from every present agent **and** from every agent the
/// record names, so a machine whose `~/.config/roost` was wiped still
/// comes clean. With nothing to remove, `off` writes nothing at all.
pub fn ensure(
    home: &Home,
    mode: Mode,
    skip: &[Agent],
    by: &str,
    guard: Guard,
) -> Result<Outcome, InstallError> {
    guard.check()?;
    let _lock = crate::write::lock(&home.lock_path())?;
    let (mut record, warning) = state::load(home)?;
    let mut outcome = Outcome::default();
    if let Some(warning) = warning {
        // Not attributable to one agent; the first one carries it so the
        // caller's rendering has somewhere to put it.
        outcome.warnings.push(AgentWarning {
            agent: ALL_AGENTS[0],
            warning,
        });
    }

    match mode {
        Mode::Auto => {
            for agent in ALL_AGENTS {
                if skip.contains(&agent) {
                    outcome.skipped.push(AgentSkip {
                        agent,
                        reason: SkipReason::SkipList,
                    });
                    continue;
                }
                wire_one(home, agent, by, guard, &mut record, &mut outcome);
            }
        }
        Mode::Off => {
            for agent in unwire_targets(home, &record) {
                unwire_one(home, agent, guard, &mut record, &mut outcome);
            }
        }
    }

    outcome.unnoticed = unnoticed(&record);
    outcome.wrote |= state::save(home, &record)?;
    Ok(outcome)
}

/// Every agent the record says is wired and has never been announced.
///
/// Read off the record rather than off this run's `wired` list, which is
/// the difference between "Roost has told you about this agent" and
/// "this particular process is the one that wired it". See
/// [`Outcome::unnoticed`].
fn unnoticed(record: &Record) -> Vec<Agent> {
    ALL_AGENTS
        .into_iter()
        .filter(|agent| state::entry(record, *agent).is_some_and(|entry| !entry.noticed))
        .collect()
}

/// Wire exactly these agents, whatever the mode says.
///
/// `agent install <name>` is an explicit instruction, and explicit wins:
/// a user who has turned `agent-hooks` off and then asks for one agent
/// gets that agent. The skip list is not consulted either, for the same
/// reason.
pub fn install(
    home: &Home,
    agents: &[Agent],
    by: &str,
    guard: Guard,
) -> Result<Outcome, InstallError> {
    guard.check()?;
    let _lock = crate::write::lock(&home.lock_path())?;
    let (mut record, _) = state::load(home)?;
    let mut outcome = Outcome::default();
    for agent in agents {
        wire_one(home, *agent, by, guard, &mut record, &mut outcome);
    }
    outcome.wrote |= state::save(home, &record)?;
    Ok(outcome)
}

pub fn uninstall(home: &Home, agents: &[Agent], guard: Guard) -> Result<Outcome, InstallError> {
    guard.check()?;
    let _lock = crate::write::lock(&home.lock_path())?;
    let (mut record, _) = state::load(home)?;
    let mut outcome = Outcome::default();
    for agent in agents {
        unwire_one(home, *agent, guard, &mut record, &mut outcome);
    }
    outcome.wrote |= state::save(home, &record)?;
    Ok(outcome)
}

/// Read-only: what `roostctl agent status` and doctor render.
pub fn status(home: &Home) -> Result<Vec<Status>, InstallError> {
    let (record, _) = state::load(home)?;
    ALL_AGENTS
        .into_iter()
        .map(|agent| {
            let entry = state::entry(&record, agent);
            let present = home.is_present(agent);
            let plan = if present {
                Some(plan_for(agent, home, Intent::Install)?)
            } else {
                None
            };
            Ok(Status {
                agent,
                present,
                wired: entry.map(|e| e.integration_version),
                up_to_date: plan.as_ref().is_some_and(InstallPlan::is_noop),
                noticed: entry.is_some_and(|e| e.noticed),
                skipped: match &plan {
                    Some(plan) => plan.skipped.clone(),
                    None => Some(SkipReason::NotPresent),
                },
                warnings: plan.map(|p| p.warnings).unwrap_or_default(),
            })
        })
        .collect()
}

/// Everything `off` has to clean: what is installed now, plus what the
/// record says Roost has touched. Either list alone leaves a case
/// behind — an agent uninstalled since, or a record lost with
/// `~/.config/roost`.
fn unwire_targets(home: &Home, record: &Record) -> Vec<Agent> {
    ALL_AGENTS
        .into_iter()
        .filter(|agent| home.is_present(*agent) || state::entry(record, *agent).is_some())
        .collect()
}

fn wire_one(
    home: &Home,
    agent: Agent,
    by: &str,
    guard: Guard,
    record: &mut Record,
    outcome: &mut Outcome,
) {
    if !home.is_present(agent) {
        outcome.skipped.push(AgentSkip {
            agent,
            reason: SkipReason::NotPresent,
        });
        return;
    }
    let plan = match plan_for(agent, home, Intent::Install) {
        Ok(plan) => plan,
        Err(error) => {
            outcome.errors.push(AgentError { agent, error });
            return;
        }
    };
    collect_warnings(agent, &plan, outcome);
    if let Some(reason) = plan.skipped {
        outcome.skipped.push(AgentSkip { agent, reason });
        return;
    }

    let was_wired = state::entry(record, agent).is_some();
    if plan.is_noop() && was_wired {
        outcome.current.push(agent);
        return;
    }
    // Read off the plan before it is applied: afterwards the files
    // exist and `config.toml` says whatever Roost put there.
    let wired = state::Wired {
        files: plan.files.clone(),
        created: plan
            .edits
            .iter()
            .filter(|edit| !edit.image.exists() && edit.after.is_some())
            .map(|edit| edit.image.path.clone())
            .collect(),
        codex_features_hooks: (agent == Agent::Codex).then(|| codex::observed_features_flag(home)),
    };
    if let Err(error) = apply(&plan, guard) {
        outcome.errors.push(AgentError { agent, error });
        return;
    }
    outcome.wrote |= !plan.edits.is_empty();
    state::set_wired(
        record,
        agent,
        INTEGRATION_VERSION,
        &wired,
        by,
        state::now_secs(),
    );
    if was_wired {
        outcome.refreshed.push(agent);
    } else {
        outcome.wired.push(agent);
    }
}

/// Every place this agent's entries could be: where it lives now, plus
/// every directory the state record's file list points at.
///
/// The second half is the plan's "and from every file the state record
/// names". A `CODEX_HOME` the user has since moved away from is
/// otherwise cleaned by nothing — `off` would plan against the current
/// path, find nothing, and then delete the only record of where the old
/// install was.
fn unwire_homes(home: &Home, agent: Agent, record: &Record) -> Vec<Home> {
    let mut dirs = vec![home.agent_dir(agent).to_path_buf()];
    if let Some(entry) = state::entry(record, agent) {
        for file in &entry.files {
            let Some(dir) = crate::agent_dir_of(agent, std::path::Path::new(file)) else {
                continue;
            };
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs.into_iter()
        .map(|dir| home.with_agent_dir(agent, dir))
        .collect()
}

fn unwire_one(home: &Home, agent: Agent, guard: Guard, record: &mut Record, outcome: &mut Outcome) {
    let mut removed_anything = false;
    // The record is only forgotten when every place it named came clean.
    // Dropping it after a skip or a failure is how a half-cleaned
    // machine becomes one nothing knows how to finish.
    let mut clean = true;

    for at in unwire_homes(home, agent, record) {
        let plan = match plan_for(agent, &at, Intent::Uninstall) {
            Ok(plan) => plan,
            Err(error) => {
                outcome.errors.push(AgentError { agent, error });
                clean = false;
                continue;
            }
        };
        if let Some(reason) = plan.skipped {
            outcome.skipped.push(AgentSkip { agent, reason });
            clean = false;
            continue;
        }
        if plan.is_noop() {
            continue;
        }
        if let Err(error) = apply(&plan, guard) {
            outcome.errors.push(AgentError { agent, error });
            clean = false;
            continue;
        }
        outcome.wrote = true;
        removed_anything = true;
    }

    if clean {
        record.remove(agent.source());
    }
    if removed_anything {
        outcome.removed.push(agent);
    }
}

fn collect_warnings(agent: Agent, plan: &InstallPlan, outcome: &mut Outcome) {
    for warning in &plan.warnings {
        outcome.warnings.push(AgentWarning {
            agent,
            warning: warning.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skip_list_resolves_names_and_keeps_the_ones_it_cannot() {
        let (known, unknown) = skip_list(["codex", "Cursor", "gemini", "codex"]);
        assert_eq!(known, vec![Agent::Codex, Agent::Cursor]);
        assert_eq!(unknown, vec!["gemini"]);
    }

    /// gx shares grok's file and reports as grok; it is not a name of
    /// its own, so skipping by it has to be visible rather than silent.
    #[test]
    fn a_name_no_agent_answers_to_is_unknown() {
        let (known, unknown) = skip_list(["gx"]);
        assert!(known.is_empty());
        assert_eq!(unknown, vec!["gx"]);
    }

    #[test]
    fn every_agent_can_be_skipped_by_the_name_status_prints() {
        let names: Vec<&str> = ALL_AGENTS.iter().map(|a| a.source()).collect();
        let (known, unknown) = skip_list(names);
        assert_eq!(known, ALL_AGENTS.to_vec());
        assert!(unknown.is_empty());
        for agent in ALL_AGENTS {
            assert!(agent_names().contains(agent.source()), "{}", agent.source());
        }
    }
}
