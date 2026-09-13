//! Where this UI's own tabs run, what it publishes about that, and how
//! a launch decides between the two (plan 063 §D1/§D5/§D7).
//!
//! The [`LocalRoute`] snapshot is the only thing the engine's IPC
//! handler knows about the backend — it answers `identify` from it, and
//! nothing else in the UI may write it. [`App::publish_local_route`]
//! is the single writer; this module owns what it writes.
//!
//! [`App::publish_local_route`]: super::App::publish_local_route

use roost_ipc::messages::Project;
use roost_ipc::{LocalBackendMode, LocalRoute};
use roost_ui_model::config::RoostConfig;
use roost_ui_model::keys::HostId;

/// What the effective-mode ladder concluded (plan 063 §D5 steps 2–4).
///
/// Three outcomes rather than a bare mode, because the fresh-install one
/// owes a config write and the other two do not — and because a
/// `Configured(InProcess)` and an `Existing` are the same backend for
/// very different reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeLadder {
    /// The key was there. An unparseable value counts: it is
    /// `in-process` *configured badly*, which is still configured, and
    /// so must never trigger the fresh-install write (`config.rs`'s
    /// `local_backend_key_present`).
    Configured(LocalBackendMode),
    /// Nothing of this app is on disk: no layout, no config. A first run
    /// starts on a session, and the key is written so the next launch is
    /// `Configured` rather than deciding again.
    FreshInstall,
    /// A setup that predates the key. It keeps the backend it has always
    /// had; flipping it is a post-release follow-up, not a launch's
    /// decision to make.
    Existing,
}

impl ModeLadder {
    /// The backend this conclusion asks for, before any write has been
    /// attempted — see [`settled_mode`].
    pub(crate) fn mode(self) -> LocalBackendMode {
        match self {
            Self::Configured(mode) => mode,
            Self::FreshInstall => LocalBackendMode::Session,
            Self::Existing => LocalBackendMode::InProcess,
        }
    }
}

/// The ladder's first input: the configured backend, or `None` when the
/// key was never written.
///
/// **Presence, not value.** `local-backend = wat` parses to
/// `in-process` *and* records the key as present, and the difference
/// matters exactly once: a fresh-install write would overwrite the line
/// the user wrote, which is why AC1 pins "malformed → in-process, no
/// fresh write" as its own clause.
pub(crate) fn configured_key(config: &RoostConfig) -> Option<LocalBackendMode> {
    config
        .local_backend_key_present
        .then(|| LocalBackendMode::from(config.local_backend))
}

/// Plan 063 §D5's ladder, steps 2–4 (step 1, the switch journal, is a
/// later commit's).
///
/// `config_exists` is a separate question from `state_json_exists` and
/// closes the shared-config hazard: `config::default_path()` has **no
/// profile component** while `BundleProfile::state_json_path()` does, so
/// a first run under the dev profile finds an empty state dir beside the
/// developer's real `config.conf`. That is not a fresh install, and
/// treating it as one would write `local-backend = session` into the
/// config the *release* profile reads.
pub(crate) fn ladder(
    key: Option<LocalBackendMode>,
    state_json_exists: bool,
    config_exists: bool,
) -> ModeLadder {
    match key {
        Some(mode) => ModeLadder::Configured(mode),
        None if !state_json_exists && !config_exists => ModeLadder::FreshInstall,
        None => ModeLadder::Existing,
    }
}

/// The mode the launch actually runs on, once the fresh-install write
/// has been attempted.
///
/// A write that failed degrades to `in-process` **for this launch
/// only**: coming up on a session whose mode nothing recorded would mean
/// the next launch finds a populated `state.json`, concludes `Existing`,
/// and leaves that session's projects invisible. Next launch reads
/// whatever is genuinely on disk and decides again.
///
/// "Failed" includes *a config now exists* — the write is create-only
/// (`config::create_with_key`), so someone else winning the race is an
/// `AlreadyExists` and this launch defers to the file they wrote rather
/// than to a decision it can no longer record.
pub(crate) fn settled_mode(ladder: ModeLadder, fresh_write_ok: bool) -> LocalBackendMode {
    match ladder {
        ModeLadder::FreshInstall if !fresh_write_ok => LocalBackendMode::InProcess,
        ladder => ladder.mode(),
    }
}

/// The label the implicit slot host is saved under (plan 063 §D7).
///
/// `localhost`, else `localhost (2)`, `localhost (3)`… — an SSH host may
/// already hold the plain name, and `Workspace::add_host` enforces
/// unique labels. `accepts` is the registry's own check rather than a
/// comparison here: that rule is Unicode case folding in both
/// directions plus the reserved word `local`, and a second spelling of
/// it would drift. The counter starts at 2, so no candidate is ever the
/// reserved word.
///
/// `None` when the registry refuses every candidate, which means
/// something other than a name collision is wrong; the caller logs and
/// comes up with no slot rather than looping.
pub(crate) fn slot_label(accepts: impl Fn(&str) -> bool) -> Option<String> {
    unique_label(roost_ui_model::host_verbs::SEED_LABEL, accepts)
}

/// The same rule for any name a saved host is about to be added under:
/// `base`, else `base (2)`, `base (3)`…
///
/// Two callers need it — the implicit slot and a *recent* being saved
/// again (plan 063 §D7), whose old label may have been taken by
/// something else since it was forgotten — and a second spelling of the
/// suffix rule would drift from the first.
pub(crate) fn unique_label(base: &str, accepts: impl Fn(&str) -> bool) -> Option<String> {
    const SUFFIX_LIMIT: u32 = 64;
    (1..=SUFFIX_LIMIT).find_map(|n| {
        let candidate = match n {
            1 => base.to_string(),
            n => format!("{base} ({n})"),
        };
        accepts(&candidate).then_some(candidate)
    })
}

/// Why a connect attempt was started, and therefore what it owes the
/// host when it lands (plan 063 §D12).
///
/// The one thing this decides is [`Self::seeds`]. Before it existed, a
/// connect that was going to be followed by a creation could not say so,
/// and "connect, then create" raced "connect, and seed because it looks
/// empty" into two projects where the user asked for one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ConnectPurpose {
    /// Nobody asked for anything beyond the connection: a launch probe,
    /// the sidebar's ↻, `roostctl host connect`, a retry rung.
    #[default]
    OrdinaryConnect,
    /// Connect *and make sure there is something there* — the launch's
    /// own dial of the slot, which has to come up on a project.
    EnsureNonempty,
    /// The caller creates as soon as this lands, so seeding would make
    /// two projects out of one gesture.
    CreateAfterConnect,
    /// A local-backend switch owns everything that happens on the
    /// destination (plan 063 §D8's replay), so this connect adds
    /// nothing of its own.
    // The switch itself is §D8's, which C6 ships; the purpose is here
    // because the seeding rule it turns off is C5's, and a destination
    // connect that seeded would be a duplicate of the first replayed
    // project.
    #[allow(dead_code)]
    SwitchDestination,
}

impl ConnectPurpose {
    /// Whether a host found empty when this connect lands gets one
    /// seeded project (plan 063 §D6/§D12).
    ///
    /// Only the two purposes that own nothing else about the landing: a
    /// caller that is about to create, or a switch that is about to
    /// replay, would each turn a seed into a duplicate.
    pub(crate) fn seeds(self) -> bool {
        match self {
            Self::OrdinaryConnect | Self::EnsureNonempty => true,
            Self::CreateAfterConnect | Self::SwitchDestination => false,
        }
    }
}

/// What a tracked host op is, for the two questions asked of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostOpKind {
    /// A project creation — a seed, a picker row, ⌘N. Named apart
    /// because "is a creation owed?" is its own clause in both §D6 and
    /// §D9: everything is empty *because the thing that fills it has not
    /// run yet* is not the user closing the last project.
    Create,
    /// Any other workspace mutation this client sent that host.
    Other,
}

/// The locally-initiated host workspace mutations still awaiting a
/// reply, keyed by the engine op id the dispatch minted.
///
/// Only the ops that change what a host *holds* — create, open, close,
/// delete, rename, reorder. Those are exactly the ops that can cause a
/// host to go empty, so they are exactly the ops whose reply the
/// auto-remove they trigger must not swallow ([`removal_step`]).
#[derive(Debug, Default)]
pub(crate) struct HostOpsInFlight {
    by_op: std::collections::HashMap<u64, (String, HostOpKind)>,
}

impl HostOpsInFlight {
    /// A dispatch went out. `saved_id` is the host it is addressed to;
    /// an op on a host this client no longer holds is not tracked,
    /// because there is nothing left for it to gate.
    pub(crate) fn begin(&mut self, op: u64, saved_id: Option<String>, kind: HostOpKind) {
        if let Some(saved_id) = saved_id {
            self.by_op.insert(op, (saved_id, kind));
        }
    }

    /// Its completion reached the main thread — which is where the
    /// reply to whoever asked is written.
    pub(crate) fn finish(&mut self, op: u64) {
        self.by_op.remove(&op);
    }

    /// Nothing this client sent that host is still outstanding.
    pub(crate) fn settled(&self, saved_id: &str) -> bool {
        !self.by_op.values().any(|(host, _)| host == saved_id)
    }

    pub(crate) fn creating(&self, saved_id: &str) -> bool {
        self.by_op
            .values()
            .any(|(host, kind)| host == saved_id && *kind == HostOpKind::Create)
    }

    pub(crate) fn any_create(&self) -> bool {
        self.by_op
            .values()
            .any(|(_, kind)| *kind == HostOpKind::Create)
    }
}

/// One host band, as the exit rule reads it (plan 063 §D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostExitRow {
    /// Whether the band still lists rows the user can see — a live
    /// mirror's or a disconnected section's retained ones. Both are
    /// shells that are still running somewhere, so both keep the window.
    pub(crate) visible_rows: bool,
    /// An attempt is in flight. It may be about to publish rows, so
    /// the window waits for it to say.
    pub(crate) connecting: bool,
}

/// Everything plan 063 §D9's predicate is a function of.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExitInput<'a> {
    pub(crate) mode: LocalBackendMode,
    /// The in-process workspace holds no projects. Always true under
    /// `session`, where nothing is ever created there.
    pub(crate) local_projects_empty: bool,
    pub(crate) hosts: &'a [HostExitRow],
    /// A seed or a create-after-connect is owed to some host (§D6/§D12).
    /// The window it covers is the one where everything is legitimately
    /// empty because the thing that will fill it has not run yet.
    pub(crate) creation_pending: bool,
    /// A local-backend switch is mid-flight (§D8a). Its own phases
    /// empty and refill both backends, and the exit latch is
    /// irreversible, so nothing may read that as "the user closed
    /// everything".
    pub(crate) switch_in_flight: bool,
    /// Whether *the slot* — the saved host holding the local band — is
    /// in the saved-host registry **right now**. See [`exit_on_empty`].
    pub(crate) slot_registered: bool,
    /// Whether one ever was, this run — [`SlotEverRegistered`]. The two
    /// ways the slot can be absent mean opposite things, and this is the
    /// only input that tells them apart.
    pub(crate) slot_ever_registered: bool,
}

/// Has a slot ever been in the saved-host registry during this run?
///
/// A one-way latch, and the one-wayness is the whole point: it is what
/// separates "the auto-remove just took the slot out" (plan 063 §D6,
/// AC7 — exit) from "bootstrap never managed to put one in" (§D5's
/// failed `ensure_local_slot` — stay open). Both are registry-absent,
/// and a predicate reading only the present tense answers the same for
/// both. A latch that could re-clear would collapse them again the
/// moment the slot left.
///
/// Same shape as [`ExitState`](super::ExitState) for the same reason:
/// an irreversible fact about the run belongs in a type that cannot
/// express going back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotEverRegistered(bool);

impl SlotEverRegistered {
    /// Fold in what the registry says now. Arms on the first `true` —
    /// at bootstrap when `ensure_local_slot` saved one or found one
    /// already there, and equally when a user adds one by hand later.
    pub(crate) fn observe(&mut self, registered: bool) {
        self.0 |= registered;
    }

    pub(crate) fn ever(self) -> bool {
        self.0
    }
}

/// Plan 063 §D9's exit rule: has the user closed the last thing this
/// window was showing?
///
/// Before R9 this was "the in-process workspace is empty", which exits
/// with a host's projects on screen (§2, reproduced live) and would exit
/// on its own first reconcile under `session`, where the in-process
/// workspace is empty by design.
///
/// **The slot carve-out** (the last clause) is what resolves the
/// contradiction between §D6 and AC7. Under `session` the local band is
/// a saved host, so "the user closed the last project" means the *slot*
/// went empty — and the app must still exit, exactly as closing the last
/// in-process project does under `in-process`. What makes that safe is
/// that the slot's own auto-remove (§D6) takes it out of the registry in
/// the same reconcile: a slot that is merely down, spawning or needing a
/// restart is **still registered** and blocks the exit, which is the
/// spawn-failure window §D5 keeps the window open for. So the clause
/// reads registry membership rather than connection state.
///
/// It reads membership in **two tenses**, because registry-absent has
/// two causes that want opposite answers. The slot having *left* is the
/// case above. The slot having *never arrived* is §D5's other failure:
/// `ensure_local_slot` warns and returns when the registry accepts no
/// label or the add fails, and coming up with no local band is
/// something the window must survive — §D5 says so for the failed spawn
/// ("the window stays open (D9 guard) with no local band"), and a save
/// that never happened is the stricter version of it. A spawn failure
/// already behaves correctly because the slot stays registered; this
/// clause is what makes the save failure behave the same.
pub(crate) fn exit_on_empty(input: ExitInput<'_>) -> bool {
    let slot_gone = input.mode != LocalBackendMode::Session
        || (input.slot_ever_registered && !input.slot_registered);
    input.local_projects_empty
        && input
            .hosts
            .iter()
            .all(|host| !host.visible_rows && !host.connecting)
        && !input.creation_pending
        && !input.switch_in_flight
        && slot_gone
}

/// A host that went empty and is waiting to be forgotten (plan 063
/// §D6).
///
/// Scheduled where the causative batch lands, settled by
/// [`sweep_removals`] and [`confirmed_step`] over the reconciles that
/// follow — the gap is the point, and `revision` names the commit the
/// claim was made at so the log line can say which one it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingAutoRemove {
    /// The connection whose mirror made the claim. A removal is
    /// discarded rather than deferred once this is no longer the live
    /// incarnation: a reconnect rebuilds the mirror from a fresh
    /// `tab.list`, and whatever that says is a new question.
    pub(crate) incarnation: HostId,
    pub(crate) revision: u64,
    /// Whether the confirming `tab.list` is already out
    /// (`HostConnSet::confirm_empty`). One per claim: every reconcile
    /// passes the gate while it is in flight, and a fresh question per
    /// reconcile would be a round trip per frame.
    pub(crate) confirming: bool,
}

/// What one reconcile does with every scheduled claim.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Sweep {
    /// Ask these hosts whether they really hold nothing.
    pub(crate) confirm: Vec<(String, HostId)>,
    /// Carry these into the next reconcile — including the ones just
    /// added to `confirm`, which are waiting on an answer.
    pub(crate) keep: std::collections::HashMap<String, PendingAutoRemove>,
    /// These claims stopped being true; forget the claim, not the host.
    pub(crate) dropped: Vec<String>,
}

/// Carry every scheduled claim forward one reconcile (plan 063 §D6).
///
/// **Every** one. A single-slot field would lose a claim whenever a
/// second host emptied while the first was still settling — and with
/// the confirming round trip that window is a whole request, not an
/// instant. The host that lost its claim is then never forgotten, and
/// if it is the slot under `session` its lingering registry row blocks
/// the exit for the life of the process.
pub(crate) fn sweep_removals(
    claims: std::collections::HashMap<String, PendingAutoRemove>,
    step: impl Fn(&str, &PendingAutoRemove) -> RemovalStep,
) -> Sweep {
    let mut sweep = Sweep {
        keep: std::collections::HashMap::with_capacity(claims.len()),
        ..Sweep::default()
    };
    for (saved_id, mut pending) in claims {
        match step(&saved_id, &pending) {
            RemovalStep::Wait => {
                sweep.keep.insert(saved_id, pending);
            }
            RemovalStep::Discard => sweep.dropped.push(saved_id),
            RemovalStep::Execute => {
                if !pending.confirming {
                    pending.confirming = true;
                    sweep.confirm.push((saved_id.clone(), pending.incarnation));
                }
                sweep.keep.insert(saved_id, pending);
            }
        }
    }
    // Deterministic, because a `HashMap` walk is not: two hosts
    // confirming in one reconcile must ask in the same order every run,
    // or a test of them is a test of the allocator.
    sweep.confirm.sort();
    sweep.dropped.sort();
    sweep
}

/// What a host's answer to the confirming `tab.list` means for the
/// claim that asked (plan 063 §D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmedStep {
    /// Forget the host.
    Forget,
    /// Keep the claim and ask again — the connection that answered is
    /// not the one that asked.
    Requeue,
    /// The claim is over. The host keeps its row.
    Drop,
}

/// Decide it.
///
/// **`Ok(false)` outranks the gate, and that is the whole point.** The
/// gate's `mirror_empty` is a projection that lags its session by
/// however long a broadcast takes, and two ordinary sequences make it
/// read empty while the session holds projects: a creation whose
/// control reply beat its `project.created` event, and a resumed
/// connection replaying a delete before the create that followed it.
/// In both the gate says "empty, go ahead" and the session says "I have
/// projects" — and the session is the party doing the committing, so it
/// wins. Forgetting there loses a host that has work on it, and on the
/// slot under `session` it ends the process over that work.
///
/// The gate is still re-read for the `Some(true)` case: the answer came
/// back from a round trip, and anything may have moved while it was
/// out.
pub(crate) fn confirmed_step(
    empty: Option<bool>,
    same_incarnation: bool,
    gate_now: RemovalStep,
) -> ConfirmedStep {
    if !same_incarnation {
        return ConfirmedStep::Requeue;
    }
    match empty {
        Some(true) if gate_now == RemovalStep::Execute => ConfirmedStep::Forget,
        // `None` is a host that could not answer, and that is not a
        // host that answered "empty": the claim goes, the row stays.
        Some(_) | None => ConfirmedStep::Drop,
    }
}

/// What a scheduled auto-remove should do on this reconcile (plan 063
/// §D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemovalStep {
    /// Forget the host now.
    Execute,
    /// Not yet — keep it scheduled and ask again next reconcile.
    Wait,
    /// The claim it was scheduled on is no longer true. Drop it.
    Discard,
}

/// The gate on that decision.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RemovalGate {
    /// The incarnation whose batch claimed the emptiness is still the
    /// live connection for that host.
    pub(crate) owned: bool,
    /// That incarnation's mirror is *still* empty.
    pub(crate) mirror_empty: bool,
    /// No locally-initiated workspace mutation on this host is still
    /// waiting for its reply.
    pub(crate) ops_settled: bool,
    /// No seed or create-after-connect is owed to this host.
    pub(crate) creation_pending: bool,
    pub(crate) switch_in_flight: bool,
}

/// Decide it.
///
/// The ordering clause is `ops_settled`, and it is the whole reason the
/// removal is *scheduled* rather than done where the batch arrives. A
/// host mutation's control reply and its broadcast are separate
/// messages, and the broadcast can win (§2): so the `project.deleted`
/// that empties a host routinely reaches this client while the
/// `project.delete` that caused it is still awaiting its answer. Removal
/// disconnects first, and a disconnect flushes the op queue with
/// `Disconnected` — which would turn the user's successful deletion into
/// an error, and under §D9 do it on the way out of the process, so
/// nobody ever hears otherwise (AC7 pins the reply landing first).
///
/// `Discard` rather than `Wait` for the two claims that cannot come
/// back: a replaced incarnation never publishes again under that id, and
/// a mirror that has rows in it is not a host anybody emptied.
pub(crate) fn removal_step(gate: RemovalGate) -> RemovalStep {
    if !gate.owned || !gate.mirror_empty {
        return RemovalStep::Discard;
    }
    if !gate.ops_settled || gate.creation_pending || gate.switch_in_flight {
        return RemovalStep::Wait;
    }
    RemovalStep::Execute
}

/// What a launch under `session` owes the slot's first mirror (plan 063
/// §D5's "initial selection").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialSelection {
    /// Nothing left to do: the mode is not `session`, or a selection is
    /// already held and an unrelated one is preserved.
    Settled,
    /// The slot has published no rows to select yet. A fresh session
    /// seeds one; a session started empty gets one when it is seeded.
    Wait,
    /// Select this project/tab pair and attach to the tab.
    Select { project: i64, tab: i64 },
}

/// Decide it (plan 063 §D5).
///
/// `slot` is the slot's live rows and its own active tab id, and is
/// `None` until the slot is connected and interactive. The acceptance
/// criterion is a *visible, attached* tab, not "≥1 project", which is
/// why this resolves to a tab rather than to a project.
pub(crate) fn initial_selection(
    mode: LocalBackendMode,
    selection_held: bool,
    slot: Option<(&[Project], i64)>,
) -> InitialSelection {
    if mode != LocalBackendMode::Session || selection_held {
        return InitialSelection::Settled;
    }
    let Some((projects, active_tab_id)) = slot else {
        return InitialSelection::Wait;
    };
    // The session's own active tab wins, so a relaunch lands where the
    // last client left it; its project is found by holding that tab
    // rather than by position, because the session's project order is
    // not this client's.
    let project = projects
        .iter()
        .find(|project| project.tabs.iter().any(|tab| tab.id == active_tab_id))
        .or_else(|| projects.iter().find(|project| !project.tabs.is_empty()));
    let Some(project) = project else {
        return InitialSelection::Wait;
    };
    let tab = project
        .tabs
        .iter()
        .find(|tab| tab.id == active_tab_id)
        .or_else(|| project.tabs.first());
    match tab {
        Some(tab) => InitialSelection::Select {
            project: project.id,
            tab: tab.id,
        },
        None => InitialSelection::Wait,
    }
}

/// The snapshot for `mode`.
///
/// Both derived fields hang off the mode: in-process has no slot, so it
/// has neither a slot socket nor a slot selection. The socket comes
/// from the session bundle profile because that is what determines it —
/// the daemon binds that path whether or not anyone is connected to it.
pub(crate) fn route_snapshot(
    mode: LocalBackendMode,
    slot_active: Option<(i64, i64)>,
) -> LocalRoute {
    match mode {
        LocalBackendMode::InProcess => LocalRoute {
            mode,
            slot_socket: None,
            slot_active: None,
        },
        LocalBackendMode::Session => LocalRoute {
            mode,
            slot_socket: roost_ipc::session_socket_path(),
            slot_active,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::paths::BundleProfile;
    use std::collections::HashMap;

    #[test]
    fn the_session_snapshot_carries_the_profile_socket_and_the_slot_selection() {
        let route = route_snapshot(LocalBackendMode::Session, Some((4, 9)));
        assert_eq!(route.mode, LocalBackendMode::Session);
        assert_eq!(route.slot_active, Some((4, 9)));
        assert_eq!(
            route.slot_socket,
            Some(
                BundleProfile::session()
                    .unwrap()
                    .socket_path
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    /// In-process has no slot at all, so a selection handed in from a
    /// previous session mode cannot survive the switch back.
    #[test]
    fn the_in_process_snapshot_has_no_slot() {
        let route = route_snapshot(LocalBackendMode::InProcess, Some((4, 9)));
        assert_eq!(route.mode, LocalBackendMode::InProcess);
        assert_eq!(route.slot_socket, None);
        assert_eq!(route.slot_active, None);
    }

    /// The ladder, branch by branch (plan 063 §D5 / AC1).
    ///
    /// The `Configured` cases are asserted on the *variant*, not on the
    /// mode: `Configured(InProcess)` and `Existing` name the same
    /// backend, and only the first of them means "the user said so".
    #[test]
    fn the_ladder_prefers_the_key_then_a_fresh_install_then_today() {
        use LocalBackendMode::{InProcess, Session};

        // A key wins whatever is on disk, in both directions.
        for state_json in [false, true] {
            for config in [false, true] {
                assert_eq!(
                    ladder(Some(Session), state_json, config),
                    ModeLadder::Configured(Session),
                    "state_json={state_json} config={config}"
                );
                assert_eq!(
                    ladder(Some(InProcess), state_json, config),
                    ModeLadder::Configured(InProcess),
                    "state_json={state_json} config={config}"
                );
            }
        }

        // No key, but something of this app is already on disk. Either
        // half is enough to say "not a first run".
        assert_eq!(ladder(None, true, false), ModeLadder::Existing);
        assert_eq!(ladder(None, false, true), ModeLadder::Existing);
        assert_eq!(ladder(None, true, true), ModeLadder::Existing);

        // Nothing at all.
        assert_eq!(ladder(None, false, false), ModeLadder::FreshInstall);

        assert_eq!(ModeLadder::FreshInstall.mode(), Session);
        assert_eq!(ModeLadder::Existing.mode(), InProcess);
    }

    /// An unparseable value is "configured", so it takes the existing
    /// backend and — the part that matters — never triggers the
    /// fresh-install write that would overwrite the line the user
    /// wrote (AC1's last clause).
    ///
    /// Driven from real config text through the same two fields the
    /// launch reads, because the trap is in the *wiring*: `wat` resolves
    /// to the default `in-process`, so anything deciding "configured?"
    /// from the value rather than from presence gets this backwards on
    /// an otherwise-empty disk.
    #[test]
    fn a_malformed_value_is_configured_rather_than_fresh() {
        let bad = RoostConfig::parse("local-backend = wat\n");
        assert_eq!(configured_key(&bad), Some(LocalBackendMode::InProcess));
        assert_eq!(
            ladder(configured_key(&bad), false, false),
            ModeLadder::Configured(LocalBackendMode::InProcess),
            "an empty disk plus a bad value is still not a fresh install"
        );

        // The same empty disk with no key at all is the case the write
        // belongs to, which is what makes the row above load-bearing.
        let absent = RoostConfig::parse("");
        assert_eq!(configured_key(&absent), None);
        assert_eq!(
            ladder(configured_key(&absent), false, false),
            ModeLadder::FreshInstall
        );

        // And a good value round-trips, both ways.
        assert_eq!(
            configured_key(&RoostConfig::parse("local-backend = session\n")),
            Some(LocalBackendMode::Session)
        );
        assert_eq!(
            configured_key(&RoostConfig::parse("local-backend = in-process\n")),
            Some(LocalBackendMode::InProcess)
        );
    }

    /// A fresh install that cannot record its choice runs the launch
    /// on the old backend rather than on an unrecorded new one.
    #[test]
    fn a_failed_fresh_install_write_degrades_to_in_process_for_this_launch() {
        assert_eq!(
            settled_mode(ModeLadder::FreshInstall, true),
            LocalBackendMode::Session
        );
        assert_eq!(
            settled_mode(ModeLadder::FreshInstall, false),
            LocalBackendMode::InProcess
        );
        // Every other conclusion writes nothing, so the write's outcome
        // cannot move it.
        for ladder in [
            ModeLadder::Configured(LocalBackendMode::Session),
            ModeLadder::Configured(LocalBackendMode::InProcess),
            ModeLadder::Existing,
        ] {
            assert_eq!(settled_mode(ladder, false), ladder.mode(), "{ladder:?}");
            assert_eq!(settled_mode(ladder, true), ladder.mode(), "{ladder:?}");
        }
    }

    /// Plan 063 §D9's exit rule, one column at a time.
    ///
    /// Every row below is the SAME baseline with exactly one field
    /// moved, which is the point: a table built by varying two things
    /// at once passes for a predicate that reads neither.
    ///
    /// The baseline is the interesting one — `session`, everything
    /// empty, nothing in flight, and the slot already gone from the
    /// registry, which is the state plan 063 §D6's auto-remove leaves
    /// behind when the user closes the last project on the slot.
    #[test]
    fn every_clause_of_the_exit_rule_can_hold_the_window_open() {
        let quiet_host = HostExitRow {
            visible_rows: false,
            connecting: false,
        };
        let hosts = [quiet_host];
        let base = ExitInput {
            mode: LocalBackendMode::Session,
            local_projects_empty: true,
            hosts: &hosts,
            creation_pending: false,
            switch_in_flight: false,
            // The state plan 063 §D6's auto-remove leaves behind: a
            // slot that was in the registry and has just left it.
            slot_registered: false,
            slot_ever_registered: true,
        };
        assert!(
            exit_on_empty(base),
            "the baseline must exit, or every row below proves nothing"
        );

        let mut local = base;
        local.local_projects_empty = false;
        assert!(!exit_on_empty(local), "an in-process project keeps it open");

        let with_rows = [HostExitRow {
            visible_rows: true,
            ..quiet_host
        }];
        let mut rows = base;
        rows.hosts = &with_rows;
        assert!(
            !exit_on_empty(rows),
            "a host band still listing rows keeps it open — those shells are running"
        );

        let dialing = [HostExitRow {
            connecting: true,
            ..quiet_host
        }];
        let mut connecting = base;
        connecting.hosts = &dialing;
        assert!(
            !exit_on_empty(connecting),
            "an attempt in flight may be about to publish rows"
        );

        let mut creating = base;
        creating.creation_pending = true;
        assert!(
            !exit_on_empty(creating),
            "empty because the seed has not landed yet is not empty because the user emptied it"
        );

        let mut switching = base;
        switching.switch_in_flight = true;
        assert!(
            !exit_on_empty(switching),
            "a switch empties both backends on its way through"
        );

        let mut slot = base;
        slot.slot_registered = true;
        assert!(
            !exit_on_empty(slot),
            "a slot that is still saved — down, spawning, needing a restart — keeps the window"
        );

        // The quantifier: one quiet host is not "every host".
        let mixed = [quiet_host, with_rows[0]];
        let mut two = base;
        two.hosts = &mixed;
        assert!(!exit_on_empty(two));
    }

    /// The slot carve-out is `session`-only, and it is the one clause
    /// that reads two inputs — so it gets the pair the single-column
    /// table above cannot express.
    ///
    /// Under `in-process` a saved localhost host is an ordinary
    /// `LOCALHOST` band beside a local workspace that does not depend on
    /// it, so its presence says nothing about whether the window has
    /// anything left to show.
    #[test]
    fn a_registered_slot_blocks_the_exit_only_under_session() {
        let hosts = [HostExitRow {
            visible_rows: false,
            connecting: false,
        }];
        let input = |mode, slot_registered| ExitInput {
            mode,
            local_projects_empty: true,
            hosts: &hosts,
            creation_pending: false,
            switch_in_flight: false,
            slot_registered,
            // Held constant: which slot *absence* this is, is the next
            // test's subject.
            slot_ever_registered: true,
        };
        assert!(!exit_on_empty(input(LocalBackendMode::Session, true)));
        assert!(exit_on_empty(input(LocalBackendMode::Session, false)));
        assert!(exit_on_empty(input(LocalBackendMode::InProcess, true)));
        assert!(exit_on_empty(input(LocalBackendMode::InProcess, false)));
    }

    /// The two ways the slot can be absent from the registry, which
    /// want opposite answers (plan 063 §D9 / §D5).
    ///
    /// Four rows, and only the `ever` column moves between the two that
    /// matter: row 2 is AC7 (the auto-remove took the slot out and the
    /// window should close), row 3 is a bootstrap whose
    /// `ensure_local_slot` saved nothing (no local band, and the window
    /// must survive it — §D5 promises exactly that for the failed
    /// spawn, which is the milder version of the same failure). A
    /// predicate reading only the present tense answers row 2 for row 3.
    #[test]
    fn the_slot_clause_tells_one_that_left_the_registry_from_one_that_never_arrived() {
        let hosts = [HostExitRow {
            visible_rows: false,
            connecting: false,
        }];
        let input = |mode, ever, registered| ExitInput {
            mode,
            local_projects_empty: true,
            hosts: &hosts,
            creation_pending: false,
            switch_in_flight: false,
            slot_registered: registered,
            slot_ever_registered: ever,
        };
        use LocalBackendMode::{InProcess, Session};

        assert!(
            !exit_on_empty(input(Session, true, true)),
            "an ordinary run: the slot is saved, so the window stays"
        );
        assert!(
            exit_on_empty(input(Session, true, false)),
            "AC7: the slot was here and the auto-remove took it out"
        );
        assert!(
            !exit_on_empty(input(Session, false, false)),
            "§D5: bootstrap could not save a slot at all — no local band, \
             and the window has to survive that rather than vanish"
        );
        // In-process never consults either tense: the same registry is
        // an ordinary LOCALHOST band beside a workspace of its own.
        for ever in [false, true] {
            for registered in [false, true] {
                assert!(
                    exit_on_empty(input(InProcess, ever, registered)),
                    "in-process ever={ever} registered={registered}"
                );
            }
        }
    }

    /// The latch is one-way, and row 2 above depends on it: a slot that
    /// leaves the registry must not take the *fact that it was here*
    /// with it, or the auto-remove case would read as the
    /// never-arrived one and the window would refuse to close.
    #[test]
    fn a_slot_that_leaves_the_registry_does_not_un_arm_the_latch() {
        let mut seen = SlotEverRegistered::default();
        assert!(!seen.ever(), "nothing has been saved yet");

        // A launch that never managed to save one keeps answering no,
        // however many times it looks.
        for _ in 0..3 {
            seen.observe(false);
        }
        assert!(!seen.ever());

        // `ensure_local_slot` saved one — or a user added one by hand.
        seen.observe(true);
        assert!(seen.ever());

        // …and the auto-remove took it out again. THIS is the row that
        // a re-clearing latch would break.
        seen.observe(false);
        assert!(seen.ever(), "the latch is one-way");

        let hosts = [HostExitRow {
            visible_rows: false,
            connecting: false,
        }];
        assert!(
            exit_on_empty(ExitInput {
                mode: LocalBackendMode::Session,
                local_projects_empty: true,
                hosts: &hosts,
                creation_pending: false,
                switch_in_flight: false,
                slot_registered: false,
                slot_ever_registered: seen.ever(),
            }),
            "and the exit AC7 asks for is still permitted through it"
        );
    }

    /// Today's behaviour, unchanged: no saved hosts, `in-process`, and
    /// the rule is exactly "the workspace is empty" (`test_exit_on_empty`
    /// is this lane).
    #[test]
    fn the_zero_host_in_process_rule_is_still_the_workspace_being_empty() {
        let input = |local_projects_empty| ExitInput {
            mode: LocalBackendMode::InProcess,
            local_projects_empty,
            hosts: &[],
            creation_pending: false,
            switch_in_flight: false,
            slot_registered: false,
            slot_ever_registered: false,
        };
        assert!(exit_on_empty(input(true)));
        assert!(!exit_on_empty(input(false)));
    }

    /// Plan 063 §D6's gate, one column at a time from a baseline that
    /// removes.
    ///
    /// The two `Discard`s are the claims that cannot come back; the
    /// three `Wait`s are the ones that can.
    #[test]
    fn the_auto_remove_gate_waits_for_what_can_change_and_drops_what_cannot() {
        let base = RemovalGate {
            owned: true,
            mirror_empty: true,
            ops_settled: true,
            creation_pending: false,
            switch_in_flight: false,
        };
        assert_eq!(removal_step(base), RemovalStep::Execute);

        let mut replaced = base;
        replaced.owned = false;
        assert_eq!(
            removal_step(replaced),
            RemovalStep::Discard,
            "a reconnect rebuilt the mirror; the claim was about a connection that is gone"
        );

        let mut refilled = base;
        refilled.mirror_empty = false;
        assert_eq!(
            removal_step(refilled),
            RemovalStep::Discard,
            "somebody created over there; this is not a host anyone emptied"
        );

        let mut unsettled = base;
        unsettled.ops_settled = false;
        assert_eq!(
            removal_step(unsettled),
            RemovalStep::Wait,
            "the delete that caused this is still waiting for its reply"
        );

        let mut creating = base;
        creating.creation_pending = true;
        assert_eq!(removal_step(creating), RemovalStep::Wait);

        let mut switching = base;
        switching.switch_in_flight = true;
        assert_eq!(removal_step(switching), RemovalStep::Wait);
    }

    fn claim(incarnation: u32, confirming: bool) -> PendingAutoRemove {
        PendingAutoRemove {
            incarnation: HostId::new(incarnation),
            revision: 7,
            confirming,
        }
    }

    /// Every scheduled claim survives one reconcile (plan 063 §D6).
    ///
    /// The case that a single slot could not hold: three hosts emptied,
    /// each at a different point in the settling, and one reconcile has
    /// to carry all three — ask the one that is ready, keep the one
    /// that is waiting, drop the one whose claim stopped being true.
    /// With one slot the two it did not hold are never forgotten, and
    /// an emptied slot that is never forgotten blocks the exit under
    /// `session` for the life of the process.
    #[test]
    fn one_reconcile_carries_every_scheduled_claim() {
        let claims = HashMap::from([
            ("ready".to_string(), claim(1, false)),
            ("waiting".to_string(), claim(2, false)),
            ("stale".to_string(), claim(3, false)),
        ]);
        let sweep = sweep_removals(claims, |saved_id, _| match saved_id {
            "ready" => RemovalStep::Execute,
            "waiting" => RemovalStep::Wait,
            _ => RemovalStep::Discard,
        });

        assert_eq!(
            sweep.confirm,
            vec![("ready".to_string(), HostId::new(1))],
            "the one that is ready is the one asked"
        );
        assert_eq!(sweep.dropped, vec!["stale".to_string()]);
        let mut kept: Vec<&str> = sweep.keep.keys().map(String::as_str).collect();
        kept.sort();
        assert_eq!(
            kept,
            vec!["ready", "waiting"],
            "the asked one is kept too — it is waiting on the answer"
        );
        assert!(
            sweep.keep["ready"].confirming,
            "and it is marked, so the next reconcile does not ask again"
        );
        assert!(!sweep.keep["waiting"].confirming);
    }

    /// A claim with its question already out passes the gate every
    /// reconcile, and must not turn that into a round trip per frame.
    #[test]
    fn a_claim_already_confirming_is_not_asked_twice() {
        let claims = HashMap::from([("h1".to_string(), claim(1, true))]);
        let sweep = sweep_removals(claims, |_, _| RemovalStep::Execute);
        assert!(sweep.confirm.is_empty(), "{:?}", sweep.confirm);
        assert!(sweep.keep["h1"].confirming);
    }

    /// Plan 063 §D6's verdict: **the host's answer outranks the
    /// mirror**.
    ///
    /// The `Some(false)` row is the whole finding. Everything the client
    /// can see says forget it — the gate is `Execute`, which means the
    /// mirror is empty, no op is outstanding and nothing is owed — and
    /// the session says it is holding projects. That is the shape of a
    /// creation whose reply beat its event, and of a resume replaying a
    /// delete before the create that followed it; forgetting there
    /// loses a host that has work on it, and on the slot it ends the
    /// process over that work.
    #[test]
    fn the_hosts_own_answer_outranks_a_mirror_that_says_empty() {
        assert_eq!(
            confirmed_step(Some(false), true, RemovalStep::Execute),
            ConfirmedStep::Drop,
            "the mirror had not caught up; the session is the authority"
        );
        assert_eq!(
            confirmed_step(Some(true), true, RemovalStep::Execute),
            ConfirmedStep::Forget
        );
        assert_eq!(
            confirmed_step(None, true, RemovalStep::Execute),
            ConfirmedStep::Drop,
            "a host that could not answer did not answer \"empty\""
        );
        // The round trip is long enough for the world to move, so the
        // gate is re-read rather than trusted from before it.
        for step in [RemovalStep::Wait, RemovalStep::Discard] {
            assert_eq!(
                confirmed_step(Some(true), true, step),
                ConfirmedStep::Drop,
                "{step:?}"
            );
        }
        // A reply from a connection that has since been replaced
        // describes a world that is gone; the live claim asks again.
        for empty in [Some(true), Some(false), None] {
            assert_eq!(
                confirmed_step(empty, false, RemovalStep::Execute),
                ConfirmedStep::Requeue,
                "{empty:?}"
            );
        }
    }

    /// The ordering property itself, over the real registry: the removal
    /// is refused while the deletion is outstanding and allowed the
    /// moment its completion retires it.
    ///
    /// What this pins is the *pair* — `HostOpsInFlight` and
    /// `removal_step` composed — not the app wiring around them, which
    /// needs an `App` (`engine_op_completed` retires the op before the
    /// reconcile that runs the removal). Without the retirement the
    /// second assertion still reads `Wait`, which is what makes the
    /// sequence load-bearing rather than decorative.
    #[test]
    fn a_removal_is_refused_until_the_delete_that_caused_it_is_answered() {
        let mut ops = HostOpsInFlight::default();
        // The user's `project.delete`, dispatched and not yet answered.
        ops.begin(7, Some("h1".to_string()), HostOpKind::Other);
        // A second host's op must not hold h1's removal.
        ops.begin(8, Some("h2".to_string()), HostOpKind::Other);
        let gate = |ops: &HostOpsInFlight| RemovalGate {
            owned: true,
            mirror_empty: true,
            ops_settled: ops.settled("h1"),
            creation_pending: ops.creating("h1"),
            switch_in_flight: false,
        };

        assert_eq!(removal_step(gate(&ops)), RemovalStep::Wait);
        ops.finish(7);
        assert_eq!(
            removal_step(gate(&ops)),
            RemovalStep::Execute,
            "h2's outstanding op is not h1's business"
        );
    }

    /// A creation owed to a host blocks its own removal and, separately,
    /// the whole window's exit — the two questions
    /// [`HostOpsInFlight`] answers.
    #[test]
    fn a_creation_in_flight_is_visible_to_both_the_host_and_the_window() {
        let mut ops = HostOpsInFlight::default();
        assert!(!ops.any_create());
        ops.begin(3, Some("h1".to_string()), HostOpKind::Other);
        assert!(
            !ops.any_create(),
            "an ordinary mutation is not a creation the window is waiting on"
        );
        assert!(!ops.creating("h1"));

        ops.begin(4, Some("h1".to_string()), HostOpKind::Create);
        assert!(ops.any_create());
        assert!(ops.creating("h1"));
        assert!(!ops.creating("h2"));

        ops.finish(4);
        assert!(!ops.any_create());
        assert!(!ops.settled("h1"), "op 3 is still out");
        ops.finish(3);
        assert!(ops.settled("h1"));

        // An op on a host this client no longer holds is not tracked:
        // there is nothing left for it to gate.
        ops.begin(5, None, HostOpKind::Create);
        assert!(!ops.any_create());
    }

    /// Plan 063 §D12: exactly the two purposes that own nothing else
    /// about the landing seed a host they find empty.
    #[test]
    fn only_a_connect_that_owes_nothing_else_seeds_an_empty_host() {
        assert!(ConnectPurpose::OrdinaryConnect.seeds());
        assert!(ConnectPurpose::EnsureNonempty.seeds());
        assert!(
            !ConnectPurpose::CreateAfterConnect.seeds(),
            "the caller creates; seeding too is two projects from one gesture"
        );
        assert!(
            !ConnectPurpose::SwitchDestination.seeds(),
            "the switch replays; a seed would be a duplicate of its first project"
        );
        // The default is what an unrecorded connect means, and it has to
        // be the one that behaves like every pre-063 connect.
        assert_eq!(ConnectPurpose::default(), ConnectPurpose::OrdinaryConnect);
    }

    /// A recent saved again under a name something else took while it
    /// was forgotten (plan 063 §D7) — the same suffix rule the slot's
    /// label follows, which is why it is the same function.
    #[test]
    fn a_recents_label_steps_past_one_that_was_taken_while_it_was_forgotten() {
        let taken =
            |names: &'static [&'static str]| move |candidate: &str| !names.contains(&candidate);
        assert_eq!(unique_label("box", taken(&[])).as_deref(), Some("box"));
        assert_eq!(
            unique_label("box", taken(&["box"])).as_deref(),
            Some("box (2)")
        );
        assert_eq!(
            unique_label("box", taken(&["box", "box (2)"])).as_deref(),
            Some("box (3)")
        );
        assert_eq!(unique_label("box", |_| false), None);
        // And the slot's own label is this rule with one base.
        assert_eq!(
            slot_label(taken(&["localhost"])).as_deref(),
            unique_label(
                roost_ui_model::host_verbs::SEED_LABEL,
                taken(&["localhost"])
            )
            .as_deref()
        );
    }

    /// The label rule (plan 063 §D7), driven through a registry check
    /// that behaves like the real one.
    #[test]
    fn the_slot_label_steps_past_a_name_an_ssh_host_already_holds() {
        let taken =
            |names: &'static [&'static str]| move |candidate: &str| !names.contains(&candidate);
        assert_eq!(slot_label(taken(&[])).as_deref(), Some("localhost"));
        assert_eq!(
            slot_label(taken(&["localhost"])).as_deref(),
            Some("localhost (2)")
        );
        assert_eq!(
            slot_label(taken(&["localhost", "localhost (2)"])).as_deref(),
            Some("localhost (3)")
        );
        // The counter starts at 2, so `local` — which the registry
        // reserves — is never a candidate.
        assert!(slot_label(|candidate| {
            assert_ne!(candidate, "local");
            candidate == "localhost (4)"
        })
        .is_some());
        // A registry refusing everything is a different problem; the
        // caller logs rather than looping.
        assert_eq!(slot_label(|_| false), None);
    }

    fn project(id: i64, tabs: &[i64]) -> Project {
        use roost_ipc::messages::{Tab, TabState};
        Project {
            id,
            name: format!("p{id}"),
            cwd: "/tmp".into(),
            position: 0,
            created_at: 0,
            tabs: tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| Tab {
                    id: *tab,
                    project_id: id,
                    title: String::new(),
                    cwd: "/tmp".into(),
                    state: TabState::Idle,
                    has_notification: false,
                    is_active: false,
                    user_titled: false,
                    position: index as i32,
                    created_at: 0,
                    last_active: 0,
                    hook_active: false,
                    shell_state: Default::default(),
                    agent_lifecycle: Default::default(),
                    ownership: None,
                })
                .collect(),
        }
    }

    /// Plan 063 §D5's initial selection, and AC2's "attached, not merely
    /// present": the answer is a tab.
    #[test]
    fn the_launch_selection_follows_the_sessions_own_active_tab() {
        let rows = [project(1, &[10, 11]), project(2, &[20, 21])];

        // The session's active tab, wherever it lives.
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&rows, 21))),
            InitialSelection::Select {
                project: 2,
                tab: 21
            }
        );
        // No active tab of its own (or one that closed): the first row
        // with something in it.
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&rows, 0))),
            InitialSelection::Select {
                project: 1,
                tab: 10
            }
        );
        // A project with no tabs is not somewhere to land.
        let empty = [project(1, &[]), project(2, &[20])];
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&empty, 0))),
            InitialSelection::Select {
                project: 2,
                tab: 20
            }
        );
    }

    /// The three ways there is nothing to do — including the one that
    /// keeps waiting, which is what a session seeded after connect
    /// depends on.
    #[test]
    fn the_launch_selection_waits_for_the_slot_and_yields_to_a_held_one() {
        let rows = [project(1, &[10])];
        assert_eq!(
            initial_selection(LocalBackendMode::InProcess, false, Some((&rows, 10))),
            InitialSelection::Settled,
            "in-process never had a slot to select from"
        );
        assert_eq!(
            initial_selection(LocalBackendMode::Session, true, Some((&rows, 10))),
            InitialSelection::Settled,
            "an unrelated remote-host selection is preserved"
        );
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, None),
            InitialSelection::Wait,
            "the slot is not connected yet"
        );
        assert_eq!(
            initial_selection(LocalBackendMode::Session, false, Some((&[], 0))),
            InitialSelection::Wait,
            "connected but empty: the seed has not landed yet"
        );
    }
}
