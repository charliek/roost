//! Wiring the coding agents' hook entries at startup, and saying so
//! once (plan 046 §3.7).
//!
//! # Why none of this runs on the UI thread
//!
//! [`roost_agent_install::ensure`] reads and writes files in the user's
//! home, takes an advisory `flock` across the whole plan+apply, and can
//! block on another Roost — the CLI, a host connect — doing the same.
//! Iced's `update`/`view` is the winit event-loop
//! thread; a `flock` taken there freezes the window. So the work goes to
//! the runtime's **blocking** pool and the answer comes back on the
//! engine feed, which is the same road every other off-thread result in
//! this app travels.
//!
//! # Why the announcement is at most once
//!
//! `ensure` returns the agents the **state record** says are wired and
//! have never been announced — `Outcome::unnoticed`, not the agents this
//! particular run wired. That is what makes the sentence a property of
//! the machine: a wiring done by `roostctl` (which has no transient
//! status surface and leaves `noticed` false on purpose) still gets said
//! here, once, on the first launch that can say it.
//!
//! The order is deliberate: toast first, then
//! [`roost_agent_install::mark_noticed`]. A crash in between loses the
//! toast rather than repeating it, which is the right way round for a
//! line that says Roost changed the user's files — and because the flag
//! is what drives the toast, a `mark_noticed` that never lands is simply
//! a toast the next launch shows instead.

use roost_agent::{Agent, ALL_AGENTS};
use roost_agent_install::{Guard, Home, InstallError, Mode, Outcome, SkipReason};
use roost_engine::ipc::HostOpFailure;
use roost_ipc::codes;
use roost_ipc::messages::{
    AgentHooksFailed, AgentHooksOutcome, AgentHooksSkipped, AgentSetHooksAgents,
    AgentSetHooksHostOutcome, AgentSetHooksResult,
};
use roost_ui_model::config::{AgentHooks, AgentIntegration, AgentSwitches, RoostConfig};

use super::agent_hooks_dialog::{AgentHooksDraft, CardMode};
use super::local_backend::plural;
use crate::engine_feed::{EngineFeed, EngineFeedSender};
use crate::host_conn::queue::HostOpError;
use crate::host_conn::HostRaise;

/// How this UI identifies itself in the state record. The same label
/// `roostctl` writes: both are this machine acting on its own behalf,
/// and the field exists to distinguish *that* from a remote client.
const BY: &str = "local";

/// What one background `ensure` had to say, as the UI needs it.
#[derive(Debug, Default)]
pub(crate) struct AgentHooksEnsured {
    /// Agents the record says are wired and unannounced — what the
    /// toast announces, less any agent in [`Self::problems`].
    pub unnoticed: Vec<Agent>,
    /// Everything that left an allowed, present agent unwired (plan 068
    /// §3.6). Each is logged at the drain and named in the startup
    /// toast, which only points at `roostctl agent status`: that and
    /// doctor are where the detail lives.
    pub problems: Vec<HookProblem>,
}

#[derive(Debug)]
pub(crate) struct HookProblem {
    /// `None` when the whole ensure failed before it reached an agent.
    pub agent: Option<Agent>,
    pub detail: String,
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

/// Claim the one first-run consent card this process gets (plan 064
/// §3.5).
///
/// Its own latch beside [`claim_start`], because they answer different
/// questions and `window_opened` asks both on **every focus change**: an
/// Alt-Tab back to the window must not put the card up a second time,
/// and the `Ask` branch deliberately never claims `claim_start`'s latch.
///
/// Three conditions, and the two that are not the latch are refusals:
/// only an unanswered key has anything to ask about, and a UI under
/// `ROOST_TEST_MODE` asks nothing at all unless the harness lifted the
/// fence — the same [`Guard`] the install engine itself checks, read
/// here so a test-mode launch never even raises the card.
pub(crate) fn claim_first_run(raised: &mut bool, start: &Start, guard: Guard) -> bool {
    if !matches!(start, Start::Ask) || guard.check().is_err() || *raised {
        return false;
    }
    *raised = true;
    true
}

/// Start the startup ensure, or decline to — and say which, because the
/// caller has its own thing to do about `Ask`.
///
/// `agent-hooks = off` returns without touching a single file — the key
/// means "Roost wires nothing here", and a startup that *removed*
/// entries would make an opt-out into an action the user did not ask
/// for. `roostctl agent ensure` is the explicit verb that reads the same
/// `off` and takes them back out.
///
/// The launch-time config decides only **whether** to start; what is
/// wired is [`roost_agent_install::ensure`]'s own read of the key,
/// inside the lock. Carrying this snapshot over would let a `roostctl
/// agent uninstall` that lands between launch and window re-appear as a
/// re-wiring nobody asked for.
///
/// The retire of the file entries this build no longer needs
/// ([`roost_agent_install::retire`]) runs first, once per process,
/// whatever the key says: it narrows `agent-hooks` to the file agents,
/// which is what the ensure then reads. It waits while the local tabs
/// run on a `roost-session` (`defer_retire`): that session may be an
/// older build whose tabs get no env delivery, and a v4 session retires
/// at its own start.
pub(crate) fn spawn_ensure(
    started: &mut bool,
    retire_started: &mut bool,
    runtime: &tokio::runtime::Handle,
    feed: &EngineFeedSender,
    config: &RoostConfig,
    defer_retire: bool,
) -> Start {
    let start = claim_start(started, resolve(config).as_ref());
    match &start {
        Start::Run | Start::Already => {}
        Start::Off => tracing::debug!("agent-hooks = off: not wiring agent hooks"),
        Start::Ask => tracing::debug!("agent-hooks is not configured; not wiring agent hooks"),
    }
    let ensure = start == Start::Run;
    let retire = !std::mem::replace(retire_started, true);
    if !ensure && !retire {
        return start;
    }

    let feed = feed.clone();
    let guard = Guard::from_env();
    runtime.spawn_blocking(move || {
        if retire {
            feed.send(EngineFeed::AgentRetire(retire_blocking(
                guard,
                defer_retire,
            )));
        }
        if ensure {
            feed.send(EngineFeed::AgentHooks(ensure_blocking(guard)));
        }
    });
    start
}

/// What the startup retire had to say, as the UI needs it.
#[derive(Debug, Default)]
pub(crate) struct AgentRetired {
    /// Agents whose retire removed entries and was never announced.
    pub unnoticed: Vec<Agent>,
    /// Entries left in place, which `roostctl agent status` names.
    pub leftovers: usize,
    pub problems: Vec<HookProblem>,
    pub deferred: bool,
    /// This run carried `agent-hooks = off` over: the one-time "they stay
    /// off" notice, naming the installed agents it would now connect.
    pub carried_off: Option<Vec<Agent>>,
    /// Claude is on, its organization's policy keeps the plugin out, and
    /// the user has not been told since the policy started to.
    pub policy_notice: bool,
}

fn retire_blocking(guard: Guard, defer: bool) -> AgentRetired {
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(error) => {
            return AgentRetired {
                problems: vec![HookProblem {
                    agent: None,
                    detail: error.to_string(),
                }],
                ..AgentRetired::default()
            }
        }
    };
    let mut retired = retire_outcome(roost_agent_install::retire(&home, guard, defer));
    if let Some(stay_off) = &mut retired.carried_off {
        stay_off.extend(
            [Agent::Claude, Agent::Opencode]
                .into_iter()
                .filter(|agent| home.is_present(*agent)),
        );
    }
    AgentRetired {
        policy_notice: policy_notice_due(&home),
        ..retired
    }
}

/// The sideload policy's notice, read without a probe of this process's
/// own: the tab environment's probe records its verdict, and the managed
/// files are read here.
fn policy_notice_due(home: &Home) -> bool {
    use roost_agent_install::tab_env::{self, PolicyNotice};
    let claude_on = roost_agent_install::retire::switches(home)
        .is_ok_and(|switches| switches.on(Agent::Claude));
    let policy = claude_on
        .then(|| tab_env::claude_policy_here(home))
        .flatten();
    match tab_env::policy_notice(claude_on, policy.as_ref(), tab_env::policy_noticed(home)) {
        PolicyNotice::Show => true,
        PolicyNotice::Forget => {
            if let Err(error) = tab_env::forget_policy_noticed(home) {
                tracing::warn!(%error, "could not clear the Claude policy toast's mark");
            }
            false
        }
        PolicyNotice::Nothing => false,
    }
}

fn retire_outcome(
    outcome: Result<roost_agent_install::RetireOutcome, InstallError>,
) -> AgentRetired {
    match outcome {
        Ok(outcome) => AgentRetired {
            unnoticed: outcome.unnoticed,
            leftovers: outcome.leftovers.len(),
            carried_off: outcome.carried_off.then(Vec::new),
            problems: outcome
                .errors
                .into_iter()
                .map(|failure| HookProblem {
                    agent: Some(failure.agent),
                    detail: failure.error.to_string(),
                })
                .chain(outcome.key_error.map(|error| HookProblem {
                    agent: None,
                    detail: error.to_string(),
                }))
                .collect(),
            deferred: outcome.deferred,
            ..AgentRetired::default()
        },
        // The harness fence: nothing to retire, nothing to say.
        Err(InstallError::TestModeRefused) => AgentRetired::default(),
        Err(error) => AgentRetired {
            problems: vec![HookProblem {
                agent: None,
                detail: error.to_string(),
            }],
            ..AgentRetired::default()
        },
    }
}

/// What the menu item is called, in every sentence that points at it.
const SETTINGS: &str = "Agent Integration…";

const BACKUP: &str = "backup in ~/.config/roost/agent-backups";

/// The one-time sentence for an upgrade: the agents whose file entries
/// the retire removed, connected now through the tab environment or
/// reporting on their own, and how many entries were left for `agent
/// status` to name. An upgrader who had said `off` (`carried_off`, the
/// installed agents Roost could now connect) hears instead that those
/// agents can connect without editing files, and stay off.
pub(crate) fn retire_line(
    agents: &[Agent],
    leftovers: usize,
    carried_off: Option<&[Agent]>,
) -> Option<String> {
    let label = |agent: &Agent| roost_ui_model::agent_palette::agent_display_name(agent.source());
    let mut line = match carried_off {
        Some(stay_off) if !stay_off.is_empty() => {
            let names: Vec<&str> = stay_off.iter().map(label).collect();
            let (its, stays, it) = if names.len() == 1 {
                ("its", "It stays", "it")
            } else {
                ("their", "They stay", "them")
            };
            let mut line = format!(
                "Roost can now connect {} without editing {its} settings files. {stays} off; \
                 turn {it} on under {SETTINGS}",
                names.join(" and ")
            );
            if !agents.is_empty() {
                line.push_str(&format!(
                    ". Roost also removed hook entries it had added earlier ({BACKUP})"
                ));
            }
            line
        }
        _ if agents.is_empty() => return None,
        Some(_) => format!("Roost removed hook entries it had added earlier ({BACKUP})"),
        None => {
            let names = |kinds: &[Agent]| -> Vec<&str> {
                agents
                    .iter()
                    .filter(|a| kinds.contains(a))
                    .map(label)
                    .collect()
            };
            let env = names(&[Agent::Claude, Agent::Opencode]);
            let own = names(&[Agent::Codex, Agent::Grok]);
            let mut clauses = Vec::new();
            if !env.is_empty() {
                let (verb, their) = if env.len() == 1 {
                    ("connects", "its")
                } else {
                    ("connect", "their")
                };
                clauses.push(format!(
                    "{} now {verb} to Roost without changes to {their} settings files",
                    env.join(" and ")
                ));
            }
            if !own.is_empty() {
                let (verb, their) = if own.len() == 1 {
                    ("reports", "its")
                } else {
                    ("report", "their")
                };
                clauses.push(format!(
                    "{} {verb} {their} status on {their} own",
                    own.join(" and ")
                ));
            }
            let lead = if clauses.is_empty() {
                "Roost no longer needs its hook entries".to_string()
            } else {
                clauses.join(", and ")
            };
            format!(
                "{lead}. Roost removed the entries it had added ({BACKUP}). {SETTINGS} to change"
            )
        }
    };
    if leftovers > 0 {
        line.push_str(&format!(
            ". Roost left {leftovers} entr{} it can't be sure it added: roostctl agent status \
             lists {}",
            if leftovers == 1 { "y" } else { "ies" },
            if leftovers == 1 { "it" } else { "them" }
        ));
    }
    Some(line)
}

/// Said once while the policy holds; doctor keeps the line.
pub(crate) const POLICY_LINE: &str = "Claude Code isn't connected: your organization's Claude \
     policy blocks plugin folders. Its status still shows from the terminal";

/// Record that the policy notice was shown, off the UI thread.
pub(crate) fn spawn_mark_policy_noticed(runtime: &tokio::runtime::Handle) {
    runtime.spawn_blocking(|| {
        let result = Home::from_env().and_then(|home| {
            roost_agent_install::tab_env::mark_policy_noticed(&home)
                .map_err(|e| InstallError::io(home.roost_config_dir(), e))
        });
        if let Err(error) = result {
            tracing::warn!(%error, "could not record that the Claude policy toast was shown");
        }
    });
}

/// Flip the retire's `noticed` for the agents the toast just named.
pub(crate) fn spawn_mark_retired(runtime: &tokio::runtime::Handle, agents: Vec<Agent>) {
    if agents.is_empty() {
        return;
    }
    runtime.spawn_blocking(move || {
        let result = Home::from_env()
            .and_then(|home| roost_agent_install::retire::mark_noticed(&home, &agents).map(|_| ()));
        if let Err(error) = result {
            tracing::warn!(%error, "could not record that the agent retire toast was shown");
        }
    });
}

/// One queued `agent.set_hooks`, and the machinery that keeps two of
/// them in the order the user asked for (#490, plan 065 §3.5).
///
/// # Why a queue and not a mutex
///
/// Each request rewrites the `agent-hooks` key outright, so two applies
/// that overlap decide the file between them. A `tokio::sync::Mutex` is
/// fair in *acquisition-attempt* order, which is the order independently
/// spawned tasks happen to reach it — not the order the requests
/// arrived. The ticket is taken on the main thread at receipt and one
/// worker drains the queue, so "last request wins" means the user's last
/// gesture, not whichever task the runtime polled last.
pub(crate) struct AgentHooksApply {
    pub ticket: u64,
    pub mode: Mode,
    /// Names the request carried that this build cannot wire. Not the
    /// worker's business beyond reporting them: they reach neither the
    /// key (`set_hooks` replaces it with what was applied) nor a host.
    pub unknown: Vec<String>,
    pub guard: Guard,
    /// What each connected host is asked to raise, and who to raise it
    /// as. Chosen on the main thread with the rest of this request — the
    /// connection registry is main-thread state.
    pub raise_names: Option<Vec<String>>,
    pub raises: Vec<HostRaise>,
    pub client: String,
    pub reply: super::HostOpReply<AgentSetHooksResult>,
}

/// The one worker every `agent.set_hooks` goes through.
#[derive(Debug, Clone)]
pub(crate) struct AgentHooksApplies {
    tx: tokio::sync::mpsc::UnboundedSender<AgentHooksApply>,
}

impl AgentHooksApplies {
    pub(crate) fn spawn(
        runtime: &tokio::runtime::Handle,
        feed: EngineFeedSender,
    ) -> AgentHooksApplies {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentHooksApply>();
        runtime.spawn(async move {
            // Awaited inline: the next apply does not start until this
            // one has written the key and answered its caller.
            while let Some(apply) = rx.recv().await {
                apply_set_hooks(apply, &feed).await;
            }
        });
        AgentHooksApplies { tx }
    }

    pub(crate) fn send(&self, apply: AgentHooksApply) {
        if let Err(tokio::sync::mpsc::error::SendError(apply)) = self.tx.send(apply) {
            let _ = apply.reply.send(Err(HostOpFailure::new(
                codes::INTERNAL,
                "the agent-hooks worker went away".to_string(),
            )));
        }
    }
}

/// How long the **first** apply of this process is held before it
/// writes, so a test can send a second one behind it and prove the file
/// lands in request order.
///
/// Gated on `ROOST_TEST_MODE=1` like every other harness seam here, and
/// read per call so a jailed UI launched with it set is the only thing
/// that can slow down.
fn apply_delay(ticket: u64) -> Option<std::time::Duration> {
    if ticket != 1 || std::env::var("ROOST_TEST_MODE").as_deref() != Ok("1") {
        return None;
    }
    let ms: u64 = std::env::var("ROOST_TEST_AGENT_HOOKS_DELAY_MS")
        .ok()?
        .parse()
        .ok()?;
    Some(std::time::Duration::from_millis(ms))
}

/// Make the apply holding this ticket refuse **before it writes
/// anything**, so a test can pin what a *superseded* apply still owes
/// the running config when the apply that superseded it lands nothing.
///
/// A seam rather than the real thing because the real thing cannot be
/// scheduled: the one worker runs applies back to back, so there is no
/// moment at which a test could take `config.lock` between two of them
/// — and that refusal, mid-queue and after an earlier apply succeeded,
/// is exactly the interleaving [`super::App::agent_hooks_applied`]
/// reasons about. Gated on `ROOST_TEST_MODE=1` like [`apply_delay`].
fn apply_refusal(ticket: u64) -> Option<String> {
    if std::env::var("ROOST_TEST_MODE").as_deref() != Ok("1") {
        return None;
    }
    let refused: u64 = std::env::var("ROOST_TEST_AGENT_HOOKS_REFUSE_TICKET")
        .ok()?
        .parse()
        .ok()?;
    (refused == ticket).then(|| "config.lock is busy (test seam)".to_string())
}

async fn apply_set_hooks(apply: AgentHooksApply, feed: &EngineFeedSender) {
    let AgentHooksApply {
        ticket,
        mode,
        unknown,
        guard,
        raise_names,
        raises,
        client,
        reply,
    } = apply;
    if let Some(delay) = apply_delay(ticket) {
        tokio::time::sleep(delay).await;
    }
    let local = match apply_refusal(ticket) {
        Some(refused) => Err(refused),
        None => tokio::task::spawn_blocking(move || set_hooks_blocking(ticket, &mode, guard))
            .await
            .unwrap_or_else(|error| {
                Err(format!("the agent-hooks install did not finish: {error}"))
            }),
    };
    let mut done = match local {
        Ok(done) => done,
        Err(message) => {
            // Reported to the caller, which is the boundary that handles
            // it; the log line is for the launches where the caller was
            // a dialog nobody was watching.
            tracing::warn!(error = %message, "agent.set_hooks could not set this machine's agent hooks");
            let _ = reply.send(Err(HostOpFailure::new(codes::INTERNAL, message)));
            return;
        }
    };
    if let Some(message) = &done.error {
        // The key landed and the rest of the run did not (#491): the
        // caller hears the failure, the main thread still takes the key,
        // and no host is raised.
        let _ = reply.send(Err(HostOpFailure::new(codes::INTERNAL, message.clone())));
        feed.send(EngineFeed::AgentHooksSet(Box::new(done)));
        return;
    }
    done.outcome
        .skipped
        .extend(AgentHooksSkipped::unknown(&unknown));
    // Only now: this machine has recorded the user's answer, so it has
    // something it is entitled to propagate. A local write that failed
    // reaches no host at all.
    let names = raise_names.unwrap_or_default();
    let pending: Vec<_> = raises
        .iter()
        .map(|raise| (raise.label.clone(), raise.send(&names, &client)))
        .collect();
    let mut hosts = Vec::new();
    for (label, outcome) in pending {
        hosts.push(match outcome.await {
            Ok(result) => AgentSetHooksHostOutcome::Result {
                host: label,
                result,
            },
            // Never fatal, and never an error frame: this machine's own
            // key is set either way, and a host that could not be asked
            // is one line in the reply.
            Err(error) => AgentSetHooksHostOutcome::Error {
                host: label,
                error: error.to_string(),
            },
        });
    }
    let _ = reply.send(Ok(AgentSetHooksResult {
        config_path: done.config_path.clone(),
        local: done.outcome.clone(),
        hosts,
    }));
    feed.send(EngineFeed::AgentHooksSet(Box::new(done)));
}

/// What one read-only status walk found, on its way to the consent card.
///
/// `mode` rides outside the `Result` because a failure still has to be
/// routed: a preferences card the user asked for owes them a sentence,
/// and a first-run probe nobody asked for stays quiet.
pub(crate) struct AgentHooksSurvey {
    pub mode: CardMode,
    /// Which survey this is. A survey reads five agents' files, so two
    /// can be in flight at once — a second palette activation, or the
    /// startup probe still running when the user opens preferences. The
    /// app keeps the id it is waiting for and drops anything else, so a
    /// stale result cannot replace the card the user is looking at.
    pub id: u64,
    pub result: Result<AgentHooksFound, String>,
}

pub(crate) struct AgentHooksFound {
    pub draft: AgentHooksDraft,
    /// The agent keys **as the survey read them off disk**, which the
    /// running UI takes when another process answered while this launch
    /// was asking.
    pub switches: AgentSwitches,
    /// Either agent key holds an answer on disk. A first-run card is not
    /// raised over an answer somebody has already given.
    pub answered: bool,
}

/// Read every agent's status for the consent card, off the UI thread.
///
/// Off the thread for this module's own reason — see its header:
/// `status` opens up to six files across five agents' config
/// directories, and the preferences card re-runs it on every open.
pub(crate) fn spawn_survey(
    mode: CardMode,
    id: u64,
    fallback: AgentSwitches,
    runtime: &tokio::runtime::Handle,
    feed: &EngineFeedSender,
) {
    let feed = feed.clone();
    runtime.spawn_blocking(move || {
        feed.send(EngineFeed::AgentHooksSurvey(Box::new(AgentHooksSurvey {
            mode,
            id,
            result: survey_blocking(mode, &fallback),
        })));
    });
}

/// The blocking half of the survey.
///
/// The key is re-read from disk rather than taken from the running UI's
/// snapshot, for [`hooks_on_disk`]'s reason and one more: opening the
/// preferences card is exactly when a key changed by `roostctl agent set
/// --local`, or raised through this machine's own session, has to be
/// what the switches show.
fn survey_blocking(mode: CardMode, fallback: &AgentSwitches) -> Result<AgentHooksFound, String> {
    let home = Home::from_env().map_err(|error| error.to_string())?;
    let switches = switches_on_disk(fallback);
    // `Ask` allows nothing: `Status::allowed` reads as "named in the
    // file shadow", and nobody has named anything yet.
    let resolved = Mode::from_config(&switches.legacy).unwrap_or_else(|| Mode::Allow(Vec::new()));
    let mut statuses = roost_agent_install::status(&home, &resolved).map_err(|e| e.to_string())?;
    for status in &mut statuses {
        status.on = switches.on(status.agent);
    }
    let claude_blocked = roost_agent_install::tab_env::claude_policy_here(&home)
        .is_some_and(|policy| policy.blocks());
    Ok(AgentHooksFound {
        draft: AgentHooksDraft::new(mode, &statuses, claude_blocked),
        answered: roost_agent_install::retire::answered(&switches),
        switches,
    })
}

/// Both agent keys **as they are on disk right now**, for
/// [`hooks_on_disk`]'s reason. Absent is unanswered, which is a real
/// state and not a failure; an unreadable config falls back to what this
/// process already believes, because failing to read is not a reason to
/// say something different.
fn switches_on_disk(fallback: &AgentSwitches) -> AgentSwitches {
    let Some(path) = roost_ui_model::config::config_path() else {
        return fallback.clone();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => AgentSwitches::read(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => AgentSwitches::default(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not re-read the agent keys");
            fallback.clone()
        }
    }
}

/// The blocking half. Every failure becomes a [`HookProblem`] rather
/// than a panic or a swallow: this runs with nobody waiting on it, so the
/// only honest thing to do with a failure is carry it back to a thread
/// that can log it and say so.
fn ensure_blocking(guard: Guard) -> AgentHooksEnsured {
    let failed = |error: InstallError| AgentHooksEnsured {
        unnoticed: Vec::new(),
        problems: vec![HookProblem {
            agent: None,
            detail: error.to_string(),
        }],
    };
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(error) => return failed(error),
    };
    match roost_agent_install::ensure(&home, BY, guard) {
        Ok(outcome) => AgentHooksEnsured {
            problems: problems(&outcome),
            unnoticed: outcome.unnoticed,
        },
        Err(error) => failed(error),
    }
}

fn problems(outcome: &Outcome) -> Vec<HookProblem> {
    let failed = outcome.errors.iter().map(|failure| HookProblem {
        agent: Some(failure.agent),
        detail: failure.error.to_string(),
    });
    let refused = outcome
        .skipped
        .iter()
        .filter(|skip| leaves_unwired(&skip.reason))
        .map(|skip| HookProblem {
            agent: Some(skip.agent),
            detail: skip.reason.to_string(),
        });
    failed.chain(refused).collect()
}

/// The skips that are a defect in a file. The other three are the user's
/// own answers — not installed, not in the key, `off` — and a toast about
/// them would nag someone for a choice they made.
fn leaves_unwired(reason: &SkipReason) -> bool {
    match reason {
        SkipReason::Unparseable { .. }
        | SkipReason::UnexpectedShape { .. }
        | SkipReason::ForeignFile { .. } => true,
        SkipReason::NotPresent
        | SkipReason::NotAllowed
        | SkipReason::ModeOff
        | SkipReason::NoFileNeeded => false,
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

/// What one `agent.set_hooks` resolved to on this machine.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SetRequest {
    /// `None` when the list named **only** agents this build cannot
    /// wire: there is nothing to write, so the op answers `ok` with
    /// everything skipped and touches neither the key nor a dotfile.
    pub mode: Option<Mode>,
    /// The names this build has no [`Agent`] for, reported back as
    /// `skipped`.
    pub unknown: Vec<String>,
}

/// What `agent.set_hooks` asks for, or the `invalid-param` it is refused
/// with before anything is written (plan 064 §3.4, reshaped by 065
/// §3.1).
///
/// The same rule `session.set_agent_hooks` draws on a host: a name this
/// binary does not know is **skipped**, not a refusal (the why, once, in
/// `roost-session`'s `agent_hooks` module doc). What is still refused is
/// a request with no content: an empty list, or a blank element. The one
/// difference from the host op is that `off` arrives here as its own
/// wire spelling instead of as a word in the list.
pub(crate) fn resolve_set(agents: &AgentSetHooksAgents) -> Result<SetRequest, String> {
    let names = match agents {
        AgentSetHooksAgents::Off => {
            return Ok(SetRequest {
                mode: Some(Mode::Off),
                unknown: Vec::new(),
            })
        }
        AgentSetHooksAgents::List(names) => names,
    };
    // Checked before `resolve_names`, which skips blanks: skipping is
    // right for a human-typed CLI list and wrong on a wire, where a
    // blank element can only be a bug in the client.
    if names.iter().any(|name| name.trim().is_empty()) {
        return Err("agent.set_hooks: `agents` carries an empty name".to_string());
    }
    if names.is_empty() {
        return Err(format!(
            "agent.set_hooks requires a non-empty `agents` ({}) or the word \"off\"",
            roost_agent_install::agent_names()
        ));
    }
    let (agents, unknown) = roost_agent_install::resolve_names(names.iter().map(String::as_str));
    Ok(SetRequest {
        mode: (!agents.is_empty()).then_some(Mode::Allow(agents)),
        unknown,
    })
}

/// What one local `agent.set_hooks` did to this machine, on its way back
/// to the main thread.
pub(crate) struct AgentHooksSet {
    /// Which apply this is (plan 065 §3.5). The main thread takes the
    /// number at receipt and shows a *receipt* only for the newest, so
    /// a choice the user has already replaced says nothing — the
    /// [`AgentHooksSurvey::id`] rule, for the same reason. The key
    /// itself is not gated on it; see
    /// [`super::App::agent_hooks_applied`].
    pub ticket: u64,
    pub config_path: String,
    pub outcome: AgentHooksOutcome,
    /// The `agent-hooks` shadow this machine now has, from what was
    /// written ([`roost_agent_install::keys_for`]) rather than out of
    /// `outcome` — an outcome describes files, and the running UI's
    /// in-memory config has to match the key.
    pub key: AgentHooks,
    /// The `agent-integration` value written beside it.
    pub integration: Option<AgentIntegration>,
    /// The toast list, and what `mark_noticed` is then given. The names
    /// are already in [`AgentHooksOutcome::wired`]; the agents are kept
    /// beside them because that is what both of those take.
    pub unnoticed: Vec<Agent>,
    /// Set when the run failed after it wrote the key
    /// ([`roost_agent_install::InstallError::AfterKey`]): `key` is on
    /// disk, nothing else of the run is, and `outcome` is empty. The
    /// caller is answered with this error; the main thread still takes
    /// the key.
    pub error: Option<String>,
}

/// The blocking half of `agent.set_hooks`: the key write and the
/// reconcile, under the install lock.
///
/// Off the UI thread for this module's own reason — see its header.
/// Only a whole-run failure that wrote no key is an `Err`; a per-agent
/// one rides back in [`AgentHooksOutcome::errors`], because one
/// unparseable `config.toml` must not cost the user the answer they
/// just gave.
pub(crate) fn set_hooks_blocking(
    ticket: u64,
    mode: &Mode,
    guard: Guard,
) -> Result<AgentHooksSet, String> {
    let home = Home::from_env().map_err(|error| error.to_string())?;
    set_hooks_in(&home, ticket, mode, guard)
}

/// [`set_hooks_blocking`] against an explicit [`Home`] — the seam the
/// tests drive.
fn set_hooks_in(
    home: &Home,
    ticket: u64,
    mode: &Mode,
    guard: Guard,
) -> Result<AgentHooksSet, String> {
    let (integration, key) = roost_agent_install::keys_for(home, mode);
    let (outcome, error) = match roost_agent_install::set_hooks(home, mode, BY, guard) {
        Ok(outcome) => (outcome, None),
        Err(error @ InstallError::AfterKey { .. }) => (
            roost_agent_install::Outcome::default(),
            Some(error.to_string()),
        ),
        Err(error) => return Err(error.to_string()),
    };
    Ok(AgentHooksSet {
        ticket,
        config_path: home.config_path().display().to_string(),
        outcome: wire_outcome(&outcome),
        key,
        integration: AgentIntegration::parse(&integration),
        unnoticed: outcome.unnoticed,
        error,
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
/// directly — that also leaves behind any name this build cannot wire,
/// so a host is asked only for agents this client can name.
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
pub(crate) fn hooks_on_disk(fallback: &AgentHooks) -> AgentHooks {
    switches_on_disk(&AgentSwitches {
        integration: None,
        legacy: fallback.clone(),
    })
    .legacy
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
         {SETTINGS} in the command palette",
        names.join(", ")
    ))
}

/// This machine's agent-hooks toast: what it says, and the agents it
/// may spend `noticed` on.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AgentHooksToast {
    /// [`wired_toast`]'s sentence, when anything is announced.
    pub announcement: Option<String>,
    /// [`problem_line`]'s sentence, when anything was left unwired.
    pub problem: Option<String>,
    /// Exactly the agents `announcement` names — never a problem.
    pub noticed: Vec<Agent>,
    /// [`retire_line`]'s sentence, when the retire removed entries.
    pub retire: Option<String>,
    /// The agents `retire` names, whose retire is flipped to noticed.
    pub retired: Vec<Agent>,
    /// [`POLICY_LINE`], when it is due; showing it marks it noticed.
    pub policy: Option<&'static str>,
}

impl AgentHooksToast {
    /// One banner line, the announcement first.
    pub(crate) fn text(&self) -> String {
        [
            self.announcement.as_deref(),
            self.problem.as_deref(),
            self.retire.as_deref(),
            self.policy,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(". ")
    }
}

/// The toast `unnoticed` and `problems` earn, or `None` when there is
/// nothing to say.
///
/// An agent that is both is left out of the announcement: "wired" is not
/// true of it right now, and `mark_noticed` would spend the one
/// announcement it is owed once its file is fixed.
pub(crate) fn toast(unnoticed: Vec<Agent>, problems: &[HookProblem]) -> Option<AgentHooksToast> {
    let mut noticed = unnoticed;
    noticed.retain(|agent| !problems.iter().any(|p| p.agent == Some(*agent)));
    let announcement = wired_toast(&noticed, None);
    let problem = problem_line(problems);
    if announcement.is_none() && problem.is_none() {
        return None;
    }
    Some(AgentHooksToast {
        announcement,
        problem,
        noticed,
        ..AgentHooksToast::default()
    })
}

/// The agents `problems` name, in the announcement's order; a problem
/// that names no agent is counted instead.
fn problem_line(problems: &[HookProblem]) -> Option<String> {
    if problems.is_empty() {
        return None;
    }
    let names: Vec<&str> = ALL_AGENTS
        .into_iter()
        .filter(|agent| problems.iter().any(|p| p.agent == Some(*agent)))
        .map(Agent::source)
        .collect();
    let unnamed = problems.iter().filter(|p| p.agent.is_none()).count();
    let what = match (names.is_empty(), unnamed) {
        (false, 0) => names.join(", "),
        (true, n) => format!("agents ({})", plural(n, "problem")),
        (false, n) => format!("{} (+{})", names.join(", "), plural(n, "problem")),
    };
    Some(format!(
        "Agent hooks: couldn't set up {what} — run roostctl agent status"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(body: &str) -> RoostConfig {
        RoostConfig::parse(body)
    }

    /// An upgrader whose entries were retired hears what replaced them;
    /// one who had said `off` hears that the agents stay off.
    #[test]
    fn the_upgrade_toast_says_what_replaced_the_entries() {
        assert_eq!(retire_line(&[], 0, None), None);
        let line = retire_line(&[Agent::Claude, Agent::Codex], 0, None).unwrap();
        assert!(
            line.starts_with(
                "Claude Code now connects to Roost without changes to its settings files, \
                 and Codex reports its status on its own. Roost removed the entries"
            ),
            "{line}"
        );
        assert!(line.ends_with("Agent Integration… to change"), "{line}");

        let both = [Agent::Claude, Agent::Opencode];
        let off = retire_line(&[], 0, Some(&both)).unwrap();
        assert!(off.contains("connect Claude Code and OpenCode"), "{off}");
        assert!(off.contains("They stay off"), "{off}");
        assert!(!off.contains("removed"), "nothing was removed: {off}");
        let off = retire_line(&[Agent::Claude], 2, Some(&both)).unwrap();
        assert!(off.contains("Roost also removed hook entries"), "{off}");
        assert!(off.ends_with("roostctl agent status lists them"), "{off}");
    }

    /// The "they stay off" notice names only the installed agents Roost
    /// could connect, and says nothing when there are none.
    #[test]
    fn the_stay_off_notice_names_only_installed_agents() {
        let off = retire_line(&[], 0, Some(&[Agent::Opencode])).unwrap();
        assert!(
            off.starts_with(
                "Roost can now connect OpenCode without editing its settings files. It stays off"
            ),
            "{off}"
        );
        assert!(!off.contains("Claude"), "{off}");
        assert_eq!(retire_line(&[], 0, Some(&[])), None);
        let removed = retire_line(&[Agent::Codex], 0, Some(&[])).unwrap();
        assert!(!removed.contains("stay off"), "{removed}");
        assert!(
            removed.starts_with("Roost removed hook entries"),
            "{removed}"
        );
    }

    /// The policy notice joins the one banner, after the rest.
    #[test]
    fn the_policy_notice_rides_the_banner() {
        let toast = AgentHooksToast {
            retire: retire_line(&[Agent::Grok], 0, None),
            policy: Some(POLICY_LINE),
            ..AgentHooksToast::default()
        };
        assert!(toast.text().ends_with(POLICY_LINE), "{}", toast.text());
        assert!(toast.text().starts_with("Grok reports"), "{}", toast.text());
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

    fn set(agents: &[&str]) -> Result<SetRequest, String> {
        resolve_set(&AgentSetHooksAgents::List(
            agents.iter().map(|s| (*s).to_string()).collect(),
        ))
    }

    fn resolved(mode: Option<Mode>, unknown: &[&str]) -> Result<SetRequest, String> {
        Ok(SetRequest {
            mode,
            unknown: unknown.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    #[test]
    fn agent_set_hooks_takes_a_list_or_the_word_off() {
        assert_eq!(
            set(&["claude", "codex"]),
            resolved(Some(Mode::Allow(vec![Agent::Claude, Agent::Codex])), &[])
        );
        assert_eq!(
            resolve_set(&AgentSetHooksAgents::Off),
            resolved(Some(Mode::Off), &[])
        );
    }

    /// A name this build cannot wire is skipped and the rest is applied
    /// (plan 065 §3.1). A list of *only* such names resolves to no mode
    /// at all, which is how the op answers `ok` having written nothing.
    #[test]
    fn agent_set_hooks_skips_the_names_this_build_cannot_wire() {
        assert_eq!(
            set(&["claude", "gemini"]),
            resolved(Some(Mode::Allow(vec![Agent::Claude])), &["gemini"])
        );
        assert_eq!(set(&["gemini", "amp"]), resolved(None, &["gemini", "amp"]));
    }

    /// The two shapes that are bugs in the caller rather than answers.
    #[test]
    fn agent_set_hooks_refuses_an_empty_list_or_a_blank_name() {
        assert!(set(&[]).unwrap_err().contains("non-empty"));
        assert!(set(&["  "]).unwrap_err().contains("empty name"));
        assert!(set(&["claude", ""]).unwrap_err().contains("empty name"));
        assert!(set(&["gemini", ""]).unwrap_err().contains("empty name"));
    }

    /// #491: a set that fails after its key write is still a result —
    /// the main thread has to take the key — carrying the error that
    /// names it.
    #[test]
    fn a_set_that_fails_after_the_key_write_still_carries_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        // A directory where the state record belongs: read only after
        // the key is written.
        std::fs::create_dir_all(home.record_path()).unwrap();
        let mode = Mode::Allow(vec![Agent::Claude, Agent::Cursor]);

        let done = set_hooks_in(&home, 7, &mode, Guard::PERMITTED)
            .unwrap_or_else(|error| panic!("the key is on disk, so this is a result: {error}"));
        let error = done
            .error
            .as_deref()
            .expect("the failure rides with the key");
        assert!(
            error.contains(
                "the agent-integration key `claude, -codex, -grok, cursor, -opencode` is \
                 already on disk"
            ),
            "{error}"
        );
        let text = std::fs::read_to_string(home.config_path()).expect("config.conf");
        assert_eq!(done.key, RoostConfig::parse(&text).agent_hooks);
        assert_eq!(done.outcome, AgentHooksOutcome::default());
        assert!(done.unnoticed.is_empty());
    }

    /// A refusal before the key write stays an `Err`: there is no key to
    /// take.
    #[test]
    fn a_set_refused_before_the_key_write_is_not_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        let fenced = Guard {
            test_mode: true,
            forced: false,
        };

        let refused = set_hooks_in(&home, 1, &Mode::Allow(vec![Agent::Claude]), fenced);
        assert!(refused.is_err());
        assert!(!home.config_path().exists());
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
        assert!(toast.contains("Agent Integration…"), "{toast}");
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
        assert_eq!(toast(Vec::new(), &[]), None);
    }

    fn problem(agent: Option<Agent>) -> HookProblem {
        HookProblem {
            agent,
            detail: "settings.json: unreadable".to_string(),
        }
    }

    #[test]
    fn a_clean_startup_toast_is_the_announcement_alone() {
        let shown = toast(vec![Agent::Claude, Agent::Codex], &[]).unwrap();
        assert_eq!(shown.problem, None);
        assert_eq!(
            shown.announcement,
            wired_toast(&[Agent::Claude, Agent::Codex], None)
        );
        assert_eq!(Some(shown.text()), shown.announcement);
        assert_eq!(shown.noticed, vec![Agent::Claude, Agent::Codex]);
    }

    #[test]
    fn a_startup_that_only_failed_names_the_agents_and_notices_none() {
        let shown = toast(
            Vec::new(),
            &[problem(Some(Agent::Cursor)), problem(Some(Agent::Claude))],
        )
        .unwrap();
        assert_eq!(shown.announcement, None);
        assert_eq!(
            shown.text(),
            "Agent hooks: couldn't set up claude, cursor — run roostctl agent status"
        );
        assert!(shown.noticed.is_empty());
    }

    #[test]
    fn a_startup_with_both_says_both_on_one_line_and_notices_only_the_announced() {
        let shown = toast(
            vec![Agent::Claude, Agent::Codex, Agent::Grok],
            &[problem(Some(Agent::Claude)), problem(Some(Agent::Cursor))],
        )
        .unwrap();
        assert_eq!(
            shown.text(),
            format!(
                "{}. Agent hooks: couldn't set up claude, cursor — run roostctl agent status",
                wired_toast(&[Agent::Codex, Agent::Grok], None).unwrap()
            )
        );
        assert!(!shown.text().contains('\n'));
        assert_eq!(shown.noticed, vec![Agent::Codex, Agent::Grok]);
    }

    #[test]
    fn a_failure_that_names_no_agent_is_counted() {
        assert_eq!(
            toast(Vec::new(), &[problem(None)]).unwrap().text(),
            "Agent hooks: couldn't set up agents (1 problem) — run roostctl agent status"
        );
        assert_eq!(
            problem_line(&[problem(Some(Agent::Grok)), problem(None), problem(None)]).unwrap(),
            "Agent hooks: couldn't set up grok (+2 problems) — run roostctl agent status"
        );
    }

    #[test]
    fn only_failures_and_file_defects_are_problems() {
        let path = std::path::PathBuf::from("/x");
        let skip = |agent, reason| roost_agent_install::AgentSkip { agent, reason };
        let outcome = Outcome {
            skipped: vec![
                skip(Agent::Claude, SkipReason::NotPresent),
                skip(Agent::Claude, SkipReason::NotAllowed),
                skip(Agent::Claude, SkipReason::ModeOff),
                skip(
                    Agent::Codex,
                    SkipReason::Unparseable {
                        path: path.clone(),
                        detail: "eof".to_string(),
                    },
                ),
                skip(
                    Agent::Grok,
                    SkipReason::UnexpectedShape {
                        path: path.clone(),
                        detail: "array".to_string(),
                    },
                ),
                skip(Agent::Opencode, SkipReason::ForeignFile { path }),
            ],
            errors: vec![roost_agent_install::AgentError {
                agent: Agent::Cursor,
                error: InstallError::NoHome,
            }],
            ..Outcome::default()
        };
        let agents: Vec<Option<Agent>> = problems(&outcome).iter().map(|p| p.agent).collect();
        assert_eq!(
            agents,
            vec![
                Some(Agent::Cursor),
                Some(Agent::Codex),
                Some(Agent::Grok),
                Some(Agent::Opencode)
            ]
        );
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

    /// The consent card is a *startup* act too, and `window_opened`
    /// runs on every focus **and** unfocus — so without its own latch an
    /// Alt-Tab back to the window would put a second card over the one
    /// the user is reading.
    #[test]
    fn the_first_run_card_is_raised_once_per_process() {
        let mut raised = false;
        assert!(claim_first_run(&mut raised, &Start::Ask, Guard::PERMITTED));
        assert!(raised);
        assert!(
            !claim_first_run(&mut raised, &Start::Ask, Guard::PERMITTED),
            "the focus that follows the open re-raised the card"
        );
        assert!(
            !claim_first_run(&mut raised, &Start::Ask, Guard::PERMITTED),
            "and so did an ordinary Alt-Tab"
        );
    }

    /// Only an unanswered key has anything to ask about, and a UI under
    /// the harness fence asks nothing at all unless the harness lifted
    /// it — the same [`Guard`] the install engine checks, read before
    /// the card is raised rather than after it has been answered.
    #[test]
    fn nothing_but_an_unanswered_key_outside_the_fence_raises_the_card() {
        for start in [Start::Run, Start::Off, Start::Already] {
            let mut raised = false;
            assert!(
                !claim_first_run(&mut raised, &start, Guard::PERMITTED),
                "{start:?} raised the consent card"
            );
            assert!(!raised, "{start:?} spent the latch");
        }

        let fenced = Guard {
            test_mode: true,
            forced: false,
        };
        let mut raised = false;
        assert!(!claim_first_run(&mut raised, &Start::Ask, fenced));
        assert!(!raised, "the fence spent the latch it refused to use");
        let forced = Guard {
            test_mode: true,
            forced: true,
        };
        assert!(
            claim_first_run(&mut raised, &Start::Ask, forced),
            "ROOST_AGENT_HOOKS_FORCE=1 is what lets the E2E see the card"
        );
    }

    /// A card that could not be shown does not spend the one ask.
    ///
    /// The latch stops Alt-Tab asking twice; it must not swallow the
    /// prompt because another dialog happened to be up when the survey
    /// came back. `App::agent_hooks_surveyed` puts it back for exactly
    /// that case.
    #[test]
    fn a_released_latch_asks_again_in_the_same_process() {
        let mut raised = false;
        assert!(claim_first_run(&mut raised, &Start::Ask, Guard::PERMITTED));
        assert!(raised);
        // What the `ScreenTaken` arm does.
        raised = false;
        assert!(
            claim_first_run(&mut raised, &Start::Ask, Guard::PERMITTED),
            "a released latch did not ask again"
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
