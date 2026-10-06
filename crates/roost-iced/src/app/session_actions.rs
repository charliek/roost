//! Install Update and Restart Session (plan 076 D7, D8): who may act on
//! a host's session right now, the localhost restart, and what
//! `host.status` reports about the latest action.
//!
//! Every surface lands on the same two requests — a palette row, a menu
//! row, the band's pill, and the `host.update` / `host.restart` ops. A
//! person gets a card first; an op holds its claim from the start and
//! acts without one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use roost_engine::ipc::HostOpFailure;
use roost_ipc::agent::{AgentLifecycle, ShellState};
use roost_ipc::bootstrap::{ProbeOutcome, RemoteArch};
use roost_ipc::codes;
use roost_ipc::messages::{host_action_kind, host_action_phase, HostActionStatus, SessionIdentify};
use roost_ipc::session_version::BuildId;
use roost_ui_model::host_sidebar::{self, FidelityAction, HostTransportKind, SectionState};
use roost_ui_model::session_update::{self, describe, RestartTarget, TargetKnowledge};

use super::bootstrap::{self, ActionTicket, OfferContext, ProbeIntent, SessionState};
use super::{host_dialog, host_notice, servicing, App};
use crate::host_conn::{HostConnState, RequestOrigin};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionKind {
    Install,
    Restart,
    /// Install, then restart: the refused ssh session's one fix.
    Update,
}

impl ActionKind {
    fn wire(self) -> &'static str {
        match self {
            Self::Install => host_action_kind::INSTALL,
            Self::Restart => host_action_kind::RESTART,
            Self::Update => host_action_kind::UPDATE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionRecord {
    kind: ActionKind,
    phase: Phase,
    message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    generation: u64,
    session_id: String,
    /// Who asked: a person can be shown a card about what follows, a
    /// machine never is.
    origin: RequestOrigin,
    /// Set for a localhost restart once its stop half is under way.
    target: Option<RestartTarget>,
    /// Whether the action reached its stop, letting go of the host's
    /// live stream if it had one: from then on the connection state on
    /// record is stale, and the action has to take the host back however
    /// it ends.
    released_stream: bool,
    /// A localhost restart past its stop, waiting for the relaunch to
    /// land.
    relaunching: bool,
    /// A job or worker is out on the action's behalf and will report
    /// back: until it does, the claim cannot be let go.
    working: bool,
    /// The action was given up on while its worker was out. It is
    /// recorded as failed already; the claim stays held, keeping every
    /// other action and Connect off the host, until the worker's
    /// completion arrives and is discarded.
    cancelled: bool,
}

/// One action per session at a time (D8), and the latest one per host.
///
/// Claims are keyed by saved host and also matched by session id, so a
/// localhost row and an ssh-to-self row naming one session cannot both
/// act. A completion carries the generation it was started under and is
/// ignored once that claim is gone.
#[derive(Debug, Default)]
pub(crate) struct HostActions {
    claims: HashMap<String, Claim>,
    latest: HashMap<String, ActionRecord>,
    /// The update facts last learned about each host with an action on
    /// record, so `host.status` can still report the action while the
    /// host is down — which is exactly when a restart's outcome matters.
    facts: HashMap<String, session_update::UpdateFacts>,
}

impl HostActions {
    pub(crate) fn claimed(&self, saved_id: &str) -> bool {
        self.claims.contains_key(saved_id)
    }

    pub(crate) fn busy(&self, saved_id: &str, session_id: &str) -> bool {
        self.claims.contains_key(saved_id)
            || self
                .claims
                .values()
                .any(|claim| claim.session_id == session_id)
    }

    fn begin(
        &mut self,
        saved_id: &str,
        generation: u64,
        kind: ActionKind,
        session_id: &str,
        origin: RequestOrigin,
    ) -> bool {
        if self.busy(saved_id, session_id) {
            return false;
        }
        self.claims.insert(
            saved_id.to_string(),
            Claim {
                generation,
                session_id: session_id.to_string(),
                origin,
                target: None,
                released_stream: false,
                relaunching: false,
                working: false,
                cancelled: false,
            },
        );
        self.latest.insert(
            saved_id.to_string(),
            ActionRecord {
                kind,
                phase: Phase::Running,
                message: None,
            },
        );
        true
    }

    /// The live claim on this host: one that has not been cancelled.
    fn live(&self, saved_id: &str) -> Option<&Claim> {
        self.claims.get(saved_id).filter(|claim| !claim.cancelled)
    }

    fn holds(&self, saved_id: &str, generation: u64) -> bool {
        self.live(saved_id)
            .is_some_and(|claim| claim.generation == generation)
    }

    fn claim_mut(&mut self, saved_id: &str, generation: u64) -> Option<&mut Claim> {
        self.claims
            .get_mut(saved_id)
            .filter(|claim| claim.generation == generation && !claim.cancelled)
    }

    /// Whether the claim on this host is a cancelled one waiting for its
    /// worker.
    pub(crate) fn cancelling(&self, saved_id: &str) -> bool {
        self.claims
            .get(saved_id)
            .is_some_and(|claim| claim.cancelled)
    }

    fn set_working(&mut self, saved_id: &str, generation: u64, working: bool) {
        if let Some(claim) = self.claim_mut(saved_id, generation) {
            claim.working = working;
        }
    }

    /// A relaunch's connection settled. Still trying is nothing to act
    /// on; settling anywhere else ends the restart through
    /// [`Self::give_up_relaunch`]. `None` when there is no relaunch
    /// pending or it is still trying.
    fn relaunch_settled(
        &mut self,
        saved_id: &str,
        state: Option<&HostConnState>,
        message: &str,
        launches_out: impl FnOnce() -> bool,
    ) -> Option<bool> {
        let (generation, ..) = self.relaunching(saved_id)?;
        let retrying = match state {
            Some(HostConnState::Connecting { .. } | HostConnState::Connected) => true,
            Some(HostConnState::Disconnected(disconnected)) => disconnected.retry_in.is_some(),
            _ => false,
        };
        if retrying {
            return None;
        }
        self.give_up_relaunch(saved_id, generation, message, launches_out)
    }

    /// A relaunch still pending under `generation`, given up on the way a
    /// Disconnect gives up on an action — its deadline passed, or its
    /// connection settled short of the session: recorded failed now, its
    /// claim held while a launch is out. `None` when nothing is pending
    /// under `generation`; otherwise whether the claim went now.
    fn give_up_relaunch(
        &mut self,
        saved_id: &str,
        generation: u64,
        message: &str,
        launches_out: impl FnOnce() -> bool,
    ) -> Option<bool> {
        if !self.relaunch_pending(saved_id, generation) {
            return None;
        }
        self.abandon(saved_id, generation, message, launches_out);
        Some(!self.claims.contains_key(saved_id))
    }

    /// Give up on `generation`'s action as failed. A relaunch's launches
    /// are counted by its pin's gate, which `launches_out` revokes: the
    /// claim is held while one is still out.
    fn abandon(
        &mut self,
        saved_id: &str,
        generation: u64,
        message: &str,
        launches_out: impl FnOnce() -> bool,
    ) -> bool {
        if self.relaunching(saved_id).is_some() && launches_out() {
            self.set_working(saved_id, generation, true);
        }
        self.cancel(saved_id, generation, message)
    }

    /// Give up on the action holding this host, recording it as failed.
    /// With nothing out on its behalf the claim goes now; otherwise it is
    /// held, cancelled, until [`Self::settle_cancelled`].
    fn cancel(&mut self, saved_id: &str, generation: u64, message: &str) -> bool {
        let Some(claim) = self.claim_mut(saved_id, generation) else {
            return false;
        };
        if claim.working {
            claim.cancelled = true;
        } else {
            self.claims.remove(saved_id);
        }
        self.record(saved_id, Phase::Failed, message);
        true
    }

    /// A cancelled action's worker reported: let the claim go, and
    /// nothing else. `false` when `generation` holds no cancelled claim.
    fn settle_cancelled(&mut self, saved_id: &str, generation: u64) -> bool {
        let cancelled = self
            .claims
            .get(saved_id)
            .is_some_and(|claim| claim.generation == generation && claim.cancelled);
        if cancelled {
            self.claims.remove(saved_id);
        }
        cancelled
    }

    fn record(&mut self, saved_id: &str, phase: Phase, message: &str) {
        if let Some(record) = self.latest.get_mut(saved_id) {
            record.phase = phase;
            record.message = Some(message.to_string());
        }
    }

    /// End the action, if `generation` is the one holding the claim.
    fn finish(
        &mut self,
        saved_id: &str,
        generation: u64,
        outcome: &Result<String, String>,
    ) -> bool {
        if !self.holds(saved_id, generation) {
            return false;
        }
        self.claims.remove(saved_id);
        match outcome {
            Ok(message) => self.record(saved_id, Phase::Done, message),
            Err(message) => self.record(saved_id, Phase::Failed, message),
        }
        true
    }

    /// The relaunch's generation, the session it replaces, and its
    /// target.
    fn relaunching(&self, saved_id: &str) -> Option<(u64, String, RestartTarget)> {
        let claim = self.live(saved_id).filter(|claim| claim.relaunching)?;
        Some((
            claim.generation,
            claim.session_id.clone(),
            claim.target.clone()?,
        ))
    }

    /// Whether `generation` is the restart still waiting on its relaunch.
    fn relaunch_pending(&self, saved_id: &str, generation: u64) -> bool {
        self.relaunching(saved_id)
            .is_some_and(|(pending, ..)| pending == generation)
    }

    /// Who started the action holding this host's claim, and whether
    /// it let go of the stream.
    fn held(&self, saved_id: &str, generation: u64) -> Option<(RequestOrigin, bool)> {
        let claim = self.live(saved_id)?;
        (claim.generation == generation).then_some((claim.origin, claim.released_stream))
    }

    /// The generation of the live claim on this host, whatever it is
    /// doing.
    fn generation(&self, saved_id: &str) -> Option<u64> {
        self.live(saved_id).map(|claim| claim.generation)
    }

    fn remember(&mut self, saved_id: &str, facts: session_update::UpdateFacts) {
        self.facts.insert(saved_id.to_string(), facts);
    }

    /// `host.status`'s `update` for a host: its current facts, or — while
    /// an action is on record and the host has none — the last ones
    /// learned. The action rides along either way.
    pub(crate) fn update_status(
        &self,
        saved_id: &str,
        current: Option<&session_update::UpdateFacts>,
    ) -> Option<roost_ipc::messages::HostUpdateStatus> {
        let action = self.status(saved_id);
        let facts = current.or_else(|| action.as_ref().and(self.facts.get(saved_id)))?;
        let mut status = facts.status();
        status.action = action;
        Some(status)
    }

    pub(crate) fn status(&self, saved_id: &str) -> Option<HostActionStatus> {
        self.latest.get(saved_id).map(|record| HostActionStatus {
            kind: record.kind.wire().to_string(),
            phase: match record.phase {
                Phase::Running => host_action_phase::RUNNING,
                Phase::Done => host_action_phase::DONE,
                Phase::Failed => host_action_phase::FAILED,
            }
            .to_string(),
            message: record.message.clone(),
        })
    }

    pub(crate) fn forget(&mut self, saved_id: &str) {
        self.claims.remove(saved_id);
        self.latest.remove(saved_id);
        self.facts.remove(saved_id);
    }
}

/// A Restart card's plan: the session it was built against and the
/// binary it would run (D8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestartPlan {
    Local {
        session_id: String,
        target: RestartTarget,
    },
    Ssh {
        session_id: String,
        /// [`roost_ipc::ssh::SshTarget::token`] when the card opened.
        token: String,
        arch: RemoteArch,
        outcome: ProbeOutcome,
        target: RestartTarget,
    },
}

impl RestartPlan {
    fn session_id(&self) -> &str {
        match self {
            Self::Local { session_id, .. } | Self::Ssh { session_id, .. } => session_id,
        }
    }
}

/// A localhost restart step reporting back through the engine feed.
#[derive(Debug)]
pub(crate) enum ActionEvent {
    /// The restart's target was identified.
    Resolved {
        saved_id: String,
        session_id: String,
        /// The op's claim, when an op asked; `None` raises the card.
        ticket: Option<u64>,
        restart: TargetKnowledge,
    },
    /// The stop half answered.
    Stopped {
        saved_id: String,
        generation: u64,
        result: Result<(), LocalFailure>,
    },
    /// A restart has re-checked what it stops and asks for the live
    /// stream to be let go before it stops it. Dropping `ack` refuses.
    ReleaseStream {
        saved_id: String,
        generation: u64,
        ack: tokio::sync::oneshot::Sender<()>,
    },
    /// The last launch out from a restart's pin has been reaped.
    LaunchSettled { saved_id: String, generation: u64 },
    /// [`RELAUNCH_DEADLINE`] passed since a localhost restart began its
    /// relaunch.
    RelaunchExpired { saved_id: String, generation: u64 },
}

/// How long a localhost restart's relaunch may take to land a verified
/// session: the launcher's own verdict and confirm budgets, plus
/// [`RELAUNCH_ATTACH_ALLOWANCE`] for the connect and identify behind
/// them. Past it the action fails, and the connection carries on under
/// its ordinary retry policy, which dials and never spawns.
pub(crate) const RELAUNCH_DEADLINE: std::time::Duration =
    roost_ipc::session_launch::DEFAULT_VERDICT_BUDGET
        .saturating_add(roost_ipc::session_launch::DEFAULT_CONFIRM_BUDGET)
        .saturating_add(RELAUNCH_ATTACH_ALLOWANCE);

const RELAUNCH_ATTACH_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(15);

/// How a restart's job asks the UI thread to drop the host's stream, so
/// the stop is never heard by a client that would race the relaunch —
/// and only once everything it stops has been re-checked, so an abort
/// leaves the connection as it was (plan 076 D8).
pub(crate) struct StreamRelease {
    feed: crate::engine_feed::EngineFeedSender,
    saved_id: String,
    generation: u64,
}

impl StreamRelease {
    /// Answers once the stream is gone. `Changed` when the action no
    /// longer holds its claim: something cancelled it.
    pub(crate) async fn request(&self) -> Result<(), roost_ipc::bootstrap::BootstrapError> {
        let (ack, answered) = tokio::sync::oneshot::channel();
        self.feed
            .send(crate::engine_feed::EngineFeed::HostAction(Box::new(
                ActionEvent::ReleaseStream {
                    saved_id: self.saved_id.clone(),
                    generation: self.generation,
                    ack,
                },
            )));
        answered
            .await
            .map_err(|_| roost_ipc::bootstrap::BootstrapError::Changed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalFailure {
    /// Revalidation found another session, or another binary: nothing
    /// was stopped.
    Changed,
    Stop(String),
}

const CHANGED: &str = "the session changed; check again";

fn busy(label: &str) -> HostOpFailure {
    HostOpFailure::new(
        codes::BUSY,
        format!("{label} is already being updated or restarted"),
    )
}

async fn identify_running(socket: &Path) -> Option<SessionIdentify> {
    let budget = roost_ipc::session_launch::DEFAULT_STOP_CALL_BUDGET
        .mul_f64(roost_ipc::session_launch::timeout_scale());
    tokio::time::timeout(budget, roost_ipc::session_launch::identify(socket, budget))
        .await
        .ok()?
        .ok()
}

async fn identify_target(path: &str) -> Option<BuildId> {
    roost_ipc::bootstrap::local_identity(Path::new(path))
        .await
        .ok()
        .map(|identity| BuildId::from(&identity))
}

/// The stop half of a localhost restart (D8): the session and the
/// target are re-checked before the stream is let go, so an abort leaves
/// the connection as it was; after that only the stop, which is itself
/// refused unless the session that answers is the one named. The spawn
/// pin re-checks the target right before the relaunch.
async fn stop_for_restart(
    socket: PathBuf,
    session_id: String,
    target: RestartTarget,
    release: StreamRelease,
) -> Result<(), LocalFailure> {
    let serving = identify_running(&socket).await;
    if serving.map(|serving| serving.session_id).as_deref() != Some(session_id.as_str()) {
        return Err(LocalFailure::Changed);
    }
    if identify_target(&target.path).await.as_ref() != Some(&target.identity) {
        return Err(LocalFailure::Changed);
    }
    release.request().await.map_err(|_| LocalFailure::Changed)?;
    let stopped = crate::host_conn::restart::stop_and_wait(&socket, Some(&session_id))
        .await
        .map_err(|failure| LocalFailure::Stop(failure.to_string()))?;
    if !stopped {
        return Err(LocalFailure::Changed);
    }
    Ok(())
}

/// [`RelaunchExpired`](ActionEvent::RelaunchExpired), `after` from now.
async fn relaunch_deadline(
    feed: crate::engine_feed::EngineFeedSender,
    saved_id: String,
    generation: u64,
    after: std::time::Duration,
) {
    tokio::time::sleep(after).await;
    feed.send(crate::engine_feed::EngineFeed::HostAction(Box::new(
        ActionEvent::RelaunchExpired {
            saved_id,
            generation,
        },
    )));
}

/// The titles of the tabs a restart would end: a foreground program, or
/// an agent mid-turn.
fn running_programs<'a>(
    tabs: impl IntoIterator<Item = (&'a str, ShellState, AgentLifecycle)>,
) -> Vec<String> {
    tabs.into_iter()
        .filter(|(_, shell, agent)| {
            *shell == ShellState::ForegroundProcess
                || matches!(agent, AgentLifecycle::Working | AgentLifecycle::Waiting)
        })
        .map(|(title, ..)| title.to_string())
        .collect()
}

/// The exec rung's build, where the probe could read one.
pub(super) fn rung_build(outcome: &ProbeOutcome) -> Option<BuildId> {
    match outcome {
        ProbeOutcome::Compatible { identity, .. }
        | ProbeOutcome::Mismatch {
            identity: Some(identity),
            ..
        } => Some(BuildId::from(identity)),
        _ => None,
    }
}

impl App {
    /// The session an action on this host is bound to: a live
    /// connection's, or a refused one's.
    pub(super) fn acting_session(
        &self,
        saved_id: &str,
    ) -> Option<(String, BuildId, Option<String>)> {
        match self.hosts.state(saved_id)? {
            HostConnState::Connected => self.hosts.facts(saved_id).map(|facts| {
                (
                    facts.session_id.clone(),
                    facts.running.clone(),
                    facts.exe_path.clone(),
                )
            }),
            HostConnState::NeedsRestart(mismatch) => Some((
                mismatch.session_id.clone(),
                mismatch.running.clone(),
                mismatch.exe_path.clone(),
            )),
            _ => None,
        }
    }

    /// The saved host and its ssh target, if it still names the machine
    /// `token` was taken from.
    pub(super) fn same_ssh_host(
        &self,
        saved_id: &str,
        token: &str,
    ) -> Option<(
        roost_engine::persistence::HostSnapshot,
        roost_ipc::ssh::SshTarget,
    )> {
        let host = self.saved_host(saved_id).ok()?;
        match roost_ipc::ssh::classify(&host.target) {
            Ok(roost_ipc::ssh::ResolvedTransport::Ssh(live)) if live.token == token => {
                Some((host, live))
            }
            _ => None,
        }
    }

    fn refuse_busy(
        &self,
        saved_id: &str,
        session_id: &str,
        label: &str,
    ) -> Result<(), HostOpFailure> {
        let ssh_job = self.saved_host(saved_id).ok().is_some_and(|host| {
            matches!(
                roost_ipc::ssh::classify(&host.target),
                Ok(roost_ipc::ssh::ResolvedTransport::Ssh(target))
                    if self.bootstraps.job_running(&target.claim_key)
            )
        });
        if self.host_actions.busy(saved_id, session_id)
            || self.bootstraps.probing(saved_id)
            || ssh_job
        {
            return Err(busy(label));
        }
        Ok(())
    }

    pub(super) fn claim_action(
        &mut self,
        saved_id: &str,
        kind: ActionKind,
        session_id: &str,
        label: &str,
        origin: RequestOrigin,
    ) -> Result<u64, HostOpFailure> {
        self.refuse_busy(saved_id, session_id, label)?;
        let generation = self.take_engine_op_id();
        if !self
            .host_actions
            .begin(saved_id, generation, kind, session_id, origin)
        {
            return Err(busy(label));
        }
        self.refresh_action_facts(saved_id);
        Ok(generation)
    }

    /// Once a host has an action on record, the latest update facts
    /// learned about it — not the ones the action started from — are
    /// what `host.status` falls back on while it is down.
    pub(super) fn refresh_action_facts(&mut self, saved_id: &str) {
        if self.host_actions.status(saved_id).is_none() {
            return;
        }
        let facts = self
            .saved_host(saved_id)
            .ok()
            .and_then(|host| self.host_update_facts(saved_id, &host.target));
        if let Some(facts) = facts {
            self.host_actions.remember(saved_id, facts);
        }
    }

    /// D7's `busy`, answered before anything else about the host: while
    /// an action holds it — the disconnected stretch of a restart
    /// included — nothing else is offered.
    fn refuse_claimed(&self, saved_id: &str, label: &str) -> Result<(), HostOpFailure> {
        if self.host_actions.claimed(saved_id) {
            return Err(busy(label));
        }
        Ok(())
    }

    /// End whatever action holds this host, as failed: the person moved
    /// the host out from under it. A worker already out keeps the claim
    /// until it reports, so nothing can act on — or connect to — the host
    /// while it may still be stopping or starting a session there.
    pub(super) fn cancel_host_action(&mut self, saved_id: &str, why: &str) {
        let Some(generation) = self.host_actions.generation(saved_id) else {
            return;
        };
        let message = format!("{}: {why}", self.label_or_id(saved_id));
        let hosts = &self.hosts;
        if self
            .host_actions
            .abandon(saved_id, generation, &message, || {
                hosts.revoke_spawn(saved_id)
            })
        {
            self.action_ended(saved_id, Err(message));
        }
    }

    /// Let go of a cancelled action's claim once its worker reported.
    /// Whatever the worker did, the cancellation already said what there
    /// is to say about it.
    pub(super) fn settle_cancelled_action(&mut self, saved_id: &str, generation: u64) -> bool {
        if !self.host_actions.settle_cancelled(saved_id, generation) {
            return false;
        }
        tracing::debug!(host = %saved_id, generation, "a cancelled action's worker reported");
        self.reconcile();
        true
    }

    pub(super) fn action_working(&mut self, saved_id: &str, generation: u64) {
        self.host_actions.set_working(saved_id, generation, true);
    }

    /// A Connect while an action holds the host, in any phase, would
    /// either cancel it or race it — a stop under a fresh attachment, a
    /// spawn of a build nobody agreed to — so it is refused instead.
    /// Disconnect stays the way to give up on an action.
    pub(super) fn refuse_connect_during_action(&self, saved_id: &str) -> Result<(), HostOpFailure> {
        if !self.host_actions.claimed(saved_id) {
            return Ok(());
        }
        let label = self.label_or_id(saved_id);
        let message = if self.host_actions.cancelling(saved_id) {
            format!("a restart/update on {label} is being cancelled")
        } else {
            format!("a restart/update is in progress on {label}")
        };
        Err(HostOpFailure::new(codes::BUSY, message))
    }

    fn refuse_dialog_open(&self) -> Result<(), HostOpFailure> {
        if self.host_dialog.is_some() {
            return Err(HostOpFailure::new(
                codes::BUSY,
                "Close the open dialog, then try again.",
            ));
        }
        Ok(())
    }

    /// An op claims now; a person's request only checks it could, since
    /// its claim is taken when the card is confirmed.
    fn reserve(
        &mut self,
        saved_id: &str,
        kind: ActionKind,
        session_id: &str,
        label: &str,
        ticketed: bool,
    ) -> Result<Option<u64>, HostOpFailure> {
        if ticketed {
            return self
                .claim_action(saved_id, kind, session_id, label, RequestOrigin::Ipc)
                .map(Some);
        }
        self.refuse_busy(saved_id, session_id, label)?;
        self.refuse_dialog_open()?;
        Ok(None)
    }

    /// End an action: its record, the band's line, and the status bar.
    pub(super) fn finish_action(
        &mut self,
        saved_id: &str,
        generation: u64,
        outcome: Result<String, String>,
    ) {
        if !self.host_actions.finish(saved_id, generation, &outcome) {
            tracing::debug!(host = %saved_id, generation, "dropped a superseded action completion");
            return;
        }
        self.action_ended(saved_id, outcome);
    }

    /// What every end of an action owes the band, the status bar and the
    /// spawn pin, once its record is written.
    fn action_ended(&mut self, saved_id: &str, outcome: Result<String, String>) {
        self.hosts.unpin_spawn(saved_id);
        let connected = matches!(self.hosts.state(saved_id), Some(HostConnState::Connected));
        match &outcome {
            Ok(message) => {
                tracing::info!(host = %saved_id, %message, "host action done");
                self.hosts.set_bootstrap_note(saved_id, None);
            }
            Err(message) => {
                tracing::warn!(host = %saved_id, %message, "host action failed");
                let note =
                    (!connected && self.saved_host(saved_id).is_ok()).then(|| message.clone());
                self.hosts.set_bootstrap_note(saved_id, note);
            }
        }
        self.refresh_action_facts(saved_id);
        self.set_status(outcome.unwrap_or_else(|message| message));
        self.reconcile();
    }

    /// Install Update…, from any surface. `ticketed` is an op: it claims
    /// now and never raises a card.
    pub(crate) fn host_install_requested(
        &mut self,
        saved_id: &str,
        ticketed: bool,
    ) -> Result<(), HostOpFailure> {
        let host = self
            .saved_host(saved_id)
            .map_err(|error| HostOpFailure::new(codes::NOT_FOUND, error.to_string()))?;
        let transport = servicing::transport_kind(&host.target);
        if transport == HostTransportKind::Socket {
            return Err(HostOpFailure::new(
                codes::NOT_SUPPORTED,
                format!("{} is reached through a socket; its roost-session is not this Roost's to install", host.label),
            ));
        }
        self.refuse_claimed(saved_id, &host.label)?;
        let facts = self
            .host_update_facts(saved_id, &host.target)
            .ok_or_else(|| {
                HostOpFailure::new(
                    codes::INVALID_PARAM,
                    format!("{} is not connected", host.label),
                )
            })?;
        session_update::install_offer(&facts, transport).map_err(|refusal| {
            HostOpFailure::new(
                codes::INVALID_PARAM,
                format!(
                    "Install Update is not offered on {}: {}",
                    host.label,
                    refusal.reason()
                ),
            )
        })?;
        let Some((session_id, ..)) = self.acting_session(saved_id) else {
            return Err(HostOpFailure::new(
                codes::INVALID_PARAM,
                format!("{} is not connected", host.label),
            ));
        };
        let fidelity = self
            .hosts
            .facts(saved_id)
            .filter(|facts| facts.reduced_fidelity)
            .map(|facts| bootstrap::FidelityOffer {
                session_id: facts.session_id.clone(),
                skew: facts.skew.clone(),
            });
        self.start_action_probe(
            saved_id,
            &host.label,
            ActionKind::Install,
            session_id,
            fidelity,
            ticketed,
        )
    }

    /// Update roost-session… on a refused ssh session: today's combined
    /// install-and-restart, through its consent card.
    pub(crate) fn host_update_requested(&mut self, saved_id: &str) -> Result<(), HostOpFailure> {
        let host = self
            .saved_host(saved_id)
            .map_err(|error| HostOpFailure::new(codes::NOT_FOUND, error.to_string()))?;
        let transport = servicing::transport_kind(&host.target);
        self.refuse_claimed(saved_id, &host.label)?;
        let offered = self
            .host_update_facts(saved_id, &host.target)
            .is_some_and(|facts| session_update::update_offered(&facts, transport));
        if !offered {
            return Err(HostOpFailure::new(
                codes::INVALID_PARAM,
                format!("{} does not need Update roost-session", host.label),
            ));
        }
        let bound = self
            .acting_session(saved_id)
            .map(|(session_id, ..)| session_id);
        if let Some(session_id) = &bound {
            self.refuse_busy(saved_id, session_id, &host.label)?;
        }
        self.refuse_dialog_open()?;
        self.start_bootstrap_probe(
            saved_id,
            OfferContext {
                session: SessionState::Running,
                intent: ProbeIntent::Bootstrap,
                bound_session: bound,
                failure: None,
                fidelity: None,
            },
        );
        Ok(())
    }

    /// Restart Session…, from any surface; `ticketed` as for Install.
    pub(crate) fn host_session_restart_requested(
        &mut self,
        saved_id: &str,
        ticketed: bool,
    ) -> Result<(), HostOpFailure> {
        let host = self
            .saved_host(saved_id)
            .map_err(|error| HostOpFailure::new(codes::NOT_FOUND, error.to_string()))?;
        let transport = servicing::transport_kind(&host.target);
        if transport == HostTransportKind::Socket {
            return Err(HostOpFailure::new(
                codes::NOT_SUPPORTED,
                format!(
                    "{} is reached through a socket; its session is not this Roost's to restart",
                    host.label
                ),
            ));
        }
        self.refuse_claimed(saved_id, &host.label)?;
        let facts = self
            .host_update_facts(saved_id, &host.target)
            .ok_or_else(|| {
                HostOpFailure::new(
                    codes::INVALID_PARAM,
                    format!("{} is not connected", host.label),
                )
            })?;
        if let Some(why) = session_update::restart_refusal(&facts, transport) {
            return Err(HostOpFailure::new(
                codes::INVALID_PARAM,
                format!("Restart Session is not offered on {}: {why}", host.label),
            ));
        }
        let Some((session_id, running, exe_path)) = self.acting_session(saved_id) else {
            return Err(HostOpFailure::new(
                codes::INVALID_PARAM,
                format!("{} is not connected", host.label),
            ));
        };
        if transport == HostTransportKind::Ssh {
            return self.start_action_probe(
                saved_id,
                &host.label,
                ActionKind::Restart,
                session_id,
                None,
                ticketed,
            );
        }
        let ticket = self.reserve(
            saved_id,
            ActionKind::Restart,
            &session_id,
            &host.label,
            ticketed,
        )?;
        self.resolve_local_target(saved_id.to_string(), session_id, running, exe_path, ticket);
        Ok(())
    }

    /// The localhost compatibility gate's own restart prompt, confirmed:
    /// the same restart, resolved fresh and run without a second card.
    pub(super) fn host_mismatch_restart_confirmed(
        &mut self,
        saved_id: &str,
        label: &str,
        bound: &str,
    ) {
        let Some((session_id, running, exe_path)) = self
            .acting_session(saved_id)
            .filter(|(serving, ..)| serving == bound)
        else {
            self.set_status(format!("{label}: {CHANGED} — nothing was stopped"));
            return;
        };
        match self.claim_action(
            saved_id,
            ActionKind::Restart,
            &session_id,
            label,
            RequestOrigin::User,
        ) {
            Ok(ticket) => {
                self.set_status(format!("restarting the session on {label}…"));
                self.resolve_local_target(
                    saved_id.to_string(),
                    session_id,
                    running,
                    exe_path,
                    Some(ticket),
                );
            }
            Err(failure) => self.set_status(failure.message),
        }
    }

    fn start_action_probe(
        &mut self,
        saved_id: &str,
        label: &str,
        kind: ActionKind,
        session_id: String,
        fidelity: Option<bootstrap::FidelityOffer>,
        ticketed: bool,
    ) -> Result<(), HostOpFailure> {
        let ticket = self.reserve(saved_id, kind, &session_id, label, ticketed)?;
        let intent = match kind {
            ActionKind::Restart => ProbeIntent::Restart { session_id, ticket },
            ActionKind::Install | ActionKind::Update => ProbeIntent::Install { session_id, ticket },
        };
        let started = self.start_bootstrap_probe(
            saved_id,
            OfferContext {
                session: SessionState::Running,
                intent,
                bound_session: None,
                failure: None,
                fidelity,
            },
        );
        if !started {
            if let Some(ticket) = ticket {
                self.finish_action(
                    saved_id,
                    ticket,
                    Err(format!("{label} could not be checked")),
                );
            }
            return Err(HostOpFailure::new(
                codes::BUSY,
                format!("{label} is already being checked"),
            ));
        }
        Ok(())
    }

    fn resolve_local_target(
        &mut self,
        saved_id: String,
        session_id: String,
        running: BuildId,
        exe_path: Option<String>,
        ticket: Option<u64>,
    ) {
        let generation = self.take_engine_op_id();
        let launch = super::update_knowledge::LaunchFacts::from_env();
        let client = bootstrap::client_build_id().clone();
        let feed = self.feed_tx.clone();
        self.runtime_handle.spawn(async move {
            let restart = super::update_knowledge::identify_localhost(
                launch,
                exe_path,
                running,
                client,
                session_id.clone(),
                generation,
            )
            .await;
            feed.send(crate::engine_feed::EngineFeed::HostAction(Box::new(
                ActionEvent::Resolved {
                    saved_id,
                    session_id,
                    ticket,
                    restart,
                },
            )));
        });
    }

    pub(crate) fn host_action_event(&mut self, event: ActionEvent) {
        match event {
            ActionEvent::Resolved {
                saved_id,
                session_id,
                ticket,
                restart,
            } => self.local_target_resolved(&saved_id, session_id, ticket, restart),
            ActionEvent::Stopped {
                saved_id,
                generation,
                result,
            } => self.local_restart_stopped(&saved_id, generation, result),
            ActionEvent::ReleaseStream {
                saved_id,
                generation,
                ack,
            } => self.stream_release_requested(&saved_id, generation, ack),
            ActionEvent::LaunchSettled {
                saved_id,
                generation,
            } => {
                self.settle_cancelled_action(&saved_id, generation);
            }
            ActionEvent::RelaunchExpired {
                saved_id,
                generation,
            } => self.relaunch_expired(&saved_id, generation),
        }
    }

    fn local_target_resolved(
        &mut self,
        saved_id: &str,
        session_id: String,
        ticket: Option<u64>,
        restart: TargetKnowledge,
    ) {
        let Some(label) = self.host_label(saved_id) else {
            if let Some(ticket) = ticket {
                self.finish_action(
                    saved_id,
                    ticket,
                    Err(format!("{saved_id} is no longer saved")),
                );
            }
            return;
        };
        if ticket.is_some_and(|ticket| !self.host_actions.holds(saved_id, ticket)) {
            return;
        }
        let still = self
            .acting_session(saved_id)
            .is_some_and(|(serving, ..)| serving == session_id);
        let outcome = match (still, restart) {
            (false, _) => Err(format!("{label}: {CHANGED}")),
            (true, TargetKnowledge::Found(target)) => Ok(target),
            (true, TargetKnowledge::NoneUsable(why)) => {
                Err(format!("{label} can't be restarted: {}", why.reason()))
            }
            (true, TargetKnowledge::NotChecked | TargetKnowledge::Checking) => Err(format!(
                "{label} can't be restarted: its roost-session was not identified"
            )),
        };
        match (ticket, outcome) {
            (Some(ticket), Ok(target)) => {
                self.run_local_restart(saved_id, ticket, session_id, target)
            }
            (Some(ticket), Err(message)) => self.finish_action(saved_id, ticket, Err(message)),
            (None, Ok(target)) => {
                if let Err(failure) = self.refuse_dialog_open() {
                    self.set_status(failure.message);
                    return;
                }
                self.open_restart_card(
                    saved_id,
                    &label,
                    RestartPlan::Local { session_id, target },
                    None,
                );
            }
            (None, Err(message)) => self.set_status(message),
        }
    }

    fn open_restart_card(
        &mut self,
        saved_id: &str,
        label: &str,
        plan: RestartPlan,
        lead: Option<String>,
    ) {
        let target = match &plan {
            RestartPlan::Local { target, .. } | RestartPlan::Ssh { target, .. } => target,
        };
        let reduced = self
            .hosts
            .facts(saved_id)
            .filter(|facts| facts.reduced_fidelity)
            .map(|facts| host_notice::reduced_fidelity_reason(&facts.skew));
        let lead = match (reduced, lead) {
            (Some(reduced), Some(lead)) => Some(format!("{reduced} {lead}")),
            (reduced, lead) => reduced.or(lead),
        };
        let ends = self
            .hosts
            .incarnation(saved_id)
            .and_then(|incarnation| self.hosts.mirror(incarnation))
            .map(|mirror| {
                let mirror = mirror.read();
                running_programs(
                    mirror
                        .tabs()
                        .map(|tab| (tab.title.as_str(), tab.shell_state, tab.agent_lifecycle)),
                )
            })
            .unwrap_or_default();
        let prompt = host_notice::restart_card(label, lead.as_deref(), target, &ends);
        self.open_host_dialog(host_dialog::HostDialog::ConfirmRestart {
            saved_id: saved_id.to_string(),
            prompt,
            expected_session: None,
            session_id: Some(plan.session_id().to_string()),
            plan: Some(plan),
        });
        self.reconcile();
    }

    /// The Restart card's button.
    pub(super) fn session_restart_confirmed(&mut self) {
        let Some(host_dialog::HostDialog::ConfirmRestart {
            saved_id,
            plan: Some(plan),
            ..
        }) = self.host_dialog.take()
        else {
            return;
        };
        let label = self.label_or_id(&saved_id);
        let still = self
            .acting_session(&saved_id)
            .is_some_and(|(serving, ..)| serving == plan.session_id());
        if !still {
            self.set_status(format!("{label}: {CHANGED} — nothing was stopped"));
            self.reconcile();
            return;
        }
        let generation = match self.claim_action(
            &saved_id,
            ActionKind::Restart,
            plan.session_id(),
            &label,
            RequestOrigin::User,
        ) {
            Ok(generation) => generation,
            Err(failure) => {
                self.set_status(failure.message);
                return;
            }
        };
        match plan {
            RestartPlan::Local { session_id, target } => {
                self.run_local_restart(&saved_id, generation, session_id, target);
            }
            RestartPlan::Ssh {
                session_id,
                token,
                arch,
                outcome,
                target,
            } => {
                let plan = bootstrap::restart_only_plan(
                    &outcome,
                    &target.path,
                    &target.identity,
                    &session_id,
                );
                self.run_ssh_action(
                    &saved_id,
                    generation,
                    &token,
                    arch,
                    plan,
                    ActionKind::Restart,
                    target.identity,
                    session_id,
                );
            }
        }
    }

    /// Hand an action's job to the bootstrap runner. A plan that stops
    /// the session asks for the live stream to be let go once it has
    /// re-checked what it stops ([`StreamRelease`]).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_ssh_action(
        &mut self,
        saved_id: &str,
        generation: u64,
        token: &str,
        arch: RemoteArch,
        plan: bootstrap::BootstrapPlan,
        kind: ActionKind,
        build: BuildId,
        session_id: String,
    ) {
        let Some((host, live)) = self.same_ssh_host(saved_id, token) else {
            self.finish_action(
                saved_id,
                generation,
                Err(format!(
                    "{saved_id} no longer names that host — nothing was changed"
                )),
            );
            return;
        };
        let note = match kind {
            ActionKind::Install => "installing roost-session…",
            ActionKind::Restart => "restarting…",
            ActionKind::Update => "setting up roost-session…",
        };
        let ticket = ActionTicket {
            kind,
            generation,
            build,
            session_id,
        };
        let release = plan.stop.then(|| self.stream_release(saved_id, generation));
        if !self.launch_bootstrap_job(&host, live, plan, arch, note, Some(ticket), release) {
            self.finish_action(
                saved_id,
                generation,
                Err(format!("{} is already being set up", host.label)),
            );
        }
    }

    fn run_local_restart(
        &mut self,
        saved_id: &str,
        generation: u64,
        session_id: String,
        target: RestartTarget,
    ) {
        let socket =
            self.saved_host(saved_id).ok().and_then(|host| {
                match roost_ipc::ssh::classify(&host.target) {
                    Ok(roost_ipc::ssh::ResolvedTransport::LocalSession(socket)) => Some(socket),
                    _ => None,
                }
            });
        let Some(socket) = socket else {
            self.finish_action(
                saved_id,
                generation,
                Err(format!("{saved_id} is no longer this machine's session")),
            );
            return;
        };
        if let Some(claim) = self.host_actions.claim_mut(saved_id, generation) {
            claim.target = Some(target.clone());
        }
        let release = self.stream_release(saved_id, generation);
        self.action_working(saved_id, generation);
        self.hosts
            .set_bootstrap_note(saved_id, Some("restarting…".to_string()));
        let label = self.label_or_id(saved_id);
        tracing::info!(host = %saved_id, target = %target.path, "restarting a host session");
        self.set_status(format!("restarting the session on {label}…"));
        self.reconcile();
        let feed = self.feed_tx.clone();
        let saved_id = saved_id.to_string();
        self.runtime_handle.spawn(async move {
            let result = stop_for_restart(socket, session_id, target, release).await;
            feed.send(crate::engine_feed::EngineFeed::HostAction(Box::new(
                ActionEvent::Stopped {
                    saved_id,
                    generation,
                    result,
                },
            )));
        });
    }

    fn local_restart_stopped(
        &mut self,
        saved_id: &str,
        generation: u64,
        result: Result<(), LocalFailure>,
    ) {
        if self.settle_cancelled_action(saved_id, generation) {
            return;
        }
        let label = self.label_or_id(saved_id);
        let Some((origin, released)) = self.host_actions.held(saved_id, generation) else {
            tracing::debug!(host = %saved_id, generation, "dropped a superseded restart step");
            return;
        };
        self.host_actions.set_working(saved_id, generation, false);
        match result {
            Ok(()) => {
                let Some(claim) = self.host_actions.claim_mut(saved_id, generation) else {
                    return;
                };
                let Some(target) = claim.target.clone() else {
                    return;
                };
                claim.relaunching = true;
                // Held until the action ends, so every connect started
                // for this host meanwhile — this one, an auto-retry, a
                // person's Connect — starts the target and nothing else.
                self.hosts.set_bootstrap_note(saved_id, None);
                self.hosts.pin_spawn(
                    saved_id,
                    crate::host_conn::task::SpawnPin {
                        path: PathBuf::from(&target.path),
                        identity: target.identity,
                        launches: crate::host_conn::task::LaunchGate::new(
                            saved_id,
                            generation,
                            self.feed_tx.clone(),
                        ),
                    },
                );
                self.arm_relaunch_deadline(saved_id, generation);
                self.host_reconnect_requested(
                    saved_id,
                    origin,
                    crate::host_conn::AttemptCause::Explicit,
                );
            }
            Err(failure) => {
                let message = match failure {
                    LocalFailure::Changed => format!("{label}: {CHANGED}"),
                    LocalFailure::Stop(message) => format!("{label}: {message}"),
                };
                self.finish_action(saved_id, generation, Err(message));
                // Take back what the restart let go of, without a spawn:
                // whatever is serving there now is what there is.
                if released {
                    self.host_redial(saved_id, origin);
                }
            }
        }
    }

    pub(super) fn host_actions_held(
        &self,
        saved_id: &str,
        generation: u64,
    ) -> Option<(RequestOrigin, bool)> {
        self.host_actions.held(saved_id, generation)
    }

    pub(super) fn stream_release(&self, saved_id: &str, generation: u64) -> StreamRelease {
        StreamRelease {
            feed: self.feed_tx.clone(),
            saved_id: saved_id.to_string(),
            generation,
        }
    }

    fn stream_release_requested(
        &mut self,
        saved_id: &str,
        generation: u64,
        ack: tokio::sync::oneshot::Sender<()>,
    ) {
        let connected = matches!(self.hosts.state(saved_id), Some(HostConnState::Connected));
        let Some(claim) = self.host_actions.claim_mut(saved_id, generation) else {
            return;
        };
        claim.released_stream = true;
        if connected {
            self.drop_host_stream(saved_id);
        }
        let _ = ack.send(());
    }

    fn arm_relaunch_deadline(&self, saved_id: &str, generation: u64) {
        let deadline = RELAUNCH_DEADLINE.mul_f64(roost_ipc::session_launch::timeout_scale());
        self.runtime_handle.spawn(relaunch_deadline(
            self.feed_tx.clone(),
            saved_id.to_string(),
            generation,
            deadline,
        ));
    }

    /// A relaunch that has not landed a verified session by its deadline
    /// fails the restart, the way a Disconnect would: failed on record at
    /// once, its claim held while a launch is still out. Ending the
    /// action lifts the pin, so whatever the connection still tries is an
    /// ordinary dial.
    fn relaunch_expired(&mut self, saved_id: &str, generation: u64) {
        let label = self.label_or_id(saved_id);
        let reason = self
            .hosts
            .section_reason(saved_id)
            .map(str::to_string)
            .unwrap_or_else(|| "it did not come back".to_string());
        let message = format!(
            "{label} did not restart within {}s: {reason}",
            RELAUNCH_DEADLINE.as_secs()
        );
        let hosts = &self.hosts;
        if self
            .host_actions
            .give_up_relaunch(saved_id, generation, &message, || {
                hosts.revoke_spawn(saved_id)
            })
            .is_some()
        {
            self.action_ended(saved_id, Err(message));
        }
    }

    /// A connection's identify facts landed. Settles a localhost restart
    /// waiting on its relaunch (D8's verification).
    pub(super) fn host_action_facts_landed(&mut self, saved_id: &str) {
        self.refresh_action_facts(saved_id);
        let Some((generation, before, target)) = self.host_actions.relaunching(saved_id) else {
            return;
        };
        let Some(facts) = self.hosts.facts(saved_id) else {
            return;
        };
        let label = self.label_or_id(saved_id);
        let outcome = session_update::verify_restart(
            &before,
            &target.identity,
            &facts.session_id,
            &facts.running,
        )
        .map(|()| {
            format!(
                "{label} restarted on roost-session {}",
                describe(&target.identity)
            )
        })
        .map_err(|serving| format!("{label} restarted, but the session is still {serving}"));
        self.finish_action(saved_id, generation, outcome);
    }

    /// A connection settled somewhere other than connected. Fails a
    /// localhost restart whose relaunch will not land, and any other
    /// action whose live stream dropped without the action letting go
    /// of it.
    pub(super) fn host_action_connect_settled(&mut self, saved_id: &str) {
        self.refresh_action_facts(saved_id);
        let Some(generation) = self.host_actions.generation(saved_id) else {
            return;
        };
        let state = self.hosts.state(saved_id);
        if self.host_actions.relaunching(saved_id).is_none() {
            let released = self
                .host_actions
                .held(saved_id, generation)
                .is_some_and(|(_, released)| released);
            if !released && !matches!(state, Some(HostConnState::NeedsRestart(_))) {
                self.cancel_host_action(saved_id, "the connection dropped while the action ran");
            }
            return;
        }
        let label = self.label_or_id(saved_id);
        let reason = self
            .hosts
            .section_reason(saved_id)
            .map(str::to_string)
            .unwrap_or_else(|| "it did not come back".to_string());
        let message = format!("{label} did not restart: {reason}");
        let hosts = &self.hosts;
        if self
            .host_actions
            .relaunch_settled(saved_id, state, &message, || hosts.revoke_spawn(saved_id))
            .is_some()
        {
            self.action_ended(saved_id, Err(message));
        }
    }

    /// A plan 076 probe answered: raise the card, or — for an op — act.
    pub(super) fn host_action_probed(
        &mut self,
        saved_id: &str,
        intent: ProbeIntent,
        fidelity: Option<bootstrap::FidelityOffer>,
        result: Result<bootstrap::Probed, roost_ipc::bootstrap::BootstrapError>,
        live: (String, roost_ipc::ssh::SshTarget),
        target_spelling: &str,
    ) {
        let (label, ssh_target) = live;
        let (kind, session_id, ticket) = match intent {
            ProbeIntent::Install { session_id, ticket } => {
                (ActionKind::Install, session_id, ticket)
            }
            ProbeIntent::Restart { session_id, ticket } => {
                (ActionKind::Restart, session_id, ticket)
            }
            ProbeIntent::Bootstrap => return,
        };
        if ticket.is_some_and(|ticket| !self.host_actions.holds(saved_id, ticket)) {
            return;
        }
        let refuse = |app: &mut Self, message: String| match ticket {
            Some(ticket) => app.finish_action(saved_id, ticket, Err(message)),
            None => {
                app.set_status(message);
                app.reconcile();
            }
        };
        let probed = match result {
            Ok(probed) => probed,
            Err(error) => return refuse(self, error.message(target_spelling)),
        };
        let Some((serving, running, _)) = self
            .acting_session(saved_id)
            .filter(|(serving, ..)| *serving == session_id)
        else {
            return refuse(self, format!("{label}: {CHANGED}"));
        };
        let client = bootstrap::client_build_id().clone();
        let outcome = probed.probe.outcome.clone();
        let generation = self.take_engine_op_id();
        let (restart, _) =
            session_update::ssh_knowledge(&outcome, &running, &client, &serving, generation);
        let restart_plan = |target: RestartTarget| RestartPlan::Ssh {
            session_id: serving.clone(),
            token: ssh_target.token.clone(),
            arch: probed.probe.arch,
            outcome: outcome.clone(),
            target,
        };
        if kind == ActionKind::Install {
            match session_update::install_refusal(&running, &client, rung_build(&outcome).as_ref())
            {
                Ok(()) => {}
                // D5: the build is already there — the card says so and
                // offers the restart that puts it to use.
                Err(session_update::InstallRefusal::AlreadyInstalled) if ticket.is_none() => {
                    let TargetKnowledge::Found(target) = restart else {
                        return refuse(
                            self,
                            format!(
                                "{label}: {}",
                                session_update::InstallRefusal::AlreadyInstalled.reason()
                            ),
                        );
                    };
                    let lead = format!(
                        "roost-session {} is already installed at {} on {label} — restart to \
                         use it.",
                        describe(&target.identity),
                        target.path
                    );
                    return self.open_restart_card(
                        saved_id,
                        &label,
                        restart_plan(target),
                        Some(lead),
                    );
                }
                Err(refusal) => {
                    return refuse(self, format!("{label}: {}", refusal.reason()));
                }
            }
            let source = match probed.source {
                Ok(source) => source,
                Err(error) => return refuse(self, error.message(target_spelling)),
            };
            let plan = bootstrap::install_only_plan(&outcome, &serving);
            let identity = bootstrap::client_identity();
            if let Some(ticket) = ticket {
                return self.run_ssh_action(
                    saved_id,
                    ticket,
                    &ssh_target.token,
                    probed.probe.arch,
                    plan,
                    ActionKind::Install,
                    BuildId::from(&identity),
                    serving,
                );
            }
            let copy = bootstrap::bootstrap_copy(bootstrap::CopyInputs {
                label: &label,
                identity: &identity,
                dest: &bootstrap::card_dest(&plan),
                dest_on_disk: &bootstrap::dest_on_disk(&plan, &probed.probe.home),
                source: &source,
                plan: &plan,
                fidelity: fidelity.as_ref().map(|opened| &opened.skew),
            });
            self.open_host_dialog(host_dialog::HostDialog::Bootstrap(
                bootstrap::BootstrapDraft {
                    saved_id: saved_id.to_string(),
                    label,
                    token: ssh_target.token.clone(),
                    claim: ssh_target.claim_key.clone(),
                    arch: probed.probe.arch,
                    plan,
                    copy,
                    offer: OfferContext {
                        session: SessionState::Running,
                        intent: ProbeIntent::Install {
                            session_id: serving,
                            ticket: None,
                        },
                        bound_session: None,
                        failure: None,
                        fidelity,
                    },
                },
            ));
            self.reconcile();
            return;
        }
        let target = match restart {
            TargetKnowledge::Found(target) => target,
            TargetKnowledge::NoneUsable(why) => {
                return refuse(
                    self,
                    format!("{label} can't be restarted: {}", why.reason()),
                );
            }
            TargetKnowledge::NotChecked | TargetKnowledge::Checking => {
                return refuse(
                    self,
                    format!("{label} can't be restarted: nothing answered"),
                );
            }
        };
        match ticket {
            Some(ticket) => {
                let plan = bootstrap::restart_only_plan(
                    &outcome,
                    &target.path,
                    &target.identity,
                    &serving,
                );
                self.run_ssh_action(
                    saved_id,
                    ticket,
                    &ssh_target.token,
                    probed.probe.arch,
                    plan,
                    ActionKind::Restart,
                    target.identity,
                    serving,
                );
            }
            None => self.open_restart_card(saved_id, &label, restart_plan(target), None),
        }
    }

    /// What a press on `reduced fidelity` does under Option 2 (D6):
    /// Install when the ssh rung is stale, the Restart card when a build
    /// is staged or the host is this machine, nothing otherwise.
    pub(super) fn fidelity_route_requested(&mut self, saved_id: &str, label: &str, target: &str) {
        let transport = servicing::transport_kind(target);
        let facts = self.host_update_facts(saved_id, target);
        let route = session_update::fidelity_route(
            host_sidebar::fidelity_action(true, transport, SectionState::Connected),
            facts.as_ref(),
            transport,
        );
        let result = match route {
            Some(FidelityAction::Update) => self.host_install_requested(saved_id, false),
            Some(FidelityAction::Restart) => self.host_session_restart_requested(saved_id, false),
            Some(FidelityAction::Manual) | None => {
                self.set_status(format!(
                    "{label} has nothing to install or restart onto from here"
                ));
                return;
            }
        };
        if let Err(failure) = result {
            self.set_status(failure.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ui_model::session_update::TargetSource;

    #[test]
    fn one_action_per_host_and_per_session() {
        let mut actions = HostActions::default();
        assert!(actions.begin("local", 1, ActionKind::Restart, "s1", RequestOrigin::Ipc));
        assert!(actions.busy("local", "other"), "the host is claimed");
        assert!(
            actions.busy("ssh-to-self", "s1"),
            "another row naming the same session is busy too"
        );
        assert!(!actions.begin(
            "ssh-to-self",
            2,
            ActionKind::Install,
            "s1",
            RequestOrigin::Ipc
        ));
        assert!(!actions.busy("elsewhere", "s2"));
        assert!(actions.begin(
            "elsewhere",
            3,
            ActionKind::Install,
            "s2",
            RequestOrigin::Ipc
        ));
    }

    #[test]
    fn a_superseded_completion_is_ignored() {
        let mut actions = HostActions::default();
        assert!(actions.begin("h", 1, ActionKind::Install, "s1", RequestOrigin::Ipc));
        assert!(actions.finish("h", 1, &Err("failed".into())));
        assert!(actions.begin("h", 2, ActionKind::Restart, "s1", RequestOrigin::Ipc));
        assert!(
            !actions.finish("h", 1, &Ok("late".into())),
            "the first attempt's late answer must not end the second"
        );
        assert!(actions.holds("h", 2));
        assert_eq!(actions.status("h").unwrap().phase, "running");
        assert!(actions.finish("h", 2, &Ok("h restarted".into())));
        assert!(!actions.busy("h", "s1"));
    }

    #[test]
    fn the_latest_action_is_reported_and_survives_its_claim() {
        let mut actions = HostActions::default();
        assert_eq!(actions.status("h"), None);
        actions.begin("h", 1, ActionKind::Install, "s1", RequestOrigin::Ipc);
        assert_eq!(
            actions.status("h"),
            Some(HostActionStatus {
                kind: "install".into(),
                phase: "running".into(),
                message: None,
            })
        );
        actions.finish("h", 1, &Ok("Installed roost-session 0.0.22 on h".into()));
        assert_eq!(
            actions.status("h"),
            Some(HostActionStatus {
                kind: "install".into(),
                phase: "done".into(),
                message: Some("Installed roost-session 0.0.22 on h".into()),
            })
        );
        actions.forget("h");
        assert_eq!(actions.status("h"), None);
    }

    #[test]
    fn a_relaunch_is_held_until_it_is_settled() {
        let mut actions = HostActions::default();
        actions.begin("h", 4, ActionKind::Restart, "s1", RequestOrigin::Ipc);
        assert_eq!(actions.relaunching("h"), None);
        let target = RestartTarget {
            path: "/bin/roost-session".into(),
            identity: BuildId::default(),
            source: TargetSource::Bundled,
            session_id: "s1".into(),
            generation: 9,
        };
        let claim = actions.claim_mut("h", 4).unwrap();
        claim.target = Some(target.clone());
        claim.relaunching = true;
        assert!(
            actions.busy("h", "s2"),
            "the claim is held through the relaunch"
        );
        assert_eq!(
            actions.relaunching("h"),
            Some((4, "s1".to_string(), target))
        );
        assert!(actions.claim_mut("h", 3).is_none());
    }

    /// D7's `action` stays visible while the host has no facts of its
    /// own — the stretch of a restart where the outcome matters most.
    #[test]
    fn an_action_is_reported_with_the_last_facts_learned() {
        let mut actions = HostActions::default();
        let facts = session_update::UpdateFacts {
            state: session_update::SessionUpdate::Staged,
            running: BuildId {
                version: "0.0.21".into(),
                ..BuildId::default()
            },
            client: BuildId::default(),
            restart: session_update::RestartOffer {
                offered: true,
                why: None,
                target: None,
            },
            staged: None,
        };
        assert_eq!(actions.update_status("h", None), None);
        actions.remember("h", facts.clone());
        assert_eq!(
            actions.update_status("h", None),
            None,
            "remembered facts alone are no reason to report anything"
        );
        actions.begin("h", 7, ActionKind::Restart, "s1", RequestOrigin::User);
        let status = actions
            .update_status("h", None)
            .expect("an action on record");
        assert_eq!(status.session.version, "0.0.21");
        assert_eq!(status.action.unwrap().phase, "running");
        assert_eq!(actions.held("h", 7), Some((RequestOrigin::User, false)));
        assert_eq!(actions.generation("h"), Some(7));
        actions.finish("h", 7, &Err("gone".into()));
        assert_eq!(
            actions
                .update_status("h", None)
                .unwrap()
                .action
                .unwrap()
                .phase,
            "failed"
        );
        assert!(!actions.claimed("h"));
        actions.forget("h");
        assert_eq!(actions.update_status("h", None), None);
    }

    /// A cancel with a worker out keeps the claim — failed on record,
    /// still busy — until that worker reports; with none out it goes now.
    #[test]
    fn a_cancelled_claim_is_held_until_its_worker_reports() {
        let mut actions = HostActions::default();
        actions.begin("h", 5, ActionKind::Restart, "s1", RequestOrigin::Ipc);
        actions.set_working("h", 5, true);
        assert!(actions.cancel("h", 5, "h: disconnected"));
        assert_eq!(actions.status("h").unwrap().phase, "failed");
        assert!(actions.claimed("h") && actions.cancelling("h"));
        assert!(actions.busy("other-row", "s1"), "the session stays claimed");
        assert_eq!(actions.generation("h"), None, "nothing cancels it twice");
        assert!(!actions.holds("h", 5) && actions.held("h", 5).is_none());
        assert!(!actions.settle_cancelled("h", 4));
        assert!(actions.settle_cancelled("h", 5));
        assert!(!actions.claimed("h"));
        assert_eq!(
            actions.status("h").unwrap().message.as_deref(),
            Some("h: disconnected")
        );

        actions.begin("h", 6, ActionKind::Install, "s1", RequestOrigin::Ipc);
        assert!(actions.cancel("h", 6, "h: disconnected"));
        assert!(!actions.claimed("h"), "nothing out, nothing to wait for");
    }

    fn relaunching(actions: &mut HostActions, generation: u64) {
        actions.begin(
            "h",
            generation,
            ActionKind::Restart,
            "s1",
            RequestOrigin::Ipc,
        );
        let claim = actions.claim_mut("h", generation).unwrap();
        claim.target = Some(RestartTarget {
            path: "/bin/roost-session".into(),
            identity: BuildId::default(),
            source: TargetSource::Bundled,
            session_id: "s1".into(),
            generation: 1,
        });
        claim.relaunching = true;
    }

    /// The deadline, armed, fires its own restart's expiry. Expiring is
    /// a Disconnect's cancel: failed at once, and — with a launch out
    /// from the pin — the claim held, busy, until that launch settles.
    /// A stale deadline, or one for a relaunch that has landed, does
    /// nothing.
    #[tokio::test(start_paused = true)]
    async fn an_expired_relaunch_fails_at_once_and_holds_its_launch_until_it_settles() {
        let (feed, mut rx) = crate::engine_feed::channel();
        let deadline = tokio::spawn(relaunch_deadline(
            feed.clone(),
            "h".into(),
            8,
            RELAUNCH_DEADLINE,
        ));
        tokio::time::sleep(RELAUNCH_DEADLINE).await;
        deadline.await.expect("the deadline fired");
        let mut batch = crate::engine_feed::EngineBatch::default();
        let Some(crate::engine_feed::EngineFeed::HostAction(event)) = rx.try_next(&mut batch)
        else {
            panic!("the armed deadline reports its expiry");
        };
        let ActionEvent::RelaunchExpired {
            saved_id,
            generation,
        } = *event
        else {
            panic!("not an expiry");
        };
        assert_eq!((saved_id.as_str(), generation), ("h", 8));

        let mut actions = HostActions::default();
        relaunching(&mut actions, 8);
        let gate = crate::host_conn::task::LaunchGate::new("h", 8, feed);
        let launch = gate.begin_for_test();
        assert_eq!(
            actions.give_up_relaunch("h", 7, "stale", || gate.revoke()),
            None
        );
        assert_eq!(
            actions.give_up_relaunch("h", 8, "h did not restart within 70s", || gate.revoke()),
            Some(false),
            "a launch is out, so the claim is held"
        );
        assert_eq!(actions.status("h").unwrap().phase, "failed");
        assert!(actions.claimed("h") && actions.cancelling("h"));
        assert!(actions.busy("other-row", "s1"));
        assert_eq!(
            actions.give_up_relaunch("h", 8, "again", || gate.revoke()),
            None
        );

        drop(launch);
        let Some(crate::engine_feed::EngineFeed::HostAction(event)) = rx.try_next(&mut batch)
        else {
            panic!("the launch reports itself settled");
        };
        let ActionEvent::LaunchSettled { generation, .. } = *event else {
            panic!("not a settle");
        };
        assert!(actions.settle_cancelled("h", generation));
        assert!(!actions.claimed("h"));

        relaunching(&mut actions, 9);
        assert_eq!(
            actions.give_up_relaunch("h", 9, "h did not restart within 70s", || false),
            Some(true),
            "nothing out, nothing to wait for"
        );
        assert!(!actions.claimed("h"));
        assert!(
            RELAUNCH_DEADLINE
                > roost_ipc::session_launch::DEFAULT_VERDICT_BUDGET
                    + roost_ipc::session_launch::DEFAULT_CONFIRM_BUDGET,
            "a launch still inside its own budgets is never failed early"
        );
    }

    /// A relaunch whose connection settles short of the session — the
    /// launcher said it failed — fails at once, but holds its claim
    /// while that launch is still out, exactly as a Disconnect would.
    #[test]
    fn a_terminal_relaunch_failure_holds_its_launch_until_it_settles() {
        use crate::host_conn::state::Disconnected;
        let (feed, mut rx) = crate::engine_feed::channel();
        let mut actions = HostActions::default();
        relaunching(&mut actions, 8);
        let gate = crate::host_conn::task::LaunchGate::new("h", 8, feed);
        let launch = gate.begin_for_test();
        let retrying = HostConnState::Disconnected(Disconnected {
            reason: "connection refused".into(),
            detail: None,
            retry_in: Some(std::time::Duration::from_secs(1)),
        });
        let terminal = HostConnState::Disconnected(Disconnected {
            reason: "roost-session failed to start".into(),
            detail: None,
            retry_in: None,
        });
        assert_eq!(
            actions.relaunch_settled("h", Some(&retrying), "x", || gate.revoke()),
            None,
            "still trying"
        );
        assert_eq!(
            actions.relaunch_settled(
                "h",
                Some(&HostConnState::Connecting { previous: None }),
                "x",
                || gate.revoke()
            ),
            None
        );
        assert_eq!(
            actions.relaunch_settled("h", Some(&terminal), "h did not restart", || gate.revoke()),
            Some(false),
            "a launch is out, so the claim is held"
        );
        assert_eq!(actions.status("h").unwrap().phase, "failed");
        assert!(actions.claimed("h") && actions.cancelling("h"));
        assert_eq!(
            actions.relaunch_settled("h", Some(&terminal), "again", || gate.revoke()),
            None
        );

        drop(launch);
        let mut batch = crate::engine_feed::EngineBatch::default();
        let Some(crate::engine_feed::EngineFeed::HostAction(event)) = rx.try_next(&mut batch)
        else {
            panic!("the launch reports itself settled");
        };
        let ActionEvent::LaunchSettled { generation, .. } = *event else {
            panic!("not a settle");
        };
        assert!(actions.settle_cancelled("h", generation));
        assert!(!actions.claimed("h"));
    }

    #[test]
    fn the_tabs_a_restart_ends_are_the_busy_ones() {
        let tabs = [
            ("idle", ShellState::AtPrompt, AgentLifecycle::Inactive),
            (
                "vim",
                ShellState::ForegroundProcess,
                AgentLifecycle::Inactive,
            ),
            ("claude", ShellState::AtPrompt, AgentLifecycle::Working),
            ("done", ShellState::AtPrompt, AgentLifecycle::Finished),
        ];
        assert_eq!(running_programs(tabs), ["vim", "claude"]);
        assert!(running_programs([]).is_empty());
    }
}
