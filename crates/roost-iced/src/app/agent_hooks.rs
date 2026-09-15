//! Wiring the coding agents' hook entries at startup, and saying so
//! once (plan 046 §3.7).
//!
//! # Why none of this runs on the UI thread
//!
//! [`roost_agent_install::ensure`] reads and writes files in the user's
//! home, takes an advisory `flock` across the whole plan+apply, and can
//! block on another Roost — the CLI, the Swift app, a host connect —
//! doing the same. Iced's `update`/`view` is the winit event-loop
//! thread; a `flock` taken there freezes the window. So the work goes to
//! the runtime's **blocking** pool and the answer comes back on the
//! engine feed, which is the same road every other off-thread result in
//! this app travels.
//!
//! # Why the toast is at most once
//!
//! `ensure` returns the agents the **state record** says are wired and
//! have never been announced — `Outcome::unnoticed`, not the agents this
//! particular run wired. That is what makes the sentence a property of
//! the machine: a wiring done by `roostctl` or by the Mac app (which has
//! no transient status surface and leaves `noticed` false on purpose)
//! still gets said here, once, on the first launch that can say it.
//!
//! The order is deliberate: toast first, then
//! [`roost_agent_install::mark_noticed`]. A crash in between loses the
//! toast rather than repeating it, which is the right way round for a
//! line that says Roost changed the user's files — and because the flag
//! is what drives the toast, a `mark_noticed` that never lands is simply
//! a toast the next launch shows instead.

use roost_agent::Agent;
use roost_agent_install::{Guard, Home, Mode};
use roost_ipc::messages::{
    AgentHooksFailed, AgentHooksOutcome, AgentHooksSkipped, AgentSetHooksAgents,
};
use roost_ui_model::config::{AgentHooks, RoostConfig};

use crate::engine_feed::{EngineFeed, EngineFeedSender};
use crate::host_conn::queue::HostOpError;

/// How this UI identifies itself in the state record. The same label
/// `roostctl` writes: both are this machine acting on its own behalf,
/// and the field exists to distinguish *that* from a remote client.
const BY: &str = "local";

/// What one background `ensure` had to say, as the UI needs it.
#[derive(Debug, Default)]
pub(crate) struct AgentHooksEnsured {
    /// Agents the record says are wired and unannounced — the toast
    /// list, and the list `mark_noticed` is then given.
    pub unnoticed: Vec<Agent>,
    /// One line per agent that could not be wired. Rendered nowhere:
    /// `roostctl agent status` and doctor are the durable surface, so
    /// these are logged at the drain and left there.
    pub errors: Vec<String>,
}

/// What `config.conf` asks for, or `None` when nobody has answered the
/// consent dialog yet (plan 064) — the caller must wire and unwire
/// nothing in that case, not guess.
fn resolve(config: &RoostConfig) -> Option<Mode> {
    Mode::from_config(&config.agent_hooks)
}

/// What a `window_opened` should do about the startup ensure.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Start {
    Run,
    /// `agent-hooks = off`.
    Off,
    /// Nobody has answered the consent dialog yet (plan 064) — the same
    /// non-decision as `Off` for the purposes of this latch, but a
    /// distinct reason worth telling apart in a log line.
    Ask,
    /// Already run once in this process.
    Already,
}

/// Claim the one startup ensure this process gets.
///
/// `window_opened` is not once: iced routes **every** focus change
/// through it, unfocus included, so an Alt-Tab would otherwise start a
/// fresh five-agent ensure — and a user who had just removed a hook by
/// hand would watch Roost put it back for the crime of clicking on the
/// window. The plan says startup, so the latch says startup: it is
/// claimed only when the ensure actually starts, which leaves the `off`
/// and `ask` cases free to be reconsidered if a later launch ever
/// re-reads config.
///
/// `resolved` is `None` for `Ask` — [`resolve`]'s shape, carried through
/// rather than re-derived, so this function cannot itself decide to run
/// an ensure `resolve` said not to.
pub(crate) fn claim_start(started: &mut bool, resolved: Option<&Mode>) -> Start {
    match resolved {
        None => Start::Ask,
        Some(Mode::Off) => Start::Off,
        Some(Mode::Allow(_)) => {
            if *started {
                return Start::Already;
            }
            *started = true;
            Start::Run
        }
    }
}

/// Start the startup ensure, or decline to.
///
/// `agent-hooks = off` returns without touching a single file — the key
/// means "Roost wires nothing here", and a startup that *removed*
/// entries would make an opt-out into an action the user did not ask
/// for. `roostctl agent ensure` is the explicit verb that reads the same
/// `off` and takes them back out.
pub(crate) fn spawn_ensure(
    started: &mut bool,
    runtime: &tokio::runtime::Handle,
    feed: &EngineFeedSender,
    config: &RoostConfig,
) {
    let resolved = resolve(config);
    let mode = match claim_start(started, resolved.as_ref()) {
        Start::Run => resolved.expect("Start::Run implies resolve() returned Some"),
        Start::Off => {
            tracing::debug!("agent-hooks = off: not wiring agent hooks");
            return;
        }
        Start::Ask => {
            tracing::debug!("agent-hooks is not configured; not wiring agent hooks");
            return;
        }
        Start::Already => return,
    };

    let feed = feed.clone();
    let guard = Guard::from_env();
    // The resolved values ride the closure rather than being re-read
    // over there: a config edit landing mid-launch would otherwise split
    // the decision from the action it authorised.
    runtime.spawn_blocking(move || {
        feed.send(EngineFeed::AgentHooks(ensure_blocking(&mode, guard)));
    });
}

/// The blocking half. Every failure becomes a line in
/// [`AgentHooksEnsured::errors`] rather than a panic or a swallow: this
/// runs with nobody waiting on it, so the only honest thing to do with a
/// failure is carry it back to a thread that can log it.
fn ensure_blocking(mode: &Mode, guard: Guard) -> AgentHooksEnsured {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(error) => {
            return AgentHooksEnsured {
                unnoticed: Vec::new(),
                errors: vec![error.to_string()],
            }
        }
    };
    match roost_agent_install::ensure(&home, mode, BY, guard) {
        Ok(outcome) => AgentHooksEnsured {
            unnoticed: outcome.unnoticed,
            errors: outcome
                .errors
                .iter()
                .map(|e| format!("{}: {}", e.agent.source(), e.error))
                .collect(),
        },
        Err(error) => AgentHooksEnsured {
            unnoticed: Vec::new(),
            errors: vec![error.to_string()],
        },
    }
}

/// Flip `noticed` for the agents the toast just named, off the UI
/// thread. Failing to record it costs one repeated toast on the next
/// launch — the flag *is* the toast list, so the repeat is the retry —
/// and so it is logged and dropped rather than retried here.
pub(crate) fn spawn_mark_noticed(runtime: &tokio::runtime::Handle, agents: Vec<Agent>) {
    if agents.is_empty() {
        return;
    }
    runtime.spawn_blocking(move || {
        let result = Home::from_env()
            .and_then(|home| roost_agent_install::mark_noticed(&home, &agents).map(|_| ()));
        if let Err(error) = result {
            tracing::warn!(%error, "could not record that the agent-hooks toast was shown");
        }
    });
}

/// What `agent.set_hooks` asks for, or the `invalid-param` it is refused
/// with before anything is written (plan 064 §3.4).
///
/// The same rule `session.set_agent_hooks` draws on a host
/// (`roost-session`'s `agent_hooks::resolve`) and `roostctl agent set`
/// draws on a spec: this op answers the consent question, and a consent
/// answer has no honest partial reading — one name nothing answers to
/// refuses the whole list rather than silently narrowing it. The one
/// difference is that `off` arrives here as its own wire spelling
/// instead of as a word in the list.
pub(crate) fn resolve_set(agents: &AgentSetHooksAgents) -> Result<Mode, String> {
    let names = match agents {
        AgentSetHooksAgents::Off => return Ok(Mode::Off),
        AgentSetHooksAgents::List(names) => names,
    };
    // Checked before `resolve_names`, which skips blanks: skipping is
    // right for a human-typed CLI list and wrong on a wire, where a
    // blank element can only be a bug in the client.
    if names.iter().any(|name| name.trim().is_empty()) {
        return Err("agent.set_hooks: `agents` carries an empty name".to_string());
    }
    let (agents, unknown) = roost_agent_install::resolve_names(names.iter().map(String::as_str));
    if let Some(name) = unknown.first() {
        return Err(format!(
            "agent.set_hooks: no agent named {name:?} ({})",
            roost_agent_install::agent_names()
        ));
    }
    if agents.is_empty() {
        return Err(format!(
            "agent.set_hooks requires a non-empty `agents` ({}) or the word \"off\"",
            roost_agent_install::agent_names()
        ));
    }
    Ok(Mode::Allow(agents))
}

/// What one local `agent.set_hooks` did to this machine, on its way back
/// to the main thread.
pub(crate) struct AgentHooksSet {
    pub config_path: String,
    pub outcome: AgentHooksOutcome,
    /// The key this machine now has, read off the mode that was written
    /// rather than out of `outcome` — an outcome describes files, and
    /// the running UI's in-memory config has to match the key.
    pub key: AgentHooks,
    /// The toast list, and what `mark_noticed` is then given. The names
    /// are already in [`AgentHooksOutcome::wired`]; the agents are kept
    /// beside them because that is what both of those take.
    pub unnoticed: Vec<Agent>,
}

/// The blocking half of `agent.set_hooks`: the key write and the
/// reconcile, under the install lock.
///
/// Off the UI thread for this module's own reason — see its header.
/// Only a whole-run failure is an `Err`; a per-agent one rides back in
/// [`AgentHooksOutcome::errors`], because one unparseable `config.toml`
/// must not cost the user the answer they just gave.
pub(crate) fn set_hooks_blocking(mode: &Mode, guard: Guard) -> Result<AgentHooksSet, String> {
    let home = Home::from_env().map_err(|error| error.to_string())?;
    let outcome =
        roost_agent_install::set_hooks(&home, mode, BY, guard).map_err(|e| e.to_string())?;
    Ok(AgentHooksSet {
        config_path: home.config_path().display().to_string(),
        outcome: wire_outcome(&outcome),
        key: mode.to_config(),
        unnoticed: outcome.unnoticed,
    })
}

/// One install [`roost_agent_install::Outcome`] as the wire carries it.
///
/// `roost-session`'s `agent_hooks::reply` is the same map for the host
/// half of the same wire type, and the two copies are deliberate:
/// `roost-agent-install` owns `Outcome` and does not depend on
/// `roost-ipc`, so the only shared home would be a leaf crate holding
/// one function. Change one and check the other.
fn wire_outcome(outcome: &roost_agent_install::Outcome) -> AgentHooksOutcome {
    let names = |agents: &[Agent]| -> Vec<String> {
        agents.iter().map(|a| a.source().to_string()).collect()
    };
    AgentHooksOutcome {
        // The toast list, not this run's writes — see the field's own
        // doc in `roost-ipc`.
        wired: names(&outcome.unnoticed),
        refreshed: names(&outcome.refreshed),
        removed: names(&outcome.removed),
        skipped: outcome
            .skipped
            .iter()
            .map(|skip| AgentHooksSkipped {
                agent: skip.agent.source().to_string(),
                reason: skip.reason.to_string(),
            })
            .collect(),
        errors: outcome
            .errors
            .iter()
            .map(|failure| AgentHooksFailed {
                agent: failure.agent.source().to_string(),
                error: failure.error.to_string(),
            })
            .collect(),
    }
}

/// What one host answered `session.set_agent_hooks` with, on its way to
/// the toast (plan 046 §3.4).
pub(crate) struct HostAgentHooks {
    /// The host's label, for the toast prefix and the log.
    pub label: String,
    pub outcome: Result<AgentHooksOutcome, HostOpError>,
}

/// The allow-list one `session.set_agent_hooks` carries, or `None` when
/// this decision reaches a host at all.
///
/// **Only `Allow` ever sends anything, because the wire can now only
/// ever raise.** Plan 064 §3.3 reshaped `session.set_agent_hooks` into a
/// pure widen: there is no wire spelling of "off" or "narrow this" left
/// to send, so a host's key can only move up, never down. That retires
/// the C1-era "off is off everywhere" rule this decision used to carry —
/// `Off` sends nothing now, exactly like an unanswered key, because
/// neither state has an allow-list to widen a host with. Taking a host's
/// entries back out stays a deliberate, local act: `roostctl agent
/// ensure`/`uninstall`, run by hand on the host itself.
///
/// Both send sites resolve through here — the connect-time
/// [`remote_request`] and `agent.set_hooks`'s Apply push — so "what does
/// `off` do to a host?" has one answer and not two.
pub(crate) fn raise_list(mode: &Mode) -> Option<Vec<String>> {
    match mode {
        Mode::Allow(agents) if !agents.is_empty() => {
            Some(agents.iter().map(|a| a.source().to_string()).collect())
        }
        Mode::Allow(_) | Mode::Off => None,
    }
}

/// What this client asks a host to do at connect time, read fresh from
/// its config, or `None` to send nothing at all.
///
/// Sent on **every** connect, values and all, when there is a decision
/// to send: the op is idempotent, and a config edit made since the last
/// connect has no other way to reach the host.
///
/// An unanswered key sending nothing was always the rule: wiring a
/// host's dotfiles before the user has answered the local consent
/// dialog would be exactly the unconsented write plan 064 exists to
/// stop. It arrives here as [`resolve`]'s `None`, which is why this
/// reads the key through [`Mode`] rather than matching [`AgentHooks`]
/// directly — that also drops a name no agent answers to, so a stale
/// `config.conf` cannot put one on the wire.
pub(crate) fn remote_request(config: &RoostConfig) -> Option<Vec<String>> {
    raise_list(&resolve(config)?)
}

/// This machine's `agent-hooks` key **as it is on disk right now**.
///
/// `self.config` is a snapshot of launch time, and since plan 064 this
/// process is not its only writer: a connecting client raises this
/// machine's key through its own `roost-session`, and `roostctl agent
/// set --local` writes it with no UI running at all. A connect that
/// sent the snapshot would hand a host a list the user has since
/// changed — and because a raise can only widen, the host would keep it.
///
/// Reads and parses one small file rather than calling
/// `RoostConfig::load_default`, which also walks the providers
/// directory; this runs once per connect, not once per frame. An
/// unreadable config falls back to what this process already believes,
/// because failing to read is not a reason to say something different.
pub(crate) fn hooks_on_disk(fallback: &RoostConfig) -> AgentHooks {
    let Some(path) = roost_ui_model::config::config_path() else {
        return fallback.agent_hooks.clone();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => RoostConfig::parse(&text).agent_hooks,
        // Absent is unanswered, which is a real state and not a failure:
        // it is what a machine nobody has consented on looks like.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => AgentHooks::Ask,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not re-read agent-hooks");
            fallback.agent_hooks.clone()
        }
    }
}

/// How this client names itself in a host's state record.
///
/// The machine name, because `by` exists so that two clients of one host
/// — a Mac on `auto`, a Linux box on `off`, flipping the files past each
/// other — are tellable apart in `roostctl agent status` on the host.
pub(crate) fn client_label() -> String {
    let name = gethostname::gethostname();
    let name = name.to_string_lossy();
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "roost".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The one-time toast, per §3.7. `host` names the machine when the
/// wiring happened over a host connection, and is `None` for this one.
/// Split the agent names a host reported into the ones this client can
/// name and the ones it cannot.
///
/// The second list is not a rounding error. A host running a newer Roost
/// can wire an agent this client predates, and by the time the name
/// arrives the host has already flipped that agent's `noticed` flag —
/// there is no second telling. Filtering the name away would make the
/// one announcement that Roost edited that machine's dotfiles vanish
/// permanently, so the caller keeps it and says so.
pub(crate) fn split_known(names: &[String]) -> (Vec<Agent>, Vec<&str>) {
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    for name in names {
        match Agent::parse(name) {
            Some(agent) => known.push(agent),
            None => unknown.push(name.as_str()),
        }
    }
    (known, unknown)
}

pub(crate) fn wired_toast(agents: &[Agent], host: Option<&str>) -> Option<String> {
    if agents.is_empty() {
        return None;
    }
    let names: Vec<&str> = agents.iter().map(|agent| agent.source()).collect();
    let prefix = match host {
        Some(label) => format!("on {label}: "),
        None => String::new(),
    };
    Some(format!(
        "{prefix}Roost wired agent hooks for {} — change it under \
         Agent Hooks… in the command palette",
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(body: &str) -> RoostConfig {
        RoostConfig::parse(body)
    }

    /// A name this client cannot parse survives as a name. The host has
    /// already spent its `noticed` flag on it, so anything that dropped
    /// it here would lose the announcement for good.
    #[test]
    fn an_agent_name_this_client_does_not_know_is_kept_not_dropped() {
        let names = vec![
            "claude".to_string(),
            "gemini".to_string(),
            "codex".to_string(),
        ];
        let (known, unknown) = split_known(&names);
        assert_eq!(known, vec![Agent::Claude, Agent::Codex]);
        assert_eq!(unknown, vec!["gemini"]);
    }

    /// `Ask` — the default — resolves to `None`: nobody has consented
    /// yet, so `spawn_ensure` must not wire anything.
    #[test]
    fn an_unconfigured_config_resolves_to_none() {
        assert_eq!(resolve(&config("")), None);
    }

    #[test]
    fn an_allow_list_wires_only_the_named_agents() {
        assert_eq!(
            resolve(&config("agent-hooks = claude, cursor")),
            Some(Mode::Allow(vec![Agent::Claude, Agent::Cursor]))
        );
    }

    #[test]
    fn off_resolves_to_off() {
        assert_eq!(resolve(&config("agent-hooks = off")), Some(Mode::Off));
    }

    /// `Off` sends nothing — the wire can only ever raise a host now
    /// (plan 064 §3.3), and an off client has no allow-list to raise it
    /// with. Retiring the host's entries stays a local, explicit act.
    ///
    /// Asserted at both send sites: the connect-time read of the key,
    /// and the Apply push `agent.set_hooks` makes.
    #[test]
    fn off_sends_nothing_to_a_host() {
        assert_eq!(remote_request(&config("agent-hooks = off")), None);
        assert_eq!(raise_list(&Mode::Off), None);
    }

    /// The Apply push carries exactly what was applied.
    #[test]
    fn an_applied_allow_list_travels_as_itself() {
        assert_eq!(
            raise_list(&Mode::Allow(vec![Agent::Claude, Agent::Cursor])),
            Some(vec!["claude".to_string(), "cursor".to_string()])
        );
    }

    #[test]
    fn an_allow_list_travels_as_itself() {
        assert_eq!(
            remote_request(&config("agent-hooks = claude, cursor")),
            Some(vec!["claude".to_string(), "cursor".to_string()])
        );
    }

    /// `Ask` sends nothing at all: an unconfigured client must not wire
    /// a host's dotfiles either.
    #[test]
    fn an_unconfigured_config_sends_nothing_to_a_host() {
        assert_eq!(remote_request(&config("")), None);
    }

    fn set(agents: &[&str]) -> Result<Mode, String> {
        resolve_set(&AgentSetHooksAgents::List(
            agents.iter().map(|s| (*s).to_string()).collect(),
        ))
    }

    #[test]
    fn agent_set_hooks_takes_a_list_or_the_word_off() {
        assert_eq!(
            set(&["claude", "codex"]),
            Ok(Mode::Allow(vec![Agent::Claude, Agent::Codex]))
        );
        assert_eq!(resolve_set(&AgentSetHooksAgents::Off), Ok(Mode::Off));
    }

    /// The two shapes that are bugs in the caller rather than answers.
    #[test]
    fn agent_set_hooks_refuses_an_empty_or_unknown_list() {
        assert!(set(&[]).unwrap_err().contains("non-empty"));
        assert!(set(&["  "]).unwrap_err().contains("empty name"));
        assert!(set(&["claude", ""]).unwrap_err().contains("empty name"));
        let refused = set(&["claude", "gemini"]).unwrap_err();
        assert!(refused.contains("gemini"), "{refused}");
        for known in ["claude", "codex", "grok", "cursor", "opencode"] {
            assert!(refused.contains(known), "{refused}");
        }
    }

    /// It goes into the host's state record, so it has to be a name and
    /// never an empty string.
    #[test]
    fn the_client_label_is_never_empty() {
        assert!(!client_label().is_empty());
    }

    /// The text is what the user is left with after Roost has edited
    /// their dotfiles, so it has to name both what was wired and the one
    /// surface that changes it again (plan 064 §3.4).
    #[test]
    fn the_toast_names_the_agents_and_the_way_back() {
        let toast = wired_toast(&[Agent::Claude, Agent::Codex], None).unwrap();
        assert!(
            toast.starts_with("Roost wired agent hooks for claude, codex"),
            "{toast}"
        );
        assert!(toast.contains("Agent Hooks…"), "{toast}");
    }

    /// C8 wires a host's result through the same text; the prefix is the
    /// only difference, and it goes in front rather than rewording it.
    #[test]
    fn a_host_wiring_says_where() {
        let toast = wired_toast(&[Agent::Grok], Some("shed")).unwrap();
        assert!(
            toast.starts_with("on shed: Roost wired agent hooks for grok"),
            "{toast}"
        );
    }

    /// Nothing left to announce is not news — a refresh on upgrade is
    /// silent, because its agents are already `noticed`.
    #[test]
    fn nothing_wired_is_no_toast() {
        assert_eq!(wired_toast(&[], None), None);
        assert_eq!(wired_toast(&[], Some("shed")), None);
    }

    /// The ensure is a *startup* act. `window_opened` runs again on
    /// every focus **and** unfocus, so without the latch an Alt-Tab
    /// would re-wire — and silently undo a hook the user had just
    /// removed by hand.
    #[test]
    fn the_startup_ensure_runs_once_per_process() {
        let mut started = false;
        let allow = Mode::Allow(vec![Agent::Claude]);
        assert_eq!(claim_start(&mut started, Some(&allow)), Start::Run);
        assert!(started);
        // The second window event is the focus that follows the open;
        // the third is an ordinary Alt-Tab. Neither may wire anything.
        assert_eq!(claim_start(&mut started, Some(&allow)), Start::Already);
        assert_eq!(claim_start(&mut started, Some(&allow)), Start::Already);
    }

    /// `off` declines without consuming the latch: the two answers are
    /// different reasons and must not be confused for one another.
    #[test]
    fn off_declines_without_claiming_the_latch() {
        let mut started = false;
        assert_eq!(claim_start(&mut started, Some(&Mode::Off)), Start::Off);
        assert!(!started);
        assert_eq!(
            claim_start(&mut started, Some(&Mode::Allow(vec![Agent::Claude]))),
            Start::Run
        );
    }

    /// `ask` — `resolve` returning `None` — declines the same way `off`
    /// does: neither claims the latch, and the two must stay tellable
    /// apart in the log line each produces.
    #[test]
    fn ask_declines_without_claiming_the_latch() {
        let mut started = false;
        assert_eq!(claim_start(&mut started, None), Start::Ask);
        assert!(!started);
        assert_eq!(
            claim_start(&mut started, Some(&Mode::Allow(vec![Agent::Claude]))),
            Start::Run
        );
    }
}
