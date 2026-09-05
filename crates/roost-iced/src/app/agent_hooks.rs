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
use roost_ui_model::config::{AgentHooks, RoostConfig};

use crate::engine_feed::{EngineFeed, EngineFeedSender};

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

/// The mode and skip list `config.conf` asks for, plus any skip name no
/// agent answers to.
fn resolve(config: &RoostConfig) -> (Mode, Vec<Agent>, Vec<String>) {
    let (skip, unknown) =
        roost_agent_install::skip_list(config.agent_hooks_skip.iter().map(String::as_str));
    let mode = match config.agent_hooks {
        AgentHooks::Auto => Mode::Auto,
        AgentHooks::Off => Mode::Off,
    };
    (mode, skip, unknown)
}

/// What a `window_opened` should do about the startup ensure.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Start {
    Run,
    /// `agent-hooks = off`.
    Off,
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
/// case free to be reconsidered if a later launch ever re-reads config.
pub(crate) fn claim_start(started: &mut bool, mode: Mode) -> Start {
    if mode == Mode::Off {
        return Start::Off;
    }
    if *started {
        return Start::Already;
    }
    *started = true;
    Start::Run
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
    let (mode, skip, unknown) = resolve(config);
    match claim_start(started, mode) {
        Start::Run => {}
        Start::Off => {
            tracing::debug!("agent-hooks = off: not wiring agent hooks");
            return;
        }
        Start::Already => return,
    }
    for name in unknown {
        tracing::warn!(
            name,
            known = %roost_agent_install::agent_names(),
            "agent-hooks-skip names no agent Roost knows how to wire"
        );
    }

    let feed = feed.clone();
    let guard = Guard::from_env();
    // The resolved values ride the closure rather than being re-read
    // over there: a config edit landing mid-launch would otherwise split
    // the decision from the action it authorised.
    runtime.spawn_blocking(move || {
        feed.send(EngineFeed::AgentHooks(ensure_blocking(&skip, guard)));
    });
}

/// The blocking half. Every failure becomes a line in
/// [`AgentHooksEnsured::errors`] rather than a panic or a swallow: this
/// runs with nobody waiting on it, so the only honest thing to do with a
/// failure is carry it back to a thread that can log it.
fn ensure_blocking(skip: &[Agent], guard: Guard) -> AgentHooksEnsured {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(error) => {
            return AgentHooksEnsured {
                unnoticed: Vec::new(),
                errors: vec![error.to_string()],
            }
        }
    };
    match roost_agent_install::ensure(&home, Mode::Auto, skip, BY, guard) {
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

/// The one-time toast, per §3.7. `host` names the machine when the
/// wiring happened over a host connection, and is `None` for this one.
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
        "{prefix}Roost wired agent hooks for {} — undo: \
         `roostctl agent uninstall --all` or `agent-hooks = off`",
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(body: &str) -> RoostConfig {
        RoostConfig::parse(body)
    }

    #[test]
    fn the_default_config_wires_everything() {
        let (mode, skip, unknown) = resolve(&config(""));
        assert_eq!(mode, Mode::Auto);
        assert!(skip.is_empty());
        assert!(unknown.is_empty());
    }

    #[test]
    fn off_and_the_skip_list_reach_the_engine() {
        let (mode, skip, unknown) = resolve(&config(
            "agent-hooks = off\nagent-hooks-skip = cursor, gemini",
        ));
        assert_eq!(mode, Mode::Off);
        assert_eq!(skip, vec![Agent::Cursor]);
        assert_eq!(unknown, vec!["gemini"]);
    }

    /// The text is what the user is left with after Roost has edited
    /// their dotfiles, so both escape hatches have to be *in* it.
    #[test]
    fn the_toast_names_the_agents_and_both_ways_out() {
        let toast = wired_toast(&[Agent::Claude, Agent::Codex], None).unwrap();
        assert!(
            toast.starts_with("Roost wired agent hooks for claude, codex"),
            "{toast}"
        );
        assert!(toast.contains("roostctl agent uninstall --all"), "{toast}");
        assert!(toast.contains("agent-hooks = off"), "{toast}");
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
        assert_eq!(claim_start(&mut started, Mode::Auto), Start::Run);
        assert!(started);
        // The second window event is the focus that follows the open;
        // the third is an ordinary Alt-Tab. Neither may wire anything.
        assert_eq!(claim_start(&mut started, Mode::Auto), Start::Already);
        assert_eq!(claim_start(&mut started, Mode::Auto), Start::Already);
    }

    /// `off` declines without consuming the latch: the two answers are
    /// different reasons and must not be confused for one another.
    #[test]
    fn off_declines_without_claiming_the_latch() {
        let mut started = false;
        assert_eq!(claim_start(&mut started, Mode::Off), Start::Off);
        assert!(!started);
        assert_eq!(claim_start(&mut started, Mode::Auto), Start::Run);
    }
}
