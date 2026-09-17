//! `ensure`, `reconcile`, `raise`, `install`, `uninstall`, `set_hooks`,
//! `status` — the things the callers actually do.
//!
//! Each writer takes the lock, plans every agent it is responsible for,
//! applies what it planned, and updates the record — in that order,
//! once, under one lock. Planning inside the lock is the point: an
//! atomic rename stops a torn file, but two ensures that both *read*
//! before either *wrote* would still lose one of the two writes.
//!
//! [`ConfigLock`] is **not re-entrant**: `flock` belongs to the open
//! file description, so a second acquire on the same path blocks even
//! inside this process. Every entry point here is therefore a thin
//! wrapper that takes the lock once and hands the guard to a `*_locked`
//! worker; nothing below that line locks again, and the key is written
//! through [`roost_ui_model::config::set_key_locked`], which will not
//! compile without a guard.
//!
//! # What decides policy, and what does not
//!
//! The wiring entries other than [`ensure`] take a [`Mode`] their caller
//! resolved — the UI, the CLI and a host session each hold it from
//! somewhere different. Everything that reads or writes the
//! `agent-hooks` key does so **inside the lock**, against
//! [`Home::config_path`]: the key and the files it authorises have to
//! move together, or an `install` could lose a concurrent `raise` and a
//! startup [`ensure`] could wire what an `uninstall` had just taken
//! back.

use roost_agent::{Agent, ALL_AGENTS};
use roost_ui_model::config::{AgentHooks, ConfigLock, RoostConfig};

use crate::command::INTEGRATION_VERSION;
use crate::error::{AgentError, AgentSkip, AgentWarning, InstallError, SkipReason};
use crate::home::Home;
use crate::plan::{apply, Guard, InstallPlan, Intent};
use crate::state::{self, Record};
use crate::{claude, codex, cursor, grok, opencode};

/// Which agents Roost may wire on this machine.
///
/// The resolved `agent-hooks` key, minus its third state: `Ask` never
/// reaches this crate, because "nobody has answered yet" is not an
/// instruction to write or to remove anything. Callers resolve it
/// (usually to "do nothing at all") before they get here — which is also
/// why there is no `Default`: there is no safe default, so the caller
/// has to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The agents the user consented to, in [`ALL_AGENTS`] order.
    Allow(Vec<Agent>),
    Off,
}

impl Mode {
    /// The engine's view of a parsed `agent-hooks` key. `None` is
    /// `Ask` — see the type's own doc.
    pub fn from_config(hooks: &AgentHooks) -> Option<Mode> {
        match hooks {
            AgentHooks::Allow { agents, .. } => Some(Mode::Allow(allowed_from(agents))),
            AgentHooks::Off => Some(Mode::Off),
            AgentHooks::Ask => None,
        }
    }

    /// What this mode spells in `config.conf`.
    ///
    /// An empty allow-list spells `off`, not an empty value: an empty
    /// value parses back as `Ask`, which would turn "the user switched
    /// every agent off" into "nobody has answered" and bring the consent
    /// dialog back on the next launch.
    pub fn to_config(&self) -> AgentHooks {
        match self {
            Mode::Allow(agents) if agents.is_empty() => AgentHooks::Off,
            Mode::Allow(agents) => AgentHooks::allow(agents.iter().map(|a| a.source())),
            Mode::Off => AgentHooks::Off,
        }
    }

    fn allows(&self, agent: Agent) -> bool {
        matches!(self, Mode::Allow(agents) if agents.contains(&agent))
    }

    /// Why an agent this mode does not name was left where it was.
    fn left_alone(&self) -> SkipReason {
        match self {
            Mode::Off => SkipReason::ModeOff,
            Mode::Allow(_) => SkipReason::NotAllowed,
        }
    }
}

/// Config names to agents, in [`ALL_AGENTS`] order.
///
/// The parser has already split the names it could not resolve out into
/// [`AgentHooks::unknown`], so there is nothing to drop here: this sees
/// only names that answer to an agent.
fn allowed_from(names: &[String]) -> Vec<Agent> {
    ALL_AGENTS
        .into_iter()
        .filter(|agent| names.iter().any(|name| name == agent.source()))
        .collect()
}

/// The agents `hooks` allows, in [`ALL_AGENTS`] order. `Off` and an
/// unanswered key both allow nothing.
fn allowed_in(hooks: &AgentHooks) -> Vec<Agent> {
    match hooks {
        AgentHooks::Allow { agents, .. } => allowed_from(agents),
        AgentHooks::Off | AgentHooks::Ask => Vec::new(),
    }
}

/// `hooks` rewritten to allow exactly `agents`, still carrying whatever
/// it said that this build cannot name.
///
/// Every key this crate writes is a read-modify-write, so a token
/// dropped here is a name erased off the user's disk — see
/// [`AgentHooks`]'s own doc.
fn allowing(hooks: &AgentHooks, agents: &[Agent]) -> AgentHooks {
    AgentHooks::Allow {
        agents: agents.iter().map(|a| a.source().to_string()).collect(),
        unknown: hooks.unknown().to_vec(),
    }
}

/// Who flips the state record's `noticed` flag for what a run reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Notice {
    /// The caller shows the toast and then calls [`crate::mark_noticed`].
    /// The order is deliberate on a machine with a screen: a crash
    /// between the two loses the toast rather than repeating it (plan
    /// 046 §3.3).
    #[default]
    Caller,
    /// This run flips it, in the same record write that recorded the
    /// wiring. A host has no screen and no second step — it reports
    /// `wired` in an op reply it cannot take back — so the flag has to
    /// move under the same lock that decided it. Flipping afterwards
    /// re-acquires the lock, and two clients connecting at once would
    /// both read the agent as unannounced and both be told (§3.3: "on a
    /// host the session flips `noticed` itself when it reports `wired`").
    Here,
}

/// Resolve a wire message's agent names against the agents this crate
/// can wire: the ones it recognises, and the spellings it does not.
///
/// The unknown half is returned rather than dropped because every
/// caller has something to say about it, and they do not agree on what:
/// the two wire ops report it as a `skipped` entry and act on the rest
/// (plan 065 §3.1), `roostctl agent set` refuses the whole list. What
/// none of them may do is drop it silently — that is a Roost that
/// behaves differently with nothing to say why.
pub fn resolve_names<'a>(names: impl IntoIterator<Item = &'a str>) -> (Vec<Agent>, Vec<String>) {
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

/// The agent names [`resolve_names`] accepts, for a caller's error
/// message.
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
///
/// Two independent sources, deliberately kept apart. [`Self::wired`] is
/// what Roost's own state record *claims*; [`Self::entries_on_disk`] and
/// [`Self::up_to_date`] are read off the agent's own files. They can
/// disagree — a wiped `~/.config/roost`, a record restored from a backup,
/// a hand-edited settings file — and a reader that trusts either one
/// alone reports a healthy install as broken or a broken one as healthy.
/// Callers render the disagreement; nothing here resolves it.
#[derive(Debug, Clone)]
pub struct Status {
    pub agent: Agent,
    pub present: bool,
    /// The integration version the record says is wired. `None` means
    /// the record has no entry — which is **not** the same as "nothing
    /// is wired"; that is [`Self::entries_on_disk`].
    pub wired: Option<u32>,
    /// Whether the agent's own config actually carries a Roost entry
    /// right now, at any integration version.
    ///
    /// Derived from an *uninstall* plan: it has edits iff there is
    /// something of Roost's in those files to take back out. The install
    /// plan cannot answer this — it is empty only when the entries are
    /// present **and current**, so an older version's entry looks
    /// identical to no entry at all.
    pub entries_on_disk: bool,
    /// True when a fresh `plan` would make no edits.
    pub up_to_date: bool,
    pub noticed: bool,
    /// Whether the resolved `agent-hooks` key names this agent. A caller
    /// holding `Ask` — nobody has answered the consent dialog — passes
    /// `Mode::Allow(vec![])`: nothing is allowed until the user says so.
    pub allowed: bool,
    /// The files this agent's install owns or merges into — what the Mac
    /// consent sheet names before the user answers, and what an
    /// uninstall touches. Two of them for codex.
    pub files: Vec<std::path::PathBuf>,
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
pub fn plan(agent: Agent, home: &Home, mode: &Mode) -> Result<InstallPlan, InstallError> {
    match mode {
        Mode::Off => plan_for(agent, home, Intent::Uninstall),
        Mode::Allow(_) if !mode.allows(agent) => Ok(InstallPlan::skip(
            agent,
            Intent::Install,
            SkipReason::NotAllowed,
        )),
        Mode::Allow(_) if !home.is_present(agent) => Ok(InstallPlan::skip(
            agent,
            Intent::Install,
            SkipReason::NotPresent,
        )),
        Mode::Allow(_) => plan_for(agent, home, Intent::Install),
    }
}

/// What the UIs run at startup: wire and refresh what the user allowed,
/// and **never take anything out**.
///
/// The no-unwire half is load-bearing (plan 046 C7, plan 064 §3.2). A
/// startup that reconciled downward would strip a developer's real
/// entries the moment an e2e lane ran without `ROOST_TEST_MODE`, and it
/// would turn "I have not said yes yet" into a removal nobody asked for.
/// Under `agent-hooks = off` it therefore writes nothing at all and
/// reports every agent as [`SkipReason::ModeOff`]; [`reconcile`] is the
/// explicit verb that does take entries back out.
///
/// **It reads the key itself, inside the lock**, rather than taking a
/// [`Mode`] its caller resolved. Every caller here is a *launch*, and a
/// launch reads config, opens a window and only then gets to this — a
/// window wide enough for `roostctl agent uninstall` to have answered
/// the key in between, whose consent this would then re-wire. Resolving
/// under the lock means the answer acted on is the answer on disk. An
/// unanswered key (`ask`) is not an instruction to write anything, so it
/// returns an empty outcome.
pub fn ensure(home: &Home, by: &str, guard: Guard) -> Result<Outcome, InstallError> {
    guard.check()?;
    let lock = home.config_lock()?;
    let Some(mode) = Mode::from_config(&read_hooks(home)?) else {
        return Ok(Outcome::default());
    };
    wire_locked(home, &mode, by, guard, Notice::Caller, &lock)
}

/// Bring this machine's files in line with `mode`, both ways.
///
/// `Allow` wires and refreshes what it names and **unwires what it does
/// not** — for every agent present on disk or named in the record, so a
/// machine whose `~/.config/roost` was wiped still comes clean. `Off`
/// unwires all of them. With nothing to remove it writes nothing.
///
/// What `roostctl agent ensure` and the consent dialog's Apply run. The
/// difference from [`ensure`] is the one sentence above: this is the
/// path a hand-lowered `agent-hooks` key gets reconciled downward on.
pub fn reconcile(
    home: &Home,
    mode: &Mode,
    by: &str,
    guard: Guard,
) -> Result<Outcome, InstallError> {
    guard.check()?;
    let lock = home.config_lock()?;
    reconcile_locked(home, mode, by, guard, &lock)
}

/// Union `agents` into this home's `agent-hooks` key — never removing —
/// then wire what it now allows.
///
/// What a connecting client is allowed to do to a host (plan 064 §3.3).
/// A raise is additive against whatever the key already says, `off` and
/// `ask` included: both become the client's list, because a host has no
/// screen to ask on and the client in front of the user is the only
/// authority there is. It never removes, so a second client — or an
/// agent somebody wired by hand — survives a raise it was not named in.
///
/// One thing differs from [`ensure`], and it follows from the caller
/// being remote: the run flips `noticed` itself, in the same record
/// write — see [`Notice::Here`].
pub fn raise(
    home: &Home,
    agents: &[Agent],
    by: &str,
    guard: Guard,
) -> Result<Outcome, InstallError> {
    guard.check()?;
    let lock = home.config_lock()?;
    let (mode, wrote) = widen(home, agents, &lock)?;
    let mut outcome = wire_locked(home, &mode, by, guard, Notice::Here, &lock)?;
    outcome.wrote |= wrote;
    Ok(outcome)
}

/// Set the `agent-hooks` key to exactly `mode`, then bring this
/// machine's files in line with it.
///
/// The key is written **first and under the same lock**: a partial
/// reconcile leaves the permission durable (a later `roostctl agent
/// ensure` retries what failed), where a key written afterwards would be
/// lost by every failure and by any concurrent [`raise`]. A key that
/// cannot be written is an error and nothing else happens — no agent
/// file is touched.
pub fn set_hooks(
    home: &Home,
    mode: &Mode,
    by: &str,
    guard: Guard,
) -> Result<Outcome, InstallError> {
    guard.check()?;
    let lock = home.config_lock()?;
    // `mode.to_config()` and not a union: this op *replaces* the key, so
    // a token this build cannot name goes with the rest of the old value.
    // That is the difference between an explicit local answer and a
    // raise — the user is looking at the list they just chose.
    let wrote = write_hooks(home, &read_hooks(home)?, &mode.to_config(), &lock)?;
    let mut outcome = reconcile_locked(home, mode, by, guard, &lock)?;
    outcome.wrote |= wrote;
    Ok(outcome)
}

fn wire_locked(
    home: &Home,
    mode: &Mode,
    by: &str,
    guard: Guard,
    notice: Notice,
    lock: &ConfigLock,
) -> Result<Outcome, InstallError> {
    let (mut record, mut outcome) = start(home)?;
    for agent in ALL_AGENTS {
        if mode.allows(agent) {
            wire_one(home, agent, by, guard, &mut record, &mut outcome);
        } else {
            outcome.skipped.push(AgentSkip {
                agent,
                reason: mode.left_alone(),
            });
        }
    }
    finish(home, record, outcome, notice, lock)
}

fn reconcile_locked(
    home: &Home,
    mode: &Mode,
    by: &str,
    guard: Guard,
    lock: &ConfigLock,
) -> Result<Outcome, InstallError> {
    let (mut record, mut outcome) = start(home)?;
    let targets = unwire_targets(home, &record);
    for agent in ALL_AGENTS {
        if mode.allows(agent) {
            wire_one(home, agent, by, guard, &mut record, &mut outcome);
        } else if targets.contains(&agent) {
            unwire_one(home, agent, guard, &mut record, &mut outcome);
        } else {
            outcome.skipped.push(AgentSkip {
                agent,
                reason: mode.left_alone(),
            });
        }
    }
    finish(home, record, outcome, Notice::Caller, lock)
}

/// The record, plus an [`Outcome`] carrying anything reading it had to
/// say.
fn start(home: &Home) -> Result<(Record, Outcome), InstallError> {
    let (record, warning) = state::load(home)?;
    let mut outcome = Outcome::default();
    if let Some(warning) = warning {
        // Not attributable to one agent; the first one carries it so the
        // caller's rendering has somewhere to put it.
        outcome.warnings.push(AgentWarning {
            agent: ALL_AGENTS[0],
            warning,
        });
    }
    Ok((record, outcome))
}

/// The toast list, the `noticed` flip that goes with it, and the record
/// write that makes both durable — in one place, so every entry point
/// spends a notice the same way.
fn finish(
    home: &Home,
    mut record: Record,
    mut outcome: Outcome,
    notice: Notice,
    _lock: &ConfigLock,
) -> Result<Outcome, InstallError> {
    outcome.unnoticed = unnoticed(&record);
    if notice == Notice::Here {
        for agent in &outcome.unnoticed {
            if let Some(entry) = record.get_mut(agent.source()) {
                entry.noticed = true;
            }
        }
    }
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

/// Wire exactly these agents, and say so in the `agent-hooks` key.
///
/// `agent install <name>` is an explicit instruction, and explicit wins:
/// a user who has turned `agent-hooks` off and then asks for one agent
/// gets that agent — and gets the key unioned with it, so the next
/// startup [`ensure`] does not treat what they just asked for as
/// unconsented. The union never removes, exactly as [`raise`]'s does.
pub fn install(
    home: &Home,
    agents: &[Agent],
    by: &str,
    guard: Guard,
) -> Result<Outcome, InstallError> {
    guard.check()?;
    let lock = home.config_lock()?;
    let (_, wrote) = widen(home, agents, &lock)?;
    let (mut record, mut outcome) = start(home)?;
    outcome.wrote |= wrote;
    for agent in agents {
        wire_one(home, *agent, by, guard, &mut record, &mut outcome);
    }
    finish(home, record, outcome, Notice::Caller, &lock)
}

/// Take Roost's entries back out of exactly these agents, and narrow the
/// `agent-hooks` key to match.
///
/// Narrowing is the mirror of [`install`]'s union, with one asymmetry:
/// an unanswered key (`ask`) is left unanswered rather than rewritten as
/// "everything except this one", which would be a consent the user never
/// gave. Uninstalling *every* agent is the exception — that is an answer,
/// and it is written as `off` so the dialog does not come back asking
/// again.
pub fn uninstall(home: &Home, agents: &[Agent], guard: Guard) -> Result<Outcome, InstallError> {
    guard.check()?;
    let lock = home.config_lock()?;
    let current = read_hooks(home)?;
    let wrote = match narrowed(&current, agents) {
        Some(narrowed) => write_hooks(home, &current, &narrowed, &lock)?,
        None => false,
    };
    let (mut record, mut outcome) = start(home)?;
    outcome.wrote |= wrote;
    for agent in agents {
        unwire_one(home, *agent, guard, &mut record, &mut outcome);
    }
    finish(home, record, outcome, Notice::Caller, &lock)
}

/// Read-only: what `roostctl agent status` and doctor render.
pub fn status(home: &Home, mode: &Mode) -> Result<Vec<Status>, InstallError> {
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
            // The disk half of the answer. Skipped either way when the
            // agent is absent: there are no files to read.
            let removal = if present {
                Some(plan_for(agent, home, Intent::Uninstall)?)
            } else {
                None
            };
            Ok(Status {
                agent,
                present,
                wired: entry.map(|e| e.integration_version),
                entries_on_disk: removal.is_some_and(|p| !p.is_noop()),
                up_to_date: plan.as_ref().is_some_and(InstallPlan::is_noop),
                noticed: entry.is_some_and(|e| e.noticed),
                allowed: mode.allows(agent),
                files: crate::owned_files(home, agent),
                skipped: match &plan {
                    Some(plan) => plan.skipped.clone(),
                    None => Some(SkipReason::NotPresent),
                },
                warnings: plan.map(|p| p.warnings).unwrap_or_default(),
            })
        })
        .collect()
}

/// This home's `agent-hooks` key, through the same parser every reader
/// uses.
///
/// An absent `config.conf` is `Ask`, like an absent key. Anything else
/// the filesystem says is returned: a config Roost cannot read is a
/// config it must not overwrite, because the union it would compute is
/// a guess at what the user consented to.
fn read_hooks(home: &Home) -> Result<AgentHooks, InstallError> {
    let path = home.config_path();
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(RoostConfig::parse(&text).agent_hooks),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AgentHooks::Ask),
        Err(e) => Err(InstallError::io(path, e)),
    }
}

/// Write `desired` into the `agent-hooks` key — unless `current`, the
/// key as it stands, already says exactly that, because this runs on
/// every host connect and rewriting the user's `config.conf` to the
/// bytes it already holds is churn they would see in a dotfile diff.
///
/// `current` is a parameter rather than a [`read_hooks`] call because
/// every caller has just read it to work out `desired`, and re-parsing
/// repeats every warning the config's other keys emit.
fn write_hooks(
    home: &Home,
    current: &AgentHooks,
    desired: &AgentHooks,
    lock: &ConfigLock,
) -> Result<bool, InstallError> {
    if current == desired {
        return Ok(false);
    }
    let path = home.config_path();
    let value = desired
        .to_config_value()
        .expect("a Mode is never Ask, and only Ask has no config value");
    roost_ui_model::config::set_key_locked(lock, path, "agent-hooks", &value)
        .map_err(|e| InstallError::io(path, e))?;
    Ok(true)
}

/// The `agent-hooks` key widened by `agents` and written back — the
/// shared half of [`raise`] and [`install`], and the reason both are
/// additive.
fn widen(home: &Home, agents: &[Agent], lock: &ConfigLock) -> Result<(Mode, bool), InstallError> {
    let current = read_hooks(home)?;
    let widened = union(&current, agents);
    // A union that allows nothing writes nothing. Widening is the only
    // thing these two callers may do to the key, and an empty union can
    // only mean the key allowed nothing and nothing was asked for —
    // writing then would spell that as `off`, which is a *lowering* of
    // an unanswered key nobody requested.
    let allowed = allowed_in(&widened);
    let wrote = if allowed.is_empty() {
        false
    } else {
        write_hooks(home, &current, &widened, lock)?
    };
    Ok((Mode::Allow(allowed), wrote))
}

/// `hooks` widened by `agents`, in [`ALL_AGENTS`] order.
///
/// `Off` and `Ask` both widen to exactly `agents` (plan 064 §3.3): a
/// client that says "wire claude" on a host whose key says `off` is
/// answering the question the host cannot ask, and the alternative —
/// treating `off` as a veto no remote client can lift — leaves that
/// client with no way to say yes at all.
fn union(hooks: &AgentHooks, agents: &[Agent]) -> AgentHooks {
    let current = allowed_in(hooks);
    let widened: Vec<Agent> = ALL_AGENTS
        .into_iter()
        .filter(|agent| current.contains(agent) || agents.contains(agent))
        .collect();
    if widened.is_empty() {
        return AgentHooks::Ask;
    }
    allowing(hooks, &widened)
}

/// `hooks` with `agents` taken out of it, or `None` to leave the key
/// exactly as it is. See [`uninstall`] for why `ask` survives a partial
/// uninstall and not a total one.
///
/// A *partial* narrowing keeps what this build cannot name — the user
/// took one agent out, not every agent a newer Roost knows. A **total**
/// one drops those names with the rest: an `off` key carrying names
/// beside it would not parse back as `off`, and "take every agent out"
/// is an answer about all of them.
fn narrowed(hooks: &AgentHooks, agents: &[Agent]) -> Option<AgentHooks> {
    if ALL_AGENTS.iter().all(|agent| agents.contains(agent)) {
        return Some(AgentHooks::Off);
    }
    let AgentHooks::Allow {
        agents: names,
        unknown,
    } = hooks
    else {
        return None;
    };
    let kept: Vec<Agent> = allowed_from(names)
        .into_iter()
        .filter(|agent| !agents.contains(agent))
        .collect();
    if !kept.is_empty() {
        return Some(allowing(hooks, &kept));
    }
    if unknown.is_empty() {
        return Some(AgentHooks::Off);
    }
    // The last name this build knows is gone, but the user asked about
    // *that* agent — not about the one a newer Roost put here — so the
    // unknown names are written on their own rather than replaced by
    // `off`, which would erase them (#486's data loss, one layer down).
    //
    // The cost is deliberate: an all-unknown value parses back as `Ask`,
    // not `Allow`, so the consent dialog raises again. That is the honest
    // reading of the state it describes — this build now allows nothing
    // and is holding a name it cannot wire — and a newer build that
    // knows the name still finds it.
    Some(AgentHooks::Allow {
        agents: Vec::new(),
        unknown: unknown.clone(),
    })
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

/// Answer this home's `agent-hooks` key on disk, the way the consent
/// dialog or `roostctl agent set --local` would.
///
/// Shared with [`crate::acceptance`]: [`ensure`] resolves the key itself,
/// inside the lock, so a case that wants a startup ensure to wire
/// something has to say so where it is written rather than hand over a
/// mode.
#[cfg(test)]
pub(crate) fn answer(home: &Home, mode: &Mode) {
    let value = mode
        .to_config()
        .to_config_value()
        .expect("a Mode always has a config value");
    roost_ui_model::config::set_key(home.config_path(), "agent-hooks", &value).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_resolver_keeps_the_ones_it_cannot_resolve() {
        let (known, unknown) = resolve_names(["codex", "Cursor", "gemini", "codex"]);
        assert_eq!(known, vec![Agent::Codex, Agent::Cursor]);
        assert_eq!(unknown, vec!["gemini"]);
    }

    /// gx shares grok's file and reports as grok; it is not a name of
    /// its own, so naming it has to be visible rather than silent.
    #[test]
    fn a_name_no_agent_answers_to_is_unknown() {
        let (known, unknown) = resolve_names(["gx"]);
        assert!(known.is_empty());
        assert_eq!(unknown, vec!["gx"]);
    }

    fn a_home(root: &std::path::Path) -> Home {
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        Home::rooted(root)
    }

    fn all() -> Mode {
        Mode::Allow(ALL_AGENTS.to_vec())
    }

    /// A raise wires, records, and reports exactly as a local `ensure`
    /// does — and says in the key what it was allowed to do.
    #[test]
    fn a_raise_wires_exactly_as_ensure_does() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let done = raise(&home, &[Agent::Claude], "remote", Guard::PERMITTED).expect("raise");
        assert_eq!(done.wired, vec![Agent::Claude]);
        assert_eq!(done.unnoticed, vec![Agent::Claude]);
        assert_eq!(read_hooks(&home).unwrap(), AgentHooks::allow(["claude"]));
    }

    /// The `noticed` flip happens in the run's own record write, so the
    /// agent is announced to exactly one caller.
    ///
    /// Flipping it afterwards — a second `mark_noticed` under a
    /// re-acquired lock — leaves a window in which two clients connecting
    /// at once both read the agent as unannounced and both get told that
    /// Roost changed the user's files. The local UI keeps the
    /// show-then-mark order on purpose (a crash there should lose the
    /// toast, not repeat it); a host has no toast to lose.
    #[test]
    fn a_raise_spends_the_notice_in_the_same_write() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        let first = raise(&home, &ALL_AGENTS, "remote", Guard::PERMITTED).expect("raise");
        assert_eq!(first.unnoticed, vec![Agent::Claude]);
        // Read straight off the record: no `mark_noticed` ran in between,
        // which is exactly the point.
        let recorded = std::fs::read_to_string(dir.path().join(".config/roost/agent-hooks.json"))
            .expect("state record");
        assert!(recorded.contains("\"noticed\": true"), "{recorded}");

        let second = raise(&home, &ALL_AGENTS, "remote", Guard::PERMITTED).expect("raise again");
        assert!(second.unnoticed.is_empty(), "{second:?}");
    }

    /// …and a local `ensure` still leaves the flag for its caller to
    /// spend, because the toast has to be on screen before it is.
    #[test]
    fn a_local_ensure_leaves_the_notice_for_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());

        answer(&home, &all());
        let first = ensure(&home, "local", Guard::PERMITTED).expect("ensure");
        assert_eq!(first.unnoticed, vec![Agent::Claude]);
        let second = ensure(&home, "local", Guard::PERMITTED).expect("ensure again");
        assert_eq!(
            second.unnoticed,
            vec![Agent::Claude],
            "nothing but mark_noticed may spend a local toast"
        );
    }

    /// `ensure` is the startup path, and startup never unwires (plan 046
    /// C7): an agent that has dropped off the allow-list keeps its
    /// entries until something explicit takes them out.
    #[test]
    fn a_startup_ensure_leaves_an_agent_the_key_no_longer_names() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        install(&home, &[Agent::Claude], "local", Guard::PERMITTED).expect("install");

        answer(&home, &Mode::Allow(vec![Agent::Codex]));
        let narrowed = ensure(&home, "local", Guard::PERMITTED).expect("ensure");
        assert!(narrowed.removed.is_empty(), "{narrowed:?}");
        assert!(
            std::fs::read_to_string(dir.path().join(".claude/settings.json"))
                .unwrap()
                .contains("ROOST_AGENT_HOOK")
        );

        // `off` is the same promise, stated harder: nothing at all.
        answer(&home, &Mode::Off);
        let off = ensure(&home, "local", Guard::PERMITTED).expect("ensure off");
        assert!(off.removed.is_empty(), "{off:?}");
        assert!(!off.wrote, "{off:?}");
        assert_eq!(off.skipped.len(), ALL_AGENTS.len());
        assert!(off
            .skipped
            .iter()
            .all(|skip| matches!(skip.reason, SkipReason::ModeOff)));
    }

    /// #487 finding 2: a startup ensure wires the key **as it is when it
    /// runs**, not as the launch read it.
    ///
    /// The window is real — config is read at launch, the ensure runs
    /// once a window is open — and `roostctl agent uninstall claude`
    /// lands inside it. Resolving under the lock is what makes the
    /// uninstall stick; a mode carried from the launch would put back
    /// what the user had just taken out, with nothing on screen to say
    /// so.
    #[test]
    fn a_startup_ensure_wires_the_key_as_it_stands_when_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        let settings = dir.path().join(".claude/settings.json");
        install(&home, &[Agent::Claude], "local", Guard::PERMITTED).expect("install");
        assert!(std::fs::read_to_string(&settings)
            .unwrap()
            .contains("ROOST_AGENT_HOOK"));

        // The launch read `claude`; the user then takes it back out.
        uninstall(&home, &[Agent::Claude], Guard::PERMITTED).expect("uninstall");

        let outcome = ensure(&home, "local", Guard::PERMITTED).expect("ensure");
        assert!(outcome.wired.is_empty(), "{outcome:?}");
        // Absent counts: an uninstall removes a file Roost created.
        assert!(
            !std::fs::read_to_string(&settings)
                .unwrap_or_default()
                .contains("ROOST_AGENT_HOOK"),
            "the startup ensure re-wired what an uninstall had just removed"
        );
    }

    /// The union is what makes a raise additive. `off` and `ask` are the
    /// contentious half of plan 064 §3.3: both become the client's list.
    #[test]
    fn a_union_never_removes_and_answers_for_off_and_ask() {
        let allowed = AgentHooks::allow(["grok"]);
        assert_eq!(
            union(&allowed, &[Agent::Claude]),
            AgentHooks::allow(["claude", "grok"]),
            "a raise removed what it was not asked about"
        );
        for unanswered in [AgentHooks::Off, AgentHooks::Ask] {
            assert_eq!(
                union(&unanswered, &[Agent::Codex]),
                AgentHooks::allow(["codex"])
            );
        }
    }

    /// A raise computed off a key naming an agent this build predates
    /// keeps that name. Without this the older of two Roosts silently
    /// erases the newer one's answer every time a client connects.
    #[test]
    fn a_union_carries_a_name_this_build_cannot_wire() {
        let newer = AgentHooks::Allow {
            agents: vec!["claude".into()],
            unknown: vec!["gemini".into()],
        };
        assert_eq!(
            union(&newer, &[Agent::Codex]).to_config_value().unwrap(),
            "claude, codex, gemini"
        );
    }

    /// The mirror: an uninstall narrows an allow-list, leaves an
    /// unanswered key unanswered, and treats "all of them" as an answer.
    #[test]
    fn narrowing_leaves_an_unanswered_key_alone() {
        let allowed = AgentHooks::allow(["claude", "codex"]);
        assert_eq!(
            narrowed(&allowed, &[Agent::Codex]),
            Some(AgentHooks::allow(["claude"]))
        );
        assert_eq!(
            narrowed(&allowed, &[Agent::Claude, Agent::Codex]),
            Some(AgentHooks::Off)
        );
        assert_eq!(narrowed(&AgentHooks::Ask, &[Agent::Codex]), None);
        assert_eq!(narrowed(&AgentHooks::Off, &[Agent::Codex]), None);
        assert_eq!(
            narrowed(&AgentHooks::Ask, &ALL_AGENTS),
            Some(AgentHooks::Off)
        );
    }

    /// #486's data loss, one layer down: taking the last name this build
    /// *can* wire out of `claude, gemini` must not take `gemini` with it.
    /// Naming every agent is the one uninstall that does.
    #[test]
    fn a_partial_uninstall_keeps_a_name_this_build_cannot_wire() {
        let newer = AgentHooks::Allow {
            agents: vec!["claude".into()],
            unknown: vec!["gemini".into()],
        };
        assert_eq!(
            narrowed(&newer, &[Agent::Claude])
                .and_then(|hooks| hooks.to_config_value())
                .as_deref(),
            Some("gemini"),
            "the uninstall erased a name it was not asked about"
        );
        assert_eq!(narrowed(&newer, &ALL_AGENTS), Some(AgentHooks::Off));
    }

    /// The same, through the verb the user types, ending on disk.
    #[test]
    fn an_uninstall_leaves_a_name_this_build_cannot_wire_in_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        roost_ui_model::config::set_key(home.config_path(), "agent-hooks", "claude, gemini")
            .unwrap();
        install(&home, &[Agent::Claude], "local", Guard::PERMITTED).expect("install");

        uninstall(&home, &[Agent::Claude], Guard::PERMITTED).expect("uninstall");

        let text = std::fs::read_to_string(home.config_path()).expect("config");
        assert!(text.contains("agent-hooks = gemini"), "{text}");
    }

    fn claude_row(home: &Home, mode: &Mode) -> Status {
        status(home, mode)
            .expect("status")
            .into_iter()
            .find(|row| row.agent == Agent::Claude)
            .expect("claude row")
    }

    /// The record and the agent's own files are two sources, and
    /// `status` has to answer for both. Wiring the entries and then
    /// deleting `~/.config/roost/agent-hooks.json` leaves a machine that
    /// is *fully wired* — anything reading only the record calls it "not
    /// wired" and sends the user to reinstall something already there.
    #[test]
    fn a_lost_state_record_does_not_make_installed_entries_disappear() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        install(&home, &[Agent::Claude], "local", Guard::PERMITTED).expect("install");

        let wired = claude_row(&home, &all());
        assert_eq!(wired.wired, Some(crate::command::INTEGRATION_VERSION));
        assert!(wired.entries_on_disk);
        assert!(wired.up_to_date);

        std::fs::remove_file(dir.path().join(".config/roost/agent-hooks.json")).unwrap();

        let orphaned = claude_row(&home, &all());
        assert_eq!(orphaned.wired, None, "the record really is gone");
        assert!(
            orphaned.entries_on_disk,
            "the entries are still in ~/.claude/settings.json"
        );
        assert!(orphaned.up_to_date);
    }

    /// The mirror image: a record that survives the file it describes.
    /// `wired` still says v2 because that is what the record claims —
    /// `entries_on_disk` is what says the claim is empty.
    #[test]
    fn a_record_that_outlives_the_entries_reports_nothing_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        install(&home, &[Agent::Claude], "local", Guard::PERMITTED).expect("install");
        std::fs::write(dir.path().join(".claude/settings.json"), "{}\n").unwrap();

        let row = claude_row(&home, &all());
        assert_eq!(row.wired, Some(crate::command::INTEGRATION_VERSION));
        assert!(!row.entries_on_disk, "the settings file was emptied");
        assert!(!row.up_to_date);
    }

    /// An older integration version's entry is still *wired* — it is the
    /// install plan that is non-empty, not the file. Deriving "wired"
    /// from `up_to_date` would report v1 entries as absent.
    #[test]
    fn an_older_versions_entry_still_counts_as_wired_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let home = a_home(dir.path());
        install(&home, &[Agent::Claude], "local", Guard::PERMITTED).expect("install");

        // The commands are JSON string values, so the file spells their
        // inner quotes escaped.
        let escaped = |s: &str| s.replace('"', "\\\"");
        let settings = dir.path().join(".claude/settings.json");
        let v1 = crate::command::owned_commands(Agent::Claude)
            .last()
            .unwrap()
            .clone();
        let before = std::fs::read_to_string(&settings).unwrap();
        let after = before.replace(
            &escaped(&crate::command::installed_command(Agent::Claude)),
            &escaped(&v1),
        );
        assert_ne!(before, after, "the v1 rewrite matched nothing");
        std::fs::write(&settings, after).unwrap();

        let row = claude_row(&home, &all());
        assert!(row.entries_on_disk, "a v1 entry is still an entry");
        assert!(!row.up_to_date, "but it is not current");
    }

    #[test]
    fn every_agent_resolves_by_the_name_status_prints() {
        let names: Vec<&str> = ALL_AGENTS.iter().map(|a| a.source()).collect();
        let (known, unknown) = resolve_names(names);
        assert_eq!(known, ALL_AGENTS.to_vec());
        assert!(unknown.is_empty());
        for agent in ALL_AGENTS {
            assert!(agent_names().contains(agent.source()), "{}", agent.source());
        }
    }

    /// The config key and the engine's mode are one decision in two
    /// spellings; `Ask` is the third state that never reaches the engine.
    #[test]
    fn a_mode_round_trips_through_the_config_key() {
        for mode in [Mode::Allow(vec![Agent::Codex, Agent::Grok]), Mode::Off] {
            assert_eq!(Mode::from_config(&mode.to_config()), Some(mode));
        }
        assert_eq!(Mode::from_config(&AgentHooks::Ask), None);
        // A name from a newer Roost allows nothing here — this build has
        // no file to write for it — but it stays in the key it came from.
        assert_eq!(
            Mode::from_config(&AgentHooks::Allow {
                agents: vec!["codex".into()],
                unknown: vec!["gemini".into()],
            }),
            Some(Mode::Allow(vec![Agent::Codex]))
        );
    }
}
