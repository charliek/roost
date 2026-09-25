//! Where this UI's own tabs run, what it publishes about that, and how
//! a launch decides between the two (plan 063 §D1/§D5/§D7).
//!
//! The [`LocalRoute`] snapshot is the only thing the engine's IPC
//! handler knows about the backend — it answers `identify` from it, and
//! nothing else in the UI may write it. [`App::publish_local_route`]
//! is the single writer; this module owns what it writes.
//!
//! [`App::publish_local_route`]: super::App::publish_local_route

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use roost_ipc::messages::Project;
use roost_ipc::{LocalBackendMode, LocalRoute};
use roost_ui_model::config::RoostConfig;
use roost_ui_model::keybind::KeybindAction;
use roost_ui_model::keys::{HostId, ProjectKey, TabKey};

use crate::config_writer::ConfigWriter;

/// What a palette row or keybind that would mutate the local backend is
/// refused with while a switch is in flight (plan 063 §D8a) — a toast, or
/// the error text of a `palette.activate` reply. The wire refusal is
/// `busy` with [`roost_ipc::local_route::SWITCH_BUSY_MESSAGE`].
const SWITCH_BUSY_PALETTE: &str = "busy: a local-backend switch is in progress";

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

/// What the UI knows about the slot, as one reading (plan 063
/// §D1/§D10).
///
/// The two fields travel together because they answer the same
/// question at two widths — *which* connection a bare id names, and
/// *which pair* a bare id defaults to — and a snapshot that had one
/// without the other would let `identify` report a selection on a slot
/// the forward arm would refuse.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotSelection {
    /// The slot's live incarnation, or `None` when it is not connected.
    pub(crate) host: Option<u32>,
    /// What this window has selected *on that incarnation*.
    pub(crate) active: Option<(i64, i64)>,
}

/// What a bare id names, given the slot and what this window has
/// selected (plan 063 §D1's `identify.active_*`, §D10's rewrite).
///
/// The filter is the whole of it: a selection counts only when it is on
/// **the slot's own incarnation**. A window showing an unrelated remote
/// host has no slot selection, because answering with the remote's ids
/// would send a `roostctl tab write` with no `--tab` to the wrong
/// machine — and a selection left over from a previous incarnation names
/// tabs the reconnected session has renumbered.
pub(super) fn slot_selection(
    slot: Option<HostId>,
    selection: Option<super::HostSelection>,
) -> SlotSelection {
    SlotSelection {
        host: slot.map(HostId::raw),
        active: selection
            .filter(|showing| Some(showing.tab.host) == slot)
            .map(|showing| (showing.project.project, showing.tab.tab)),
    }
}

/// The snapshot for `mode`.
///
/// Every derived field hangs off the mode: in-process has no slot, so it
/// has neither a slot socket nor an incarnation nor a slot selection.
/// The socket comes from the session bundle profile because that is what
/// determines it — the daemon binds that path whether or not anyone is
/// connected to it.
///
/// `switch` rides along rather than hanging off the mode, because it is
/// the one field that is *about* the mode moving: a client reading a
/// mode mid-switch has to be able to see that it is mid-switch.
pub(crate) fn route_snapshot(
    mode: LocalBackendMode,
    slot: SlotSelection,
    switch: SwitchState,
) -> LocalRoute {
    let switch = switch.wire();
    match mode {
        LocalBackendMode::InProcess => LocalRoute {
            mode,
            slot_socket: None,
            slot_host: None,
            slot_active: None,
            switch,
        },
        LocalBackendMode::Session => LocalRoute {
            mode,
            slot_socket: roost_ipc::session_socket_path(),
            slot_host: slot.host,
            slot_active: slot.active,
            switch,
        },
    }
}

// ── the switch (plan 063 §D8) ───────────────────────────────────────

/// Which way a switch is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchDirection {
    /// `local:use_session` — the in-process layout moves to the slot.
    ToSession,
    /// `local:use_in_process` — the key flips and nothing is copied
    /// back (§D8's No-Replay, owner-pinned 2026-09-13).
    ToInProcess,
}

impl SwitchDirection {
    pub(crate) fn source(self) -> LocalBackendMode {
        match self {
            Self::ToSession => LocalBackendMode::InProcess,
            Self::ToInProcess => LocalBackendMode::Session,
        }
    }

    pub(crate) fn destination(self) -> LocalBackendMode {
        match self {
            Self::ToSession => LocalBackendMode::Session,
            Self::ToInProcess => LocalBackendMode::InProcess,
        }
    }

    /// The direction a journal on disk describes. `None` for a pair of
    /// modes that is not a switch at all — a hand-written or truncated
    /// file, which is resolved by being ignored rather than acted on.
    fn of(from: LocalBackendMode, to: LocalBackendMode) -> Option<Self> {
        match (from, to) {
            (LocalBackendMode::InProcess, LocalBackendMode::Session) => Some(Self::ToSession),
            (LocalBackendMode::Session, LocalBackendMode::InProcess) => Some(Self::ToInProcess),
            _ => None,
        }
    }
}

/// The single latch plan 063 §D8a calls for.
///
/// Everything that could act on a half-migrated workspace reads it: both
/// switch verbs' "offered when", the local-backend mutations, §D6's
/// auto-remove, §D9's exit rule, and the Quit deferral. It is published
/// on `LocalRoute` too, so `identify` can say which phase a `busy`
/// refusal came from.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SwitchState {
    #[default]
    Idle,
    /// Getting the destination ready. Forward: save the slot if needed
    /// and connect it with [`ConnectPurpose::SwitchDestination`].
    /// Reverse: write the journal.
    Preparing,
    /// Forward only: `project.create` / `tab.open` / `tab.set_title`
    /// against the slot.
    Replaying,
    /// The key is written and the in-memory mode has flipped — **the
    /// commit point is inside this transition**, and this phase is the
    /// source teardown that follows it (forward: delete every
    /// in-process project).
    Committing,
    /// The last phase before `Idle`: the fence, and for the reverse the
    /// in-process seed. Held until the destination is really on screen,
    /// because releasing the guard is what re-arms the irreversible exit
    /// latch (§D8's phase 6).
    CleaningUp,
}

impl SwitchState {
    pub(crate) fn in_flight(self) -> bool {
        self != Self::Idle
    }

    /// The name `identify.local_backend_switch` reports, or `None` when
    /// nothing is in flight.
    pub(crate) fn wire(self) -> Option<&'static str> {
        match self {
            Self::Idle => None,
            Self::Preparing => Some("preparing"),
            Self::Replaying => Some("replaying"),
            Self::Committing => Some("committing"),
            Self::CleaningUp => Some("cleaning-up"),
        }
    }

    /// Whether the commit point is **behind** this phase — the key has
    /// been written and the mode has flipped.
    ///
    /// The one question a journal on disk is asked (§D8b): everything at
    /// or after it is finished forward, everything before it is rolled
    /// back.
    fn committed(self) -> bool {
        matches!(self, Self::Committing | Self::CleaningUp)
    }
}

/// One source project, as the forward snapshot records it (§D8 phase 2).
///
/// By position and content, never by id: the ids are the source
/// workspace's and mean nothing on the destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SwitchProject {
    /// The **source** project's id.
    ///
    /// Carried so phase 5 deletes the projects phase 3 replayed and no
    /// others: the id list is fixed when the snapshot is taken, so a
    /// project created while the replay was out is not in it and cannot
    /// be swept up by a commit that re-read the live workspace.
    #[serde(default, with = "source_id")]
    pub(crate) source: i64,
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) tabs: Vec<SwitchTab>,
}

/// Project ids are string-wrapped on every wire in this codebase; the
/// journal follows so a hand-written one reads the same way.
mod source_id {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(id: &i64, out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(&id.to_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<i64, D::Error> {
        let raw = String::deserialize(input)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SwitchTab {
    pub(crate) cwd: String,
    pub(crate) title: String,
    /// Only a user-titled tab gets a `tab.set_title` on the far side; a
    /// title the shell wrote is the shell's to write again.
    pub(crate) user_titled: bool,
}

/// What the forward replay actually created over there, in the order it
/// created it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CreatedProject {
    /// The source project this one is the copy of.
    pub(crate) source: i64,
    pub(crate) project: i64,
    /// The tabs that landed, each carrying **which source tab it is**.
    ///
    /// Positioned rather than a bare id list, because the list is
    /// *compacted*: a tab that could not be opened leaves no entry, so
    /// after a failure in the middle the nth entry is no longer the nth
    /// source tab. The active pair is mapped through here (§D8 phase 4),
    /// and reading it positionally selected the wrong tab.
    pub(crate) tabs: Vec<CreatedTab>,
    /// Whether **everything** the source project held reached the
    /// destination — every tab, and every title lock.
    ///
    /// The one input to phase 5's delete set, and the reason it exists:
    /// §D8 phase 3's cwd fallback is there so a *vanished directory*
    /// cannot abort a whole replay, not so a tab can be quietly dropped.
    /// A tab that still fails after the fallback (a spawn that failed, a
    /// host that refused) leaves this `false`, and the source project it
    /// came from is **kept** — the user ends up with it on both sides,
    /// which is recoverable, rather than short one tab, which is not.
    pub(crate) complete: bool,
}

/// One replayed tab: the destination id, and the position in the
/// source project's tab list it was copied from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CreatedTab {
    pub(crate) source: usize,
    pub(crate) tab: i64,
}

/// Plan 063 §D8b's crash record.
///
/// Written before the replay starts, rewritten as the phase moves, and
/// deleted once the last source project is gone. It is the whole of the
/// promise the ACs make — **mode and source layout are intact on any
/// reported failure or crash; a partial destination copy may remain and
/// is cleaned up on the next launch** — because there is no cross-file
/// atomicity to be had across a `config.conf` and two `state.json`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SwitchJournal {
    pub(crate) from_mode: LocalBackendMode,
    pub(crate) to_mode: LocalBackendMode,
    pub(crate) phase: SwitchState,
    /// The source layout as of phase 2. Recovery never replays from it
    /// — a rollback deletes the destination and a finish deletes the
    /// source — but it is what makes a partial destination copy
    /// attributable to a layout rather than to nothing.
    #[serde(default)]
    pub(crate) source_snapshot: Vec<SwitchProject>,
    /// The destination projects this switch created, appended as each
    /// one commits. **Appended, not written at the end**: a crash
    /// mid-replay is exactly the case the rollback exists for, and ids
    /// recorded only on completion would leave that copy orphaned.
    #[serde(default)]
    pub(crate) created_dest_ids: Vec<i64>,
    /// The **source** projects phase 5 is allowed to delete, written at
    /// the commit point.
    ///
    /// Not "every project in the source workspace", and not
    /// `source_snapshot`'s ids either: a crash between the commit and
    /// the last delete hands this list to the next launch, and that
    /// launch must make exactly the same distinction the running one
    /// did — the projects the replay landed *whole*. Without it the
    /// recovery would finish the job by deleting the very project the
    /// switch kept because the destination does not hold it.
    #[serde(default, with = "id_list")]
    pub(crate) deletable_sources: Vec<i64>,
    /// Whether this is plan 063 §D5's **launch-time** migration rather
    /// than a switch the user asked for.
    ///
    /// It changes exactly one thing, and only before the commit point:
    /// which mode the rollback settles on. A user's forward switch
    /// started from `in-process` and a failure owes them that back. A
    /// launch migration started from a key that already said `session`
    /// — nobody asked to leave it — so its rollback undoes the partial
    /// destination copy and **stays** on `session`, and the next launch
    /// finds the same populated workspace and tries again. Writing
    /// `in-process` there would silently un-edit the key the user set.
    #[serde(default)]
    pub(crate) launch_migration: bool,
    /// Destination projects an **earlier** switch left behind and this
    /// one adopted (plan 063 §D8b).
    ///
    /// Writing a journal replaces the one on disk, so without this a
    /// switch that starts while an unresolved rollback is still on file
    /// erases the only record of that abandoned copy: it is orphaned
    /// forever, and a replay then makes a second copy of everything it
    /// described. A forward switch clears this list *before* it replays;
    /// every recovery arm deletes what is left of it.
    ///
    /// Its own list rather than more `created_dest_ids`, because that
    /// one is paired with `source_snapshot` **by position** and these
    /// came from a different snapshot in a different process — so they
    /// carry their own expected tab count instead.
    #[serde(default)]
    pub(crate) inherited_dest: Vec<InheritedDest>,
}

/// One adopted destination project: its id, and the tab count the
/// switch that made it expected it to hold (`None` when that switch's
/// journal could not say).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InheritedDest {
    #[serde(with = "source_id")]
    pub(crate) project: i64,
    #[serde(default)]
    pub(crate) tabs: Option<usize>,
}

impl InheritedDest {
    fn target(self) -> (i64, Option<usize>) {
        (self.project, self.tabs)
    }

    fn of(target: (i64, Option<usize>)) -> Self {
        Self {
            project: target.0,
            tabs: target.1,
        }
    }
}

/// The same string-wrapping [`source_id`] does, over a list.
mod id_list {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(ids: &[i64], out: S) -> Result<S::Ok, S::Error> {
        out.collect_seq(ids.iter().map(i64::to_string))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Vec<i64>, D::Error> {
        Vec::<String>::deserialize(input)?
            .into_iter()
            .map(|raw| raw.parse().map_err(serde::de::Error::custom))
            .collect()
    }
}

impl SwitchJournal {
    pub(crate) fn new(direction: SwitchDirection, source_snapshot: Vec<SwitchProject>) -> Self {
        Self {
            from_mode: direction.source(),
            to_mode: direction.destination(),
            phase: SwitchState::Preparing,
            source_snapshot,
            created_dest_ids: Vec::new(),
            deletable_sources: Vec::new(),
            launch_migration: false,
            inherited_dest: Vec::new(),
        }
    }
}

/// The in-process layout plan 063 §D5's launch-time migration will
/// replay, taken before the window exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationSource {
    pub(crate) projects: Vec<SwitchProject>,
    /// The active pair as (project index, tab index) into `projects` —
    /// the same positional shape the in-place switch maps its selection
    /// through.
    pub(crate) active_at: Option<(usize, usize)>,
}

/// Read a `session`-mode launch's in-process layout, if it found one
/// (plan 063 §D5).
///
/// **The tabs come from the retained restore layout, not from the
/// projects.** A workspace loaded under `session` is never hydrated, so
/// every project row here has an empty `tabs` — the descriptors are
/// what `Workspace::open` kept, and they are the only record of the
/// user's tabs that exists. A snapshot taken from the live rows would
/// replay projects with no tabs at all and then delete the originals.
///
/// A project *with* live tabs still wins, because it is the newer
/// truth: that is the same precedence `Workspace::snapshot_for_persist`
/// applies, and it is what keeps this honest if it is ever reached from
/// a workspace that has been hydrated.
pub(crate) fn retained_migration(
    projects: &[Project],
    layout: Option<&roost_engine::RestoreLayout>,
) -> Option<MigrationSource> {
    if projects.is_empty() {
        return None;
    }
    let retained = |project_id: i64| {
        layout
            .and_then(|layout| {
                layout
                    .projects
                    .iter()
                    .find(|row| row.project_id == project_id)
            })
            .map(|row| row.tabs.as_slice())
            .unwrap_or(&[])
    };
    let source: Vec<SwitchProject> = projects
        .iter()
        .map(|project| SwitchProject {
            source: project.id,
            name: project.name.clone(),
            cwd: project.cwd.clone(),
            tabs: match project.tabs.is_empty() {
                true => retained(project.id)
                    .iter()
                    .map(|tab| SwitchTab {
                        cwd: tab.cwd.clone(),
                        title: tab.title.clone(),
                        user_titled: tab.user_titled,
                    })
                    .collect(),
                false => project
                    .tabs
                    .iter()
                    .map(|tab| SwitchTab {
                        cwd: tab.cwd.clone(),
                        title: tab.title.clone(),
                        user_titled: tab.user_titled,
                    })
                    .collect(),
            },
        })
        .collect();
    // The retained selection, in the same positional coordinates the
    // replay maps through. `active_tab_position` is already the dense
    // display index `RestoreLayout` promises, so it indexes the tab list
    // built above directly.
    let active_at = layout.and_then(|layout| {
        let project = source
            .iter()
            .position(|row| row.source == layout.active_project_id)?;
        let tab = usize::try_from(layout.active_tab_position).ok()?;
        (tab < source[project].tabs.len()).then_some((project, tab))
    });
    Some(MigrationSource {
        projects: source,
        active_at,
    })
}

/// Where the journal lives: beside `state.json`, in the UI's state dir.
pub(crate) fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join("switch-journal.json")
}

/// Write it, atomically.
///
/// tmp + rename for `state.json`'s reason: a torn journal is worse than
/// no journal, because the recovery would act on half a record. No
/// fsync — the same trade the workspace makes (`CLAUDE.md`): a crash may
/// lose the last phase update, and every phase is resolvable from the
/// one before it.
pub(crate) fn write_journal(path: &Path, journal: &SwitchJournal) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(journal)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

/// Read it, if there is one.
///
/// An unreadable or undecodable file answers `None` **and is left in
/// place**: it is not a journal this build can act on, and deleting it
/// would destroy the only record of whatever wrote it. A launch with a
/// garbage journal is an ordinary launch.
pub(crate) fn read_journal(path: &Path) -> Option<SwitchJournal> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not read the switch journal");
            return None;
        }
    };
    match serde_json::from_slice::<SwitchJournal>(&raw) {
        Ok(journal) => Some(journal),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the switch journal does not decode");
            None
        }
    }
}

pub(crate) fn clear_journal(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "switch journal cleared"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not clear the switch journal")
        }
    }
}

/// What a launch owes a journal it found (plan 063 §D8b, resolved as
/// §D5's step 1 — **before** the [`ladder`], which is step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JournalRecovery {
    /// The switch got past the commit point. Its `to_mode` is the mode
    /// this launch runs on; the key is (re)written to it because the
    /// crash may have landed between the two writes, and the source
    /// teardown it did not finish is finished now.
    Finish {
        mode: LocalBackendMode,
        /// What an *earlier* switch left on the destination and this one
        /// adopted without resolving — a reverse, which copies nothing
        /// and so never runs the forward replay that clears the list.
        /// Never this switch's own copy: that is the work it committed.
        delete_dest: Vec<(i64, Option<usize>)>,
        /// The source projects to delete — a forward switch that
        /// committed but did not finish emptying the source.
        ///
        /// A **list**, not "everything in the workspace": the running
        /// switch deletes only the projects it landed whole, and a
        /// recovery that swept the workspace would delete the one it
        /// deliberately kept. Empty for a reverse, which copies nothing
        /// and so has nothing to tear down.
        delete_source: Vec<i64>,
    },
    /// It did not. The key goes back to `from_mode` — written, not
    /// assumed, because the crash may have landed *after* the key write
    /// and before the phase update — and whatever the destination
    /// already holds is deleted.
    ///
    /// `mode` is `to_mode` for §D5's launch migration, whose `from_mode`
    /// is not a mode anybody chose: see
    /// [`SwitchJournal::launch_migration`].
    RollBack {
        mode: LocalBackendMode,
        /// Each destination project paired with the tab count the
        /// journal says the replay left on it — see
        /// [`rollback_is_still_ours`], which is what stops this from
        /// deleting a project somebody else has worked in since.
        delete_dest: Vec<(i64, Option<usize>)>,
    },
    /// The file describes no switch this build knows how to resolve.
    /// Nothing happens and the ladder decides the mode.
    Ignore,
}

/// Decide it.
///
/// The asymmetry is the whole design. Past the commit point there is a
/// destination full of the user's work and a key that says so, and the
/// only safe move is forward. Before it there is a *partial* destination
/// copy and a source that is still whole, and the only safe move is
/// back — which is why the promise is "source intact", not "the switch
/// completes".
///
/// The key is rewritten on **both** arms rather than trusted, because
/// the crash window that matters is the one between `set_key` and the
/// journal's phase update: on either side of it the file on disk may
/// disagree with the phase, and only the phase knows whether the
/// destination is whole.
pub(crate) fn journal_recovery(journal: &SwitchJournal) -> JournalRecovery {
    let Some(direction) = SwitchDirection::of(journal.from_mode, journal.to_mode) else {
        tracing::warn!(
            from = %journal.from_mode,
            to = %journal.to_mode,
            "the switch journal names no switch"
        );
        return JournalRecovery::Ignore;
    };
    if journal.phase.committed() {
        return JournalRecovery::Finish {
            mode: direction.destination(),
            delete_dest: inherited_targets(journal),
            // Only a forward switch empties a source. Reverse is
            // No-Replay: it copies nothing and so has nothing to tear
            // down, and deleting the in-process workspace there would
            // destroy the very layout it is coming back to.
            delete_source: match direction {
                SwitchDirection::ToSession => journal.deletable_sources.clone(),
                SwitchDirection::ToInProcess => Vec::new(),
            },
        };
    }
    JournalRecovery::RollBack {
        // A launch migration has no source mode to go back to: the key
        // already said `session` before it started, and the layout it
        // was moving is still whole in the in-process workspace. So the
        // copy goes and the mode stays, which leaves the next launch the
        // same populated workspace to migrate again.
        mode: match journal.launch_migration {
            true => direction.destination(),
            false => direction.source(),
        },
        delete_dest: rollback_targets(journal),
    }
}

/// The destination projects a rollback may delete, each with the tab
/// count the journal says the replay left there (plan 063 §D8b).
///
/// `created_dest_ids[i]` is the copy of `source_snapshot[i]`: the
/// replay appends the id the moment `project.create` answers, before it
/// opens a single tab, so the two lists share an order and the shorter
/// one is the crash point.
pub(crate) fn rollback_targets(journal: &SwitchJournal) -> Vec<(i64, Option<usize>)> {
    inherited_targets(journal)
        .into_iter()
        .chain(
            journal
                .created_dest_ids
                .iter()
                .enumerate()
                .map(|(index, project)| {
                    (
                        *project,
                        journal
                            .source_snapshot
                            .get(index)
                            .map(|source| source.tabs.len()),
                    )
                }),
        )
        .collect()
}

/// Just the adopted half — what a switch that **committed** still owes,
/// since its own copy is the thing it was for.
pub(crate) fn inherited_targets(journal: &SwitchJournal) -> Vec<(i64, Option<usize>)> {
    journal
        .inherited_dest
        .iter()
        .copied()
        .map(InheritedDest::target)
        .collect()
}

/// Whether a destination project a rollback named is still the copy the
/// replay made, and so still this switch's to delete.
///
/// **"I created this id" is not grounds to delete it later.** A session
/// serves every client at once — that is the whole point of it — so
/// between the crash and this rollback somebody may have opened a tab
/// in the very project the replay left behind and started working
/// there. `project.delete` cascades, so deleting it would take that
/// work with it. Same reading §D6's auto-remove arrived at: ask what is
/// there *now*, rather than acting on what this client remembers doing.
///
/// The question it can answer is "has it gained tabs since". The replay
/// opens at most `source_snapshot[i].tabs.len()` of them and nothing
/// else on a `SwitchDestination` connect creates any (the seed is
/// withheld, §D12), so anything above that count came from somewhere
/// else. Fewer is ordinary — that is a crash mid-project.
///
/// A journal with no snapshot entry for the id vouches for nothing, and
/// so deletes nothing. Every journal a real replay writes has one: it
/// is written at phase 2, before the first `project.create`.
pub(crate) fn rollback_is_still_ours(expected_tabs: Option<usize>, found_tabs: usize) -> bool {
    expected_tabs.is_some_and(|expected| found_tabs <= expected)
}

/// Put the `local-backend` key back to `mode` on a run that is unwinding.
///
/// Warned, not raised: the run is already failing, and the journal on
/// disk still describes an uncommitted switch — so the next launch
/// rewrites this key from [`journal_recovery`] whatever happens here.
///
/// Awaited rather than queued so the caller's next step sees the key it
/// put back, which is the order the crash windows below are reasoned
/// about in.
async fn put_backend_key_back(writer: &ConfigWriter, mode: LocalBackendMode) {
    if let Err(error) = writer.record("local-backend", mode.as_str()).await {
        tracing::warn!(%error, %mode, "could not put the local-backend key back");
    }
}

/// Retire a failing run's journal, and answer with what the app must go
/// on remembering (plan 063 §D8b).
///
/// **The file is never dropped over an adopted copy, at any call site.**
/// `arm_switch` moves the app's outstanding list into the run at the one
/// moment the file is about to be replaced, so from then until the run
/// ends the pair — the file on disk and the run in memory — is the only
/// record that copy exists. Clearing the file on a failure would leave
/// the ids in a run `end_switch` is about to drop: orphaned for good,
/// which is the whole thing the adoption exists to prevent. Every
/// pre-commit failure can be reached with that list populated — a label
/// that is taken, a session that will not start, a journal that will not
/// write, a phase that times out — so this is a property of the ending
/// rather than a check some of them remember to make.
///
/// The copies **this** run made are a different question and not this
/// one's: they are named by the same file, and §D8b's answer for them is
/// that a partial destination copy may remain until a launch can reach
/// it.
fn retire_failed_journal(path: &Path, journal: &SwitchJournal) -> Vec<(i64, Option<usize>)> {
    let adopted = inherited_targets(journal);
    if adopted.is_empty() {
        clear_journal(path);
    }
    adopted
}

/// The reverse's commit point as the two writes it is: the key, then the
/// journal that says the key may be trusted.
///
/// **Both are refusals, and the order is the forward commit's order for
/// the forward commit's reason.** The key goes first so the crash window
/// between them is the recoverable one (a journal still reading
/// `preparing` over a key that says `in-process` resolves as an
/// uncommitted reverse and puts `session` back). Which is exactly why a
/// journal write that *fails* cannot be logged and stepped over: past
/// this point the mode flips and the run reports success, and the next
/// launch would read that same `preparing` and restore `session` —
/// undoing a completed switch behind the user's back. So it refuses,
/// and puts the key back on its way out.
///
/// Free of the app so the refusal can be driven directly. Off the main
/// thread because the key write takes `config.lock` — see
/// [`crate::config_writer`].
async fn reverse_commit_record(
    writer: &ConfigWriter,
    journal_path: &Path,
    journal: &mut SwitchJournal,
) -> Result<(), String> {
    if let Err(error) = writer
        .record("local-backend", LocalBackendMode::InProcess.as_str())
        .await
    {
        return Err(format!("could not record the local backend: {error}"));
    }
    let uncommitted = journal.phase;
    journal.phase = SwitchState::Committing;
    if let Err(error) = write_journal(journal_path, journal) {
        journal.phase = uncommitted;
        put_backend_key_back(writer, LocalBackendMode::Session).await;
        return Err(format!("could not record the switch commit: {error}"));
    }
    Ok(())
}

/// Where the source's active tab ended up (plan 063 §D8 phase 4).
///
/// `active_at` is a **source** position pair, because the destination's
/// ids are not the source's. Resolving it has one trap, and it is the
/// reason [`CreatedTab`] carries a position at all: `made.tabs` is
/// *compacted*, so a tab that could not be opened leaves no entry and
/// every tab after it shifts down one. Indexing that list would select
/// a tab the user was not looking at — and the further the failure is
/// from the end, the further off the answer.
///
/// The fallback is the first tab of the first project the replay landed
/// whole. A switch that moved the user's work and then selected nothing
/// leaves them looking at an empty pane, and the pair it was asked for
/// may name a project or a tab that never arrived.
pub(crate) fn mapped_selection(
    created: &[CreatedProject],
    active_at: Option<(usize, usize)>,
) -> Option<(i64, i64)> {
    active_at
        .and_then(|(project, tab)| {
            let made = created.get(project)?;
            let landed = made.tabs.iter().find(|landed| landed.source == tab)?;
            Some((made.project, landed.tab))
        })
        .or_else(|| {
            created
                .iter()
                .find(|made| made.complete && !made.tabs.is_empty())
                .map(|made| (made.project, made.tabs[0].tab))
        })
}

/// The source projects phase 5 may delete (plan 063 §D8 phase 5).
///
/// **Exactly, and only, what phase 3 proved present at the
/// destination.** Two ways to get this wrong, and they are the same
/// mistake: reading the *live* project list sweeps up anything created
/// while the replay was out, and reading the snapshot unfiltered
/// deletes a project whose tab never landed over there. Both delete a
/// source for work the destination does not hold.
///
/// So the answer is derived from `created` alone — which is built from
/// the snapshot, one entry per project the replay actually committed,
/// each carrying whether it landed whole.
pub(crate) fn deletable_sources(created: &[CreatedProject]) -> Vec<i64> {
    created
        .iter()
        .filter(|made| made.complete)
        .map(|made| made.source)
        .collect()
}

/// Plan 063 §D8's phase 6, as one predicate.
///
/// **A bare `reconcile()` is not this.** The control replies that named
/// the destination ids can — and routinely do — arrive before the
/// broadcasts that put those rows in the mirror (§2: "the control reply
/// can precede the broadcast"), so a reconcile taken the instant the
/// replay future returns can see an empty band. Releasing the guard
/// there re-arms §D9's exit rule against a window whose only rows have
/// not landed yet, and [`super::ExitState`] is irreversible: the app
/// would close over a switch that had just succeeded.
///
/// So the fence is asked of the **mirror**, which is the subscription's
/// view and therefore strictly behind the replies, and of the selection
/// the switch mapped, which is what AC3 means by "selects the mapped
/// tab".
#[derive(Debug, Clone, Copy)]
pub(crate) struct SwitchFence<'a> {
    /// What the replay's replies said it made.
    pub(crate) created: &'a [CreatedProject],
    /// What the slot's mirror is currently carrying.
    pub(crate) mirrored: &'a [Project],
    /// The selection the UI holds on the slot right now.
    pub(crate) selected: Option<(i64, i64)>,
    /// The one it is supposed to hold, mapped from the source's active
    /// pair. `None` when the source had no selection to map.
    pub(crate) wanted: Option<(i64, i64)>,
    /// Whether the selected tab's attach has reached its live stream.
    ///
    /// Separate from `selected` because they are separate facts:
    /// selecting starts an attach, and an attach can be refused. Only
    /// consulted when there is a tab to be attached to — a source with
    /// nothing selected asks for no attachment.
    pub(crate) attached: bool,
}

pub(crate) fn fence_holds(fence: SwitchFence<'_>) -> bool {
    // The band is on screen. Checked in its own right rather than
    // inferred from `created`: a switch of an empty source creates
    // nothing, and releasing the guard onto an empty band is the exit
    // this fence exists to prevent.
    if fence.mirrored.is_empty() {
        return false;
    }
    let mirrored = |project: i64, tab: i64| {
        fence
            .mirrored
            .iter()
            .any(|row| row.id == project && row.tabs.iter().any(|row| row.id == tab))
    };
    // Only the projects the replay landed **whole**. An incomplete one
    // is precisely the project this switch does not vouch for: its
    // source is kept, and the destination may hold a husk of it or —
    // when the tab that failed was its only one — nothing at all,
    // because the engine's own spawn-failure path closes the tab it
    // opened and closing a project's last tab deletes the project. A
    // fence that waited for those rows would wait for rows that are
    // never coming, and release only on the phase backstop.
    let every_row = fence
        .created
        .iter()
        .filter(|created| created.complete)
        .all(|created| {
            fence.mirrored.iter().any(|row| row.id == created.project)
                && created
                    .tabs
                    .iter()
                    .all(|made| mirrored(created.project, made.tab))
        });
    if !every_row {
        return false;
    }
    match fence.wanted {
        // Selected *and* still listed: a selection pointing at a row the
        // mirror does not carry is not a tab anybody is looking at.
        Some((project, tab)) => {
            fence.selected == Some((project, tab)) && mirrored(project, tab) && fence.attached
        }
        None => true,
    }
}

/// The keybind actions a switch quiesces (plan 063 §D8a).
///
/// The four that create or destroy a project or a tab in whichever
/// workspace the local band is currently drawing. Everything else — a
/// rename, a cycle, a font size, the palette — either touches no layout
/// or is answered by a surface the switch does not move.
///
/// Enumerated as a function rather than checked at each arm so the list
/// is one thing with one test, and so a new mutating action is a visible
/// omission rather than a silent hole.
pub(crate) fn keybind_mutates_local_backend(action: KeybindAction) -> bool {
    matches!(
        action,
        KeybindAction::NewTab
            | KeybindAction::NewProject
            | KeybindAction::CloseTab
            | KeybindAction::CloseProject
    )
}

/// The same four, as the command palette's own row ids — the other
/// surface that reaches them, and the one `palette.activate` gives a
/// `roostctl` client.
pub(crate) fn palette_row_mutates_local_backend(id: &str) -> bool {
    matches!(
        id,
        "new_tab" | "new_project" | "close_tab" | "close_project"
    )
}

/// What the forward confirm card says.
///
/// The counts are named because they are the thing being moved and the
/// user is the only one who knows whether they are right; "running local
/// shells end" is named because it is the irreversible part.
pub(crate) fn forward_confirm_body(projects: usize, tabs: usize) -> String {
    format!(
        "{} and {} move to a roost-session on this machine. \
         The shells running in them end and start fresh over there; \
         the session then keeps running when Roost quits.",
        plural(projects, "project"),
        plural(tabs, "tab"),
    )
}

/// What the reverse confirm card says — No-Replay stated plainly, since
/// the surprising part is what does *not* happen (§D8's "Reverse").
pub(crate) fn reverse_confirm_body(label: &str) -> String {
    format!(
        "Local tabs go back to running inside Roost and start fresh — \
         nothing is copied back. The session keeps running and its \
         projects stay one click away under {label}."
    )
}

pub(super) fn plural(count: usize, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

// ── the switch, as the app runs it (plan 063 §D8) ───────────────────

/// One switch in flight — everything the phases need that the `App`
/// itself has no other reason to hold.
#[derive(Debug)]
pub(crate) struct SwitchRun {
    pub(crate) direction: SwitchDirection,
    pub(crate) state: SwitchState,
    /// Distinguishes this run's step completions from an abandoned
    /// run's. A step is a future on the engine runtime; nothing can
    /// recall one.
    generation: u64,
    /// The slot. Forward addresses its replay here; reverse only names
    /// it in the confirm copy.
    saved_id: String,
    /// This run saved the slot, so a failed prepare un-saves it — the
    /// promise is that a reported failure changed nothing.
    added_slot: bool,
    /// The destination connect has been asked for. Asked once: a second
    /// dial per reconcile would supersede the attempt it is waiting on.
    dialed: bool,
    /// A step future is out. Nothing advances under it, and a Quit
    /// waits for it (§D8a's "safe point").
    step_in_flight: bool,
    journal: SwitchJournal,
    /// Set when this run is plan 063 §D5's launch-time migration, and
    /// then it **is** phase 2's snapshot: the layout was read off the
    /// retained restore descriptors at bootstrap, because a workspace
    /// loaded under `session` is never hydrated and its live project
    /// rows carry no tabs at all.
    migration: Option<MigrationSource>,
    /// What the replay's replies said it made, in creation order.
    created: Vec<CreatedProject>,
    /// The source's active pair, as (project index, tab index) into the
    /// snapshot. Positions, because the destination's ids are not the
    /// source's.
    active_at: Option<(usize, usize)>,
    /// That pair, resolved against `created`.
    wanted: Option<(i64, i64)>,
    /// How many source projects the replay did **not** land whole, and
    /// which phase 5 therefore left where they were. Nonzero is a switch
    /// that worked and still owes the user a sentence about it.
    kept: usize,
    /// When the phase this run is in stops being worth waiting for.
    /// Re-armed at every transition.
    deadline: std::time::Instant,
    /// The IPC socket's admission gate has drained for this run (plan
    /// 067 §3.2). The forward snapshot and the reverse seed wait for it.
    admission_drained: bool,
    /// That drain, aborted when the run ends: one still queued behind a
    /// mutation that never replies would, through the lock's write
    /// preference, hold every later mutation on the socket behind it
    /// long after the switch that asked for it was gone.
    drain: Option<tokio::task::AbortHandle>,
}

/// How long any one phase may sit waiting on something outside this
/// process — a destination connect, or a mirror catching up.
///
/// A backstop, not a schedule: every phase has a real completion edge,
/// and this exists because the guard suppresses the exit latch. A switch
/// wedged forever would be a window that can never be closed.
fn phase_deadline() -> std::time::Instant {
    std::time::Instant::now()
        + std::time::Duration::from_secs(120).mul_f64(crate::host_conn::task::scale())
}

/// What the forward replay did, whether or not it finished.
///
/// The partial is carried on the failure path too, and that is the
/// point: those projects exist on the destination and something has to
/// delete them.
#[derive(Debug)]
pub(crate) struct ReplayOutcome {
    pub(crate) created: Vec<CreatedProject>,
    /// Whatever is left of an adopted copy (§D8b's `inherited_dest`).
    /// Empty once the replay has cleared it, which it does before it
    /// creates anything — so a non-empty list here is always paired
    /// with an `error`.
    pub(crate) inherited_left: Vec<InheritedDest>,
    pub(crate) error: Option<String>,
}

/// A switch step finishing on the engine runtime, on its way back to the
/// main thread through the feed.
///
/// It rides the feed rather than an Iced task for [`EngineFeed::HostEmptiness`]'s
/// reason: the steps are started from a reconcile, which cannot return
/// one.
///
/// [`EngineFeed::HostEmptiness`]: crate::engine_feed::EngineFeed::HostEmptiness
#[derive(Debug)]
pub(crate) struct SwitchStepDone {
    pub(crate) generation: u64,
    pub(crate) step: SwitchStep,
}

#[derive(Debug)]
pub(crate) enum SwitchStep {
    /// The forward replay ran (phase 3).
    Replayed(ReplayOutcome),
    /// The source teardown ran (phase 5). Carries what it could not
    /// delete, which is what decides whether the journal may go.
    SourceCleared { left: usize },
    /// A rollback's destination deletion ran. `false` means the
    /// destination still holds some of the copy, so the journal stays
    /// for the next launch to finish.
    DestCleared { complete: bool },
    /// The reverse's in-process hydrate ran — a seed, a restore of the
    /// rows a session-mode launch retained, or both.
    Seeded(Result<(), String>),
    /// The forward commit's two records landed, or did not (phase 4).
    Committed(Result<(), String>),
    /// The reverse commit's two records landed, or did not.
    ReverseCommitted(Result<(), String>),
}

impl super::App {
    pub(crate) fn switch_state(&self) -> SwitchState {
        self.switch
            .as_ref()
            .map_or(SwitchState::Idle, |run| run.state)
    }

    /// §D8a's latch, as everything that must quiesce reads it.
    pub(crate) fn switch_in_flight(&self) -> bool {
        self.switch_state().in_flight()
    }

    /// Whether a step future is out — the Quit deferral's "safe point"
    /// (§D8a). Between phases the journal fully describes where the
    /// switch is, so the process may go; mid-step it does not, because
    /// the step is still writing to the destination.
    pub(crate) fn switch_step_in_flight(&self) -> bool {
        self.switch.as_ref().is_some_and(|run| run.step_in_flight)
    }

    fn switch_gate_drained(&self) -> bool {
        self.switch
            .as_ref()
            .is_some_and(|run| run.admission_drained)
    }

    /// The stable refusal every local-backend mutation gets while a
    /// switch is in flight (§D8a).
    pub(crate) fn refuse_during_switch(&self) -> Result<(), String> {
        match self.switch_in_flight() {
            true => Err(SWITCH_BUSY_PALETTE.to_string()),
            false => Ok(()),
        }
    }

    fn journal_path(&self) -> std::path::PathBuf {
        journal_path(&self.state_dir)
    }

    // ── the verbs ───────────────────────────────────────────────────

    /// `local:use_session` / `local:use_in_process`: raise the confirm.
    ///
    /// The copy is composed here and carried by the dialog, for
    /// `ConfirmStop`'s reason — the card says what was true when it
    /// opened, and the counts must not be rewritten under the user's
    /// eyes by a tab exiting behind the modal.
    pub(crate) fn open_local_switch_dialog(
        &mut self,
        direction: SwitchDirection,
    ) -> Result<(), String> {
        self.refuse_during_switch()?;
        if self.local_backend != direction.source() {
            return Err(format!(
                "the local backend is already {}",
                direction.destination()
            ));
        }
        let (title, body, confirm) = match direction {
            SwitchDirection::ToSession => {
                let tabs = self
                    .projects
                    .iter()
                    .map(|project| project.tabs.len())
                    .sum::<usize>();
                (
                    "Use a session for local tabs?".to_string(),
                    forward_confirm_body(self.projects.len(), tabs),
                    "Move to a session",
                )
            }
            SwitchDirection::ToInProcess => {
                let label = self
                    .local_slot_saved_id()
                    .and_then(|saved_id| self.host_label(&saved_id))
                    .unwrap_or_else(|| roost_ui_model::host_verbs::SEED_LABEL.to_string());
                (
                    "Use in-process local tabs?".to_string(),
                    reverse_confirm_body(&label),
                    "Use in-process tabs",
                )
            }
        };
        self.open_host_dialog(super::host_dialog::HostDialog::ConfirmSwitch {
            direction,
            title,
            body,
            confirm,
        });
        Ok(())
    }

    /// The confirm card's primary button.
    pub(crate) fn local_switch_confirmed(&mut self) {
        let Some(super::host_dialog::HostDialog::ConfirmSwitch { direction, .. }) =
            self.host_dialog.take()
        else {
            return;
        };
        // Re-read, for `take_confirmed_restart_prompt`'s reason: the
        // modal is modal to the pointer, not to the world, and an IPC
        // client can have switched the backend while the card was up.
        if self.local_backend != direction.source() {
            self.set_status(format!(
                "the local backend is already {} — nothing was changed",
                self.local_backend
            ));
            return;
        }
        self.begin_switch(direction);
    }

    fn begin_switch(&mut self, direction: SwitchDirection) {
        self.arm_switch(direction, None);
        self.reconcile();
    }

    /// Put a run in place. Split from [`Self::begin_switch`] because the
    /// launch migration arms from *inside* a reconcile and must not
    /// start a nested one.
    fn arm_switch(&mut self, direction: SwitchDirection, migration: Option<MigrationSource>) {
        self.switch_generation = self.switch_generation.wrapping_add(1);
        let saved_id = self.local_slot_saved_id().unwrap_or_default();
        let mut journal = SwitchJournal::new(direction, Vec::new());
        journal.launch_migration = migration.is_some();
        // Adopted here, at the one moment the journal on disk is about
        // to be replaced: an unresolved rollback recorded only there
        // would be erased, and the replay below would then make a second
        // copy of everything that record described.
        journal.inherited_dest = std::mem::take(&mut self.pending_dest_cleanup)
            .into_iter()
            .map(InheritedDest::of)
            .collect();
        if !journal.inherited_dest.is_empty() {
            tracing::info!(
                projects = journal.inherited_dest.len(),
                "this switch adopts an earlier one's unresolved destination copy"
            );
        }
        self.switch = Some(SwitchRun {
            direction,
            state: SwitchState::Preparing,
            generation: self.switch_generation,
            saved_id,
            added_slot: false,
            dialed: false,
            step_in_flight: false,
            journal,
            migration,
            created: Vec::new(),
            active_at: None,
            wanted: None,
            kept: 0,
            deadline: phase_deadline(),
            admission_drained: false,
            drain: None,
        });
        // Before the first phase runs: the latch is what stops the
        // reconciles the phases trigger from auto-removing an emptied
        // slot or closing the window over an emptied source.
        self.publish_local_route();
        self.in_process_streams.end_for_backend_switch();
        // After the store above, never before it — see
        // `roost_engine::ipc::IpcHandler::switch_gate`.
        let gate = std::sync::Arc::clone(&self.switch_gate);
        let feed = self.feed_tx.clone();
        let generation = self.switch_generation;
        let drain = self.runtime_handle.spawn(async move {
            drop(gate.write().await);
            feed.send(crate::engine_feed::EngineFeed::SwitchAdmissionDrained { generation });
        });
        self.switch.as_mut().expect("a run").drain = Some(drain.abort_handle());
        tracing::info!(?direction, "local-backend switch started");
    }

    // ── the launch-time migration (plan 063 §D5) ────────────────────

    /// Start the migration a `session` launch owes, once there is a slot
    /// to run it against.
    ///
    /// **Parked, not driven.** The migration waits for the slot to
    /// connect rather than dialing one of its own: §D5 says "after the
    /// slot connects", and the launch's own `reconnect_saved_hosts`
    /// already dials it — with [`ConnectPurpose::SwitchDestination`],
    /// so a session that comes up empty is not seeded under the replay.
    /// A slot that never connects therefore leaves this parked forever
    /// and costs nothing: no latch, so no phase backstop, no refused
    /// `Cmd-N`, and the band shows the spawn failure with ↻ exactly as
    /// §D5's spawn-failure clause says.
    ///
    /// It is dropped the moment the mode is no longer `session`. A
    /// snapshot of source ids outlives its own workspace otherwise: the
    /// user reverses, the rows hydrate, a later forward switch moves and
    /// deletes them, and a migration armed from the stale snapshot would
    /// replay projects that no longer exist onto the session a second
    /// time.
    fn arm_pending_migration(&mut self) {
        if self.local_backend != LocalBackendMode::Session {
            if self.pending_migration.take().is_some() {
                tracing::info!("the local backend left session; the launch migration is dropped");
            }
            return;
        }
        if self.pending_migration.is_none()
            || self.switch_in_flight()
            || self.connected_slot_host().is_none()
        {
            return;
        }
        let migration = self.pending_migration.take().expect("a pending migration");
        tracing::info!(
            projects = migration.projects.len(),
            "migrating the in-process workspace onto the local session"
        );
        self.arm_switch(SwitchDirection::ToSession, Some(migration));
    }

    // ── the driver ──────────────────────────────────────────────────

    /// Run the state machine as far as it can go without waiting on
    /// something outside this process.
    ///
    /// Called from `reconcile`, which is the one hook every input the
    /// phases wait on already passes through — a host reaching
    /// `Connected`, a mirror batch, a step completion. Reentrant by
    /// construction (a phase calls `host_add_requested`, which
    /// reconciles), so the guard is a flag rather than a discipline.
    pub(super) fn drive_switch(&mut self) {
        if self.switch_driving {
            return;
        }
        self.switch_driving = true;
        self.arm_pending_migration();
        while self.advance_switch() {}
        self.switch_driving = false;
    }

    /// One transition. `true` when the state moved without waiting, so
    /// the driver should look again.
    fn advance_switch(&mut self) -> bool {
        let Some(run) = self.switch.as_ref() else {
            return false;
        };
        if run.step_in_flight {
            return false;
        }
        if std::time::Instant::now() >= run.deadline {
            return self.switch_timed_out();
        }
        match (run.direction, run.state) {
            (SwitchDirection::ToSession, SwitchState::Preparing) => self.forward_prepare(),
            (SwitchDirection::ToSession, SwitchState::CleaningUp) => self.forward_fence(),
            (SwitchDirection::ToInProcess, SwitchState::Preparing) => self.reverse_prepare(),
            (SwitchDirection::ToInProcess, SwitchState::Committing) => self.reverse_commit(),
            (SwitchDirection::ToInProcess, SwitchState::CleaningUp) => self.reverse_finish(),
            // Every other pair is waiting on a step that is out, which
            // `step_in_flight` already answered, or is `Idle`, which has
            // no run.
            _ => false,
        }
    }

    /// A phase that ran out of patience.
    ///
    /// Which way it fails is decided by the commit point, exactly as the
    /// journal's recovery is: before it, nothing has been promised and
    /// the run is abandoned with the source intact; after it, the mode
    /// has already moved and the only thing left is to stop holding the
    /// guard — the journal (or its absence) has already recorded a
    /// completed switch.
    fn switch_timed_out(&mut self) -> bool {
        let Some(run) = self.switch.as_ref() else {
            return false;
        };
        let state = run.state;
        tracing::warn!(?state, "a local-backend switch phase timed out");
        match state {
            SwitchState::CleaningUp | SwitchState::Committing => {
                self.finish_switch("the local backend moved, but the new band is slow to appear")
            }
            _ => self.fail_switch("the local backend could not be switched"),
        }
        false
    }

    // ── forward (`local:use_session`) ───────────────────────────────

    /// Phase 1: the destination.
    fn forward_prepare(&mut self) -> bool {
        // (a) A slot to be. An addition is recorded as *this run's* so
        // a failure below leaves the registry as it found it.
        match self.local_slot_saved_id() {
            Some(saved_id) => self.switch.as_mut().expect("a run").saved_id = saved_id,
            None => {
                let Some(label) = self.slot_label() else {
                    self.fail_switch("no free label for this machine's session");
                    return false;
                };
                match self.host_add_requested(&label, crate::host_conn::LOCALHOST_TARGET, None) {
                    Ok(host) => {
                        let run = self.switch.as_mut().expect("a run");
                        run.saved_id = host.id;
                        run.added_slot = true;
                    }
                    Err(error) => {
                        self.fail_switch(&format!(
                            "could not save this machine's session: {error}"
                        ));
                        return false;
                    }
                }
            }
        }
        let saved_id = self.switch.as_ref().expect("a run").saved_id.clone();

        // (b) Connected and carrying a mirror? That is the fence §D8
        // phase 1 asks for — `interactive` is `Connected` plus rows this
        // client can act on, which is only true once the connect's
        // `Reset` has published the session's own `tab.list`.
        if self.connected_slot_host().is_some() {
            return self.forward_begin_replay();
        }

        // (c) An attempt is out; wait for it.
        if super::servicing::attempt_alive(&self.hosts, &saved_id) {
            return false;
        }

        // (d) Dial once. The purpose is what stops the landing seeding a
        // project the replay is about to duplicate (§D12).
        if self.switch.as_ref().is_some_and(|run| run.dialed) {
            self.fail_switch("the local session could not be started");
            return false;
        }
        self.switch.as_mut().expect("a run").dialed = true;
        self.host_reconnect_for(
            &saved_id,
            crate::host_conn::RequestOrigin::Ipc,
            crate::host_conn::AttemptCause::Explicit,
            ConnectPurpose::SwitchDestination,
        );
        false
    }

    /// Phase 2 + 3: snapshot, journal, replay.
    fn forward_begin_replay(&mut self) -> bool {
        if !self.switch_gate_drained() {
            return false;
        }
        let Some(host) = self.connected_slot_host() else {
            return false;
        };
        let Some(ops) = self.hosts.ops_for(host).cloned() else {
            self.fail_switch("the local session is not accepting operations");
            return false;
        };
        // Phase 2's snapshot. A launch migration brought its own — read
        // off the retained restore descriptors before the window
        // existed, because the workspace it is moving was loaded and
        // deliberately never hydrated, so `self.projects` holds its
        // rows with **no tabs at all** (§D5).
        let migration = self
            .switch
            .as_ref()
            .and_then(|run| run.migration.as_ref())
            .cloned();
        let (snapshot, active_at) = match migration {
            Some(migration) => (migration.projects, migration.active_at),
            None => {
                let snapshot: Vec<SwitchProject> = self
                    .projects
                    .iter()
                    .map(|project| SwitchProject {
                        source: project.id,
                        name: project.name.clone(),
                        cwd: project.cwd.clone(),
                        tabs: project
                            .tabs
                            .iter()
                            .map(|tab| SwitchTab {
                                cwd: tab.cwd.clone(),
                                title: tab.title.clone(),
                                user_titled: tab.user_titled,
                            })
                            .collect(),
                    })
                    .collect();
                let (_, active_tab) = self.workspace.active();
                let active_at = self.projects.iter().enumerate().find_map(|(p, project)| {
                    project
                        .tabs
                        .iter()
                        .position(|tab| tab.id == active_tab)
                        .map(|t| (p, t))
                });
                (snapshot, active_at)
            }
        };

        let path = self.journal_path();
        let run = self.switch.as_mut().expect("a run");
        run.active_at = active_at;
        run.journal.source_snapshot = snapshot.clone();
        run.journal.phase = SwitchState::Replaying;
        if let Err(error) = write_journal(&path, &run.journal) {
            // The journal is the whole of the crash promise. Without it
            // the replay would be unattributable, so it is a refusal
            // rather than a risk taken quietly.
            self.fail_switch(&format!("could not record the switch: {error}"));
            return false;
        }
        let generation = run.generation;
        let journal = run.journal.clone();
        run.step_in_flight = true;
        self.switch_phase(SwitchState::Replaying);

        let feed = self.feed_tx.clone();
        let home = roost_engine::home_dir();
        let grid = self.current_grid();
        self.runtime_handle.spawn(async move {
            let outcome = replay_onto_slot(&ops, snapshot, &path, journal, &home, grid).await;

            feed.send(crate::engine_feed::EngineFeed::LocalBackendSwitch(
                Box::new(SwitchStepDone {
                    generation,
                    step: SwitchStep::Replayed(outcome),
                }),
            ));
        });
        false
    }

    /// Phase 4 (the commit point) and the phase-5 dispatch behind it.
    fn forward_commit(&mut self) {
        // 1. The key. **First**, before the journal's phase moves: the
        //    window between the two writes is a crash window either way,
        //    and this order makes it the *recoverable* one — a journal
        //    still reading `Replaying` over a key that says `session`
        //    rolls back, and the rollback rewrites the key. The reverse
        //    order would leave a journal claiming a commit that no key
        //    records.
        let recorded = self
            .config_writer
            .record("local-backend", LocalBackendMode::Session.as_str());
        // 2. The journal says the destination is whole. **A refusal, not
        //    a log.** The source teardown below is the one irreversible
        //    thing this sequence does, and this write is what tells the
        //    next launch not to undo the destination: a journal still
        //    reading `replaying` over an emptied source resolves as a
        //    rollback, and a rollback deletes the copy — both sides
        //    gone. So a write that fails stops here, before anything is
        //    deleted, and unwinds through the ordinary rollback: the key
        //    goes back, the copy goes, and the source is exactly as the
        //    user left it.
        //
        //    Both records ride one background step, in that order: the
        //    key write waits on `config.lock` and this is the event-loop
        //    thread. The journal is written only once the key's outcome
        //    is known, so the order above is the order on disk.
        let path = self.journal_path();
        let run = self.switch.as_mut().expect("a run");
        run.journal.phase = SwitchState::Committing;
        run.journal.created_dest_ids = run.created.iter().map(|made| made.project).collect();
        run.journal.deletable_sources = deletable_sources(&run.created);
        let journal = run.journal.clone();
        let generation = run.generation;
        run.step_in_flight = true;
        let feed = self.feed_tx.clone();
        self.runtime_handle.spawn(async move {
            let recorded = match recorded.await {
                Ok(()) => write_journal(&path, &journal)
                    .map_err(|error| format!("could not record the switch commit: {error}")),
                Err(error) => Err(format!("could not record the local backend: {error}")),
            };
            feed.send(crate::engine_feed::EngineFeed::LocalBackendSwitch(
                Box::new(SwitchStepDone {
                    generation,
                    step: SwitchStep::Committed(recorded),
                }),
            ));
        });
    }

    /// Phase 4's tail, once both records are on disk.
    fn forward_committed(&mut self) {
        // 3. The commit point.
        // The source's active pair, mapped by position — and where that
        // pair did not land (its project failed, or that very tab is the
        // one that could not start), the first tab this switch did land.
        // A switch that moved the user's work and then selected nothing
        // leaves them looking at an empty pane.
        let run = self.switch.as_mut().expect("a run");
        run.wanted = mapped_selection(&run.created, run.active_at);
        let generation = run.generation;
        let deletable = run.journal.deletable_sources.clone();
        run.kept = run.created.len() - deletable.len();
        let kept = run.kept;
        run.step_in_flight = true;
        self.local_backend = LocalBackendMode::Session;
        self.switch_phase(SwitchState::Committing);
        tracing::info!(
            migrated = deletable.len(),
            kept,
            "local-backend committed to session"
        );

        // Phase 5: the source. **Exactly, and only, what phase 3 proved
        // present at the destination** — the snapshot's own ids, filtered
        // to the projects the replay landed whole.
        //
        // Not the live project list, which would sweep up anything
        // created while the replay was out (an IPC client's
        // `project.create`, a hook's), and not the snapshot unfiltered,
        // which would delete a project whose tab failed to open over
        // there. Both are the same mistake: deleting a source for work
        // the destination does not hold.
        //
        // Its failures are logged and carried, not raised — the mode has
        // moved, and the next launch's recovery finishes whatever is
        // left (§D8b).
        let client = self.client.clone();
        let feed = self.feed_tx.clone();
        self.runtime_handle.spawn(async move {
            let left = delete_source_projects(&client, &deletable).await;
            feed.send(crate::engine_feed::EngineFeed::LocalBackendSwitch(
                Box::new(SwitchStepDone {
                    generation,
                    step: SwitchStep::SourceCleared { left },
                }),
            ));
        });
    }

    /// Phase 6: the fence, then the guard comes off.
    fn forward_fence(&mut self) -> bool {
        let slot = self
            .connected_slot_host()
            .and_then(|host| self.interactive_host_view(host));
        let Some((host, rows)) = slot.map(|view| (view.host, view.projects.clone())) else {
            return false;
        };
        let (created, wanted) = {
            let run = self.switch.as_ref().expect("a run");
            (run.created.clone(), run.wanted)
        };
        let selected = self
            .host_selection
            .filter(|selection| selection.tab.host == host)
            .map(|selection| (selection.project.project, selection.tab.tab));
        let attached_and_streaming = self
            .host_selection
            .and_then(|selection| self.host_attach.get(&selection.tab))
            .is_some_and(super::host_tab::HostAttach::streaming);
        // Selecting is part of the fence, not before it: a selection at
        // a row the mirror has not published yet is dropped by
        // `reconcile_host_selection` the moment it is made.
        if let Some((project, tab)) = wanted {
            let listed = rows
                .iter()
                .any(|row| row.id == project && row.tabs.iter().any(|row| row.id == tab));
            if listed && selected != Some((project, tab)) {
                let tab = TabKey::new(host, tab);
                self.set_host_selection(Some(super::HostSelection {
                    project: ProjectKey::new(host, project),
                    tab,
                    local_active: self.workspace.active().1,
                }));
                self.host_focus_tab(tab);
                return true;
            }
        }
        if !fence_holds(SwitchFence {
            created: &created,
            mirrored: &rows,
            selected,
            wanted,
            // §D8 phase 6 asks for "selected **and attached**", and a
            // selection is only the first half: `host_focus_tab` starts
            // an attach that can still be refused (a build mismatch, a
            // session that dropped). Releasing on the selection alone
            // reports a switch whose tab shows nothing.
            attached: attached_and_streaming,
        }) {
            return false;
        }
        let kept = self.switch.as_ref().map_or(0, |run| run.kept);
        self.finish_switch(&match kept {
            0 => "local tabs now run in a session on this machine".to_string(),
            n => format!(
                "local tabs now run in a session on this machine — {} did not copy \
                 completely and {} left where they were",
                plural(n, "project"),
                if n == 1 { "was" } else { "were" },
            ),
        });
        false
    }

    /// The forward failure path: put the key back, undo the
    /// destination, leave the source exactly as it was.
    ///
    /// The key is rewritten **unconditionally**, for the reason
    /// [`journal_recovery`] rewrites it on both arms: this path is
    /// reached from before the key write (where it is a no-op) *and*
    /// from after it (a failed commit-journal write), and a rollback
    /// that only sometimes restored the key would leave the two cases
    /// telling different stories to the next launch. A write that fails
    /// here is logged, not raised — the journal still says `replaying`,
    /// so the next launch does this again with the same answer.
    fn forward_roll_back(&mut self, why: &str) {
        // Except for §D5's launch migration, which has no key to put
        // back: it started from a key that already said `session`, so
        // the only thing to undo is the destination copy. Same reading
        // as [`journal_recovery`]'s rollback arm, and for the same
        // reason — see [`SwitchJournal::launch_migration`].
        let launch_migration = self
            .switch
            .as_ref()
            .is_some_and(|run| run.journal.launch_migration);
        let writer = (!launch_migration).then(|| self.config_writer.clone());
        // Paired with what the snapshot said each copy should hold, so
        // the deletion below can tell the copy from a project somebody
        // else has since worked in — `rollback_is_still_ours`. The
        // pairing is positional for `rollback_targets`' reason: the
        // replay walks the snapshot in order and `created` is what it
        // got through.
        let created: Vec<(i64, Option<usize>)> = self
            .switch
            .as_ref()
            .map(|run| {
                // An adopted copy this run did not manage to clear is
                // still on the destination and still nobody else's, so
                // it goes with this run's own.
                inherited_targets(&run.journal)
                    .into_iter()
                    .chain(run.created.iter().enumerate().map(|(index, made)| {
                        (
                            made.project,
                            run.journal
                                .source_snapshot
                                .get(index)
                                .map(|source| source.tabs.len()),
                        )
                    }))
                    .collect()
            })
            .unwrap_or_default();
        if created.is_empty() {
            // Nothing reached the destination, so the key restore *is*
            // the rollback — and `fail_switch`'s journal clear is the
            // one thing that must not overtake it, so the two ride one
            // ordered task. (Nothing created means nothing adopted
            // either, which is all `fail_switch` would do beyond that
            // clear — see `retire_failed_journal`.)
            let path = self.journal_path();
            match writer {
                Some(writer) => {
                    self.runtime_handle.spawn(async move {
                        put_backend_key_back(&writer, LocalBackendMode::InProcess).await;
                        clear_journal(&path);
                    });
                }
                None => clear_journal(&path),
            }
            self.fail_switch_keeping_journal(why);
            return;
        }
        let Some(ops) = self
            .switch
            .as_ref()
            .and_then(|run| self.hosts.ops(&run.saved_id))
            .cloned()
        else {
            // Nothing to delete it with. The journal stays where it is,
            // so the next launch — which will have a connection — is the
            // one that cleans it up (§D8b's "a partial destination copy
            // may remain"), and it is also what makes an unordered key
            // restore safe on this arm alone: nothing here can clear it.
            if let Some(writer) = writer {
                self.runtime_handle.spawn(async move {
                    put_backend_key_back(&writer, LocalBackendMode::InProcess).await;
                });
            }
            self.fail_switch_keeping_journal(why);
            return;
        };
        tracing::warn!(%why, projects = created.len(), "rolling the switch destination back");
        let run = self.switch.as_mut().expect("a run");
        let generation = run.generation;
        run.step_in_flight = true;
        let feed = self.feed_tx.clone();
        // The toast is set now rather than on the cleanup's completion:
        // it is the answer to what the user asked for, and the cleanup
        // is bookkeeping they did not.
        self.set_status(format!("{why} — nothing was changed"));
        self.runtime_handle.spawn(async move {
            let complete = roll_back_destination(writer, &ops, &created).await;
            feed.send(crate::engine_feed::EngineFeed::LocalBackendSwitch(
                Box::new(SwitchStepDone {
                    generation,
                    step: SwitchStep::DestCleared { complete },
                }),
            ));
        });
    }

    // ── reverse (`local:use_in_process`), No-Replay ─────────────────

    /// The journal, and nothing else: reverse copies nothing, so its
    /// only crash window is the flip itself.
    fn reverse_prepare(&mut self) -> bool {
        let path = self.journal_path();
        let run = self.switch.as_mut().expect("a run");
        if let Err(error) = write_journal(&path, &run.journal) {
            self.fail_switch(&format!("could not record the switch: {error}"));
            return false;
        }
        self.switch_phase(SwitchState::Committing);
        true
    }

    /// The commit point, and the demotion behind it.
    fn reverse_commit(&mut self) -> bool {
        let path = self.journal_path();
        let writer = self.config_writer.clone();
        let run = self.switch.as_mut().expect("a run");
        let mut journal = run.journal.clone();
        let generation = run.generation;
        run.step_in_flight = true;
        let feed = self.feed_tx.clone();
        self.runtime_handle.spawn(async move {
            let recorded = reverse_commit_record(&writer, &path, &mut journal).await;
            feed.send(crate::engine_feed::EngineFeed::LocalBackendSwitch(
                Box::new(SwitchStepDone {
                    generation,
                    step: SwitchStep::ReverseCommitted(recorded),
                }),
            ));
        });
        false
    }

    /// The commit point, once both records are on disk.
    fn reverse_committed(&mut self) {
        let run = self.switch.as_mut().expect("a run");
        // The record's own copy moved with the file it wrote.
        run.journal.phase = SwitchState::Committing;
        self.local_backend = LocalBackendMode::InProcess;
        self.switch_phase(SwitchState::CleaningUp);
        // The slot keeps its connection, its session and its projects —
        // it is simply an ordinary `LOCALHOST` band from here, which is
        // what flipping the mode makes it (`host_sidebar::sections`
        // reads `local_slot_input`). What does move is the *selection*:
        // "local tabs start fresh" means the window comes back to the
        // in-process band rather than staying on a session tab.
        self.set_host_selection(None);
        tracing::info!("local-backend committed to in-process");
    }

    /// Bring the in-process band back (§D8's "or whatever the
    /// in-process workspace still holds").
    ///
    /// **Hydrate it, never count it.** "Whatever it still holds" after a
    /// session-mode launch is project *rows* with no live tabs —
    /// `Workspace::open` loads the layout and session-mode bootstrap
    /// deliberately does not open it — so a reverse that saw a non-empty
    /// list and declared itself finished would hand back a band of empty
    /// rows. The next forward switch would then snapshot those empty tab
    /// lists and delete the originals: the retained layout gone for
    /// good, through two gestures that both reported success.
    ///
    /// [`super::hydrate_local_workspace`] is the launch's own routine —
    /// it seeds an empty workspace (§D4), restores a retained one, and
    /// is idempotent over projects that already have shells. Run
    /// **inside the guard**: an empty in-process workspace under
    /// `in-process` is exactly what §D9's exit rule closes the window
    /// for.
    fn reverse_finish(&mut self) -> bool {
        if !self.switch_gate_drained() {
            return false;
        }
        let run = self.switch.as_mut().expect("a run");
        let generation = run.generation;
        run.step_in_flight = true;
        let client = self.client.clone();
        let feed = self.feed_tx.clone();
        let grid = self.current_grid();
        self.runtime_handle.spawn(async move {
            let seeded = super::hydrate_local_workspace(&client, grid)
                .await
                .map_err(|error| error.to_string());
            feed.send(crate::engine_feed::EngineFeed::LocalBackendSwitch(
                Box::new(SwitchStepDone {
                    generation,
                    step: SwitchStep::Seeded(seeded),
                }),
            ));
        });
        false
    }

    // ── step completions ────────────────────────────────────────────

    /// Every transition goes through here, so `identify` names the phase
    /// the moment it starts rather than at the next reconcile's tail.
    fn switch_phase(&mut self, state: SwitchState) {
        let run = self.switch.as_mut().expect("a run");
        tracing::info!(
            direction = ?run.direction,
            from = ?run.state,
            to = ?state,
            "local-backend switch"
        );
        run.state = state;
        run.deadline = phase_deadline();
        self.publish_local_route();
    }

    pub(super) fn switch_admission_drained(&mut self, generation: u64) {
        match self.switch.as_mut() {
            Some(run) if run.generation == generation => run.admission_drained = true,
            _ => {
                tracing::debug!("an admission drain outlived the switch that asked for it");
                return;
            }
        }
        self.reconcile();
    }

    pub(super) fn switch_step_completed(&mut self, done: SwitchStepDone) {
        let SwitchStepDone { generation, step } = done;
        match self.switch.as_mut() {
            Some(run) if run.generation == generation => run.step_in_flight = false,
            _ => {
                tracing::debug!("a switch step outlived the switch that started it");
                return;
            }
        }
        match step {
            SwitchStep::Replayed(outcome) => {
                let run = self.switch.as_mut().expect("a run");
                run.created = outcome.created;
                // The App's copy of the journal is the one the commit
                // and the rollback write, so it takes the replay's word
                // for what is left of the adopted copy.
                run.journal.inherited_dest = outcome.inherited_left;
                match outcome.error {
                    Some(error) => self.forward_roll_back(&error),
                    None => self.forward_commit(),
                }
            }
            SwitchStep::SourceCleared { left } => {
                if left == 0 {
                    // The final delete (§D8b): the journal has nothing
                    // left to describe.
                    clear_journal(&self.journal_path());
                } else {
                    tracing::warn!(
                        left,
                        "some in-process projects survived the switch; \
                         the next launch finishes it"
                    );
                }
                self.switch_phase(SwitchState::CleaningUp);
            }
            SwitchStep::DestCleared { complete } => {
                match complete {
                    true => clear_journal(&self.journal_path()),
                    // Left deliberately: the next launch rolls back what
                    // this one could not.
                    false => tracing::warn!(
                        "the switch destination was not fully rolled back; \
                         the next launch finishes it"
                    ),
                }
                self.end_switch();
            }
            SwitchStep::Committed(recorded) => match recorded {
                Ok(()) => self.forward_committed(),
                Err(why) => self.forward_roll_back(&why),
            },
            SwitchStep::ReverseCommitted(recorded) => match recorded {
                Ok(()) => self.reverse_committed(),
                // The journal is the only record of an adopted copy
                // (§D8b), so it stays: the next launch resolves what is
                // on disk — an uncommitted reverse — and deletes that
                // copy.
                Err(why) => self.fail_switch_keeping_journal(&why),
            },
            SwitchStep::Seeded(result) => {
                if let Err(error) = result {
                    // The mode has already moved, so this is a band that
                    // came back short rather than a failed switch.
                    tracing::warn!(%error, "restoring the in-process band after a switch failed");
                    self.set_status(format!("local tabs run in Roost again, but: {error}"));
                    self.finish_switch_quietly();
                    return;
                }
                self.finish_switch("local tabs run in Roost again");
            }
        }
        self.reconcile();
    }

    // ── endings ─────────────────────────────────────────────────────

    /// A failure before the commit point: say so, undo this run's own
    /// registry addition, and drop the journal — unless the run adopted
    /// an earlier switch's destination copy, which
    /// [`retire_failed_journal`] is what decides.
    fn fail_switch(&mut self, why: &str) {
        let path = self.journal_path();
        let adopted = match self.switch.as_ref() {
            Some(run) => retire_failed_journal(&path, &run.journal),
            // No run means no journal of ours to reason about.
            None => {
                clear_journal(&path);
                Vec::new()
            }
        };
        if !adopted.is_empty() {
            tracing::info!(
                projects = adopted.len(),
                "a refused switch hands its adopted copy back to the next one"
            );
            // Back where `arm_switch` took it from, so the next run in
            // this process adopts it exactly as a launch would.
            self.pending_dest_cleanup = adopted;
        }
        self.fail_switch_keeping_journal(why);
    }

    fn fail_switch_keeping_journal(&mut self, why: &str) {
        tracing::warn!(%why, "local-backend switch refused");
        self.set_status(format!("{why} — nothing was changed"));
        let added = self
            .switch
            .as_ref()
            .filter(|run| run.added_slot)
            .map(|run| run.saved_id.clone());
        self.end_switch();
        if let Some(saved_id) = added {
            // Only what this run added. A daemon it *started* is left
            // running — nothing here stops a session, and the confirm
            // says so.
            if let Err(error) = self.host_remove_requested(&saved_id) {
                tracing::warn!(%error, host = %saved_id, "could not un-save the switch's slot");
            }
        }
    }

    fn finish_switch(&mut self, said: &str) {
        self.set_status(said.to_string());
        self.finish_switch_quietly();
    }

    fn finish_switch_quietly(&mut self) {
        tracing::info!(mode = %self.local_backend, "local-backend switch finished");
        self.end_switch();
    }

    /// Drop the run and republish, which is what re-arms the exit rule,
    /// the auto-remove and both verbs.
    fn end_switch(&mut self) {
        if let Some(drain) = self.switch.take().and_then(|run| run.drain) {
            drain.abort();
        }
        self.publish_local_route();
    }
}

/// Plan 063 §D8 phase 3, on the engine runtime.
///
/// The journal is rewritten after **each** project commits rather than
/// at the end, because the ids it carries are what a rollback deletes
/// and a crash mid-replay is exactly the case a rollback exists for.
async fn replay_onto_slot(
    ops: &crate::host_conn::HostOps,
    snapshot: Vec<SwitchProject>,
    path: &Path,
    mut journal: SwitchJournal,
    home: &str,
    grid: (u16, u16),
) -> ReplayOutcome {
    use roost_ipc::messages::{ops as wire, ProjectCreateResult, TabOpenResult};
    let mut created: Vec<CreatedProject> = Vec::with_capacity(snapshot.len());

    // **An adopted copy goes before anything is made.** It is a copy of
    // some of this very layout, made by a switch that crashed, and
    // replaying over it is how a user ends up with everything twice —
    // the duplication the owner's No-Replay decision exists to avoid
    // elsewhere. A failure to clear it is therefore a failure of the
    // whole replay: the rollback below puts the key back and the journal
    // keeps naming the copy for a launch that can reach it.
    let inherited = std::mem::take(&mut journal.inherited_dest);
    if !inherited.is_empty() {
        let targets: Vec<(i64, Option<usize>)> = inherited
            .iter()
            .copied()
            .map(InheritedDest::target)
            .collect();
        if !delete_dest_projects(ops, &targets).await {
            return ReplayOutcome {
                created,
                inherited_left: inherited,
                error: Some("an abandoned copy from an earlier switch could not be removed".into()),
            };
        }
        if let Err(error) = write_journal(path, &journal) {
            tracing::warn!(%error, "could not record an adopted copy's removal");
        }
    }

    for project in &snapshot {
        let mut complete = true;
        let made: Result<ProjectCreateResult, String> = super::host_call(
            ops,
            wire::PROJECT_CREATE,
            // Explicit, never empty: an empty name is `Untitled N` from
            // the *destination's* count, which would rename an
            // `Untitled 2` that moved onto an empty session (§D4).
            serde_json::json!({ "name": project.name, "cwd": project.cwd }),
        )
        .await;
        let made = match made {
            Ok(made) => made.project,
            // A project-level failure aborts: the tabs under it have
            // nowhere to go, and half a layout is not a layout.
            Err(error) => {
                return ReplayOutcome {
                    created,
                    inherited_left: Vec::new(),
                    error: Some(error),
                }
            }
        };
        journal.created_dest_ids.push(made.id);
        if let Err(error) = write_journal(path, &journal) {
            // **If we cannot record it, we cannot recover it.** The id is
            // in memory and not on disk, so a crash from here on leaves a
            // real destination project that the next launch's rollback —
            // which reads its targets off the journal — can neither name
            // nor delete. So the replay stops while this run can still
            // roll it back, and the copy goes into `created` first:
            // that list is what [`super::App::forward_roll_back`]
            // deletes.
            created.push(CreatedProject {
                source: project.source,
                project: made.id,
                tabs: Vec::new(),
                complete: false,
            });
            return ReplayOutcome {
                created,
                inherited_left: Vec::new(),
                error: Some(format!("could not record a replayed project: {error}")),
            };
        }
        let mut tabs: Vec<CreatedTab> = Vec::with_capacity(project.tabs.len());
        for (position, tab) in project.tabs.iter().enumerate() {
            let cwd = replay_cwd(&tab.cwd, &made.cwd, home);
            let opened: Result<TabOpenResult, String> = super::host_call(
                ops,
                wire::TAB_OPEN,
                super::host_tab_open_params(made.id, &cwd, &tab.title, &[], None, grid),
            )
            .await;
            let opened = match opened {
                Ok(opened) => opened.tab,
                // One tab must not cost the rest of the layout — but it
                // must cost this project its source deletion, or the
                // switch reports success over a tab that is now nowhere.
                Err(error) => {
                    tracing::warn!(project = %project.name, cwd = %cwd, %error, "replaying a tab failed");
                    complete = false;
                    continue;
                }
            };
            tabs.push(CreatedTab {
                source: position,
                tab: opened.id,
            });
            // Only a lock the user set: a title the shell wrote is the
            // shell's to write again over there.
            if tab.user_titled && !tab.title.is_empty() {
                if let Err(error) = super::host_call::<serde_json::Value>(
                    ops,
                    wire::TAB_SET_TITLE,
                    serde_json::json!({ "tab_id": opened.id.to_string(), "title": tab.title }),
                )
                .await
                {
                    // Only a title, but the same rule: what phase 5 may
                    // delete is what phase 3 *proved* present, and a
                    // title lock is part of what the source held.
                    tracing::warn!(tab = opened.id, %error, "replaying a tab title failed");
                    complete = false;
                }
            }
        }
        created.push(CreatedProject {
            source: project.source,
            project: made.id,
            tabs,
            complete,
        });
    }
    ReplayOutcome {
        created,
        inherited_left: Vec::new(),
        error: None,
    }
}

/// Where a replayed tab actually opens (plan 063 §D8 phase 3).
///
/// A directory that has been deleted since the tab was opened would fail
/// the whole `tab.open`, and losing a tab because its cwd went away is a
/// worse answer than opening it one level out. The slot is *this*
/// machine, so the check is a real one — this is the only replay
/// destination for which that is true, and the only one §D8 asks it of.
fn replay_cwd(tab: &str, project: &str, home: &str) -> String {
    roost_engine::application::usable_cwd_or(tab, project, home)
}

/// Phase 5. Answers how many are left, because that is what decides
/// whether the journal may be deleted.
async fn delete_source_projects(client: &roost_engine::LocalClient, ids: &[i64]) -> usize {
    let mut left = 0;
    for id in ids {
        if let Err(error) = client.delete_project(*id).await {
            tracing::warn!(project_id = id, %error, "deleting a migrated project failed");
            left += 1;
        }
    }
    left
}

/// The forward rollback's work on disk, in the one order that leaves it
/// recoverable: the key first, then the destination copy.
///
/// **One task, not two.** The journal is the only record that tells the
/// next launch to put this key back, and it is cleared the moment this
/// returns `true` — so a restore running *beside* the deletion can still
/// be in flight when that journal goes. A crash in that window leaves
/// `local-backend = session` with nothing left to describe it, on a run
/// that already told the user nothing was changed. Awaiting the key here
/// is what makes "the journal outlives the key write" true rather than
/// usually true.
///
/// `writer` is `None` for §D5's launch migration, which has no key to
/// put back — see [`super::App::forward_roll_back`].
///
/// Free of the app so the ordering can be driven directly.
async fn roll_back_destination(
    writer: Option<ConfigWriter>,
    ops: &crate::host_conn::HostOps,
    created: &[(i64, Option<usize>)],
) -> bool {
    if let Some(writer) = writer {
        put_backend_key_back(&writer, LocalBackendMode::InProcess).await;
    }
    delete_dest_projects(ops, created).await
}

/// The rollback's other half. `true` when the destination holds none of
/// the copy any more.
async fn delete_dest_projects(
    ops: &crate::host_conn::HostOps,
    targets: &[(i64, Option<usize>)],
) -> bool {
    use roost_ipc::messages::{ops as wire, TabListResult};

    // What is there now, asked once. A failure to ask is a failure to
    // roll back: without it nothing below can tell the copy from
    // somebody else's work, and deleting on the strength of a remembered
    // id is exactly what `rollback_is_still_ours` exists to stop.
    let listed: Result<TabListResult, String> =
        super::host_call(ops, wire::TAB_LIST, serde_json::json!({})).await;
    let found = match listed {
        Ok(listed) => tab_counts(&listed.projects),
        Err(error) => {
            tracing::warn!(%error, "could not read the switch destination back to roll it back");
            return false;
        }
    };
    let mut complete = true;
    for (id, expected) in targets {
        let Some(found) = found.get(id).copied() else {
            // Already gone is the state the rollback wanted.
            continue;
        };
        if !rollback_is_still_ours(*expected, found) {
            // **Disowned, not pending.** This client will never delete
            // it, so counting it as unfinished would leave a journal
            // that every later launch re-reads and re-declines.
            tracing::warn!(
                project_id = id,
                found,
                ?expected,
                "leaving a replayed project alone: it has gained tabs since the switch made it"
            );
            continue;
        }
        if let Err(error) = super::host_call::<serde_json::Value>(
            ops,
            wire::PROJECT_DELETE,
            serde_json::json!({ "project_id": id.to_string() }),
        )
        .await
        {
            tracing::warn!(project_id = id, %error, "rolling back a replayed project failed");
            complete = false;
        }
    }
    complete
}

/// A `tab.list` reply as "how many tabs does each project hold".
pub(crate) fn tab_counts(projects: &[Project]) -> std::collections::HashMap<i64, usize> {
    projects
        .iter()
        .map(|project| (project.id, project.tabs.len()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::paths::BundleProfile;
    use std::collections::HashMap;

    #[test]
    fn the_session_snapshot_carries_the_profile_socket_and_the_slot_selection() {
        let route = route_snapshot(
            LocalBackendMode::Session,
            SlotSelection {
                host: Some(2),
                active: Some((4, 9)),
            },
            SwitchState::Idle,
        );
        assert_eq!(route.mode, LocalBackendMode::Session);
        assert_eq!(route.slot_host, Some(2));
        assert_eq!(route.slot_active, Some((4, 9)));
        assert_eq!(route.switch, None, "an idle UI publishes no phase");
        // And the phase rides through, which is what lets `identify`
        // say why a mutation was refused (§D8a).
        assert_eq!(
            route_snapshot(
                LocalBackendMode::Session,
                SlotSelection::default(),
                SwitchState::Replaying
            )
            .switch,
            Some("replaying")
        );
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

    fn showing(host: HostId, project: i64, tab: i64) -> super::super::HostSelection {
        super::super::HostSelection {
            project: ProjectKey::new(host, project),
            tab: TabKey::new(host, tab),
            local_active: 0,
        }
    }

    /// §D1's `identify.active_*` under `session`: what a bare id means
    /// when no `--tab` was given.
    #[test]
    fn the_slot_selection_is_the_pair_showing_on_the_slot() {
        let slot = HostId::new(2);
        assert_eq!(
            slot_selection(Some(slot), Some(showing(slot, 4, 9))),
            SlotSelection {
                host: Some(2),
                active: Some((4, 9)),
            }
        );
    }

    /// The filter, one reason at a time. Each of these would otherwise
    /// answer `roostctl tab write` with ids on the wrong machine — or on
    /// an incarnation that has renumbered since.
    #[test]
    fn a_selection_that_is_not_the_slots_is_not_a_slot_selection() {
        let slot = HostId::new(2);
        let elsewhere = HostId::new(7);
        assert_eq!(
            slot_selection(Some(slot), Some(showing(elsewhere, 4, 9))).active,
            None,
            "a remote host's tab is not the local one"
        );
        assert_eq!(
            slot_selection(Some(slot), None).active,
            None,
            "nothing selected"
        );
        // A slot that is not connected has no incarnation to name, so a
        // stale selection cannot be reported against it either.
        let down = slot_selection(None, Some(showing(slot, 4, 9)));
        assert_eq!(down.host, None);
        assert_eq!(down.active, None);
    }

    /// In-process has no slot at all, so a selection handed in from a
    /// previous session mode cannot survive the switch back.
    #[test]
    fn the_in_process_snapshot_has_no_slot() {
        let route = route_snapshot(
            LocalBackendMode::InProcess,
            SlotSelection {
                host: Some(2),
                active: Some((4, 9)),
            },
            SwitchState::Idle,
        );
        assert_eq!(route.mode, LocalBackendMode::InProcess);
        assert_eq!(route.slot_socket, None);
        assert_eq!(route.slot_host, None);
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

#[cfg(test)]
mod switch_tests {
    use super::super::SPAWN_GRID;
    use super::*;
    use roost_ipc::messages::{Tab, TabState};
    use std::collections::HashMap;

    fn journal(phase: SwitchState, direction: SwitchDirection, created: &[i64]) -> SwitchJournal {
        SwitchJournal {
            from_mode: direction.source(),
            to_mode: direction.destination(),
            phase,
            // What a commit records: the sources the replay landed
            // whole, which is what the recovery is allowed to delete.
            deletable_sources: vec![3],
            source_snapshot: vec![SwitchProject {
                source: 3,
                name: "one".into(),
                cwd: "/tmp".into(),
                tabs: vec![SwitchTab {
                    cwd: "/tmp".into(),
                    title: "shell".into(),
                    user_titled: true,
                }],
            }],
            created_dest_ids: created.to_vec(),
            launch_migration: false,
            inherited_dest: Vec::new(),
        }
    }

    /// The phase names are a wire contract (`identify`), and `Idle` is
    /// spelled as absence rather than as a name.
    #[test]
    fn only_a_switch_in_flight_has_a_phase_to_report() {
        assert_eq!(SwitchState::Idle.wire(), None);
        assert!(!SwitchState::Idle.in_flight());
        for (state, name) in [
            (SwitchState::Preparing, "preparing"),
            (SwitchState::Replaying, "replaying"),
            (SwitchState::Committing, "committing"),
            (SwitchState::CleaningUp, "cleaning-up"),
        ] {
            assert_eq!(state.wire(), Some(name), "{state:?}");
            assert!(state.in_flight(), "{state:?}");
        }
    }

    /// **The one question a journal is asked** (plan 063 §D8b): is the
    /// commit point behind this phase?
    ///
    /// Asserted per variant rather than as a range, because the answer
    /// decides between finishing a migration and deleting the copy it
    /// made — and the two phases either side of the line are adjacent.
    #[test]
    fn the_commit_point_splits_the_phases_at_committing() {
        assert!(!SwitchState::Idle.committed());
        assert!(!SwitchState::Preparing.committed());
        assert!(!SwitchState::Replaying.committed());
        assert!(SwitchState::Committing.committed());
        assert!(SwitchState::CleaningUp.committed());
    }

    /// A journal survives a process, so its encoding is as load-bearing
    /// as the decision over it. Round-tripped through JSON — including
    /// the phase, whose kebab spelling is what a half-written file is
    /// read back by.
    #[test]
    fn a_journal_round_trips_through_the_file_it_lives_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        assert_eq!(path.file_name().unwrap(), "switch-journal.json");
        assert_eq!(read_journal(&path), None, "nothing written yet");

        let written = journal(SwitchState::Replaying, SwitchDirection::ToSession, &[4, 9]);
        write_journal(&path, &written).unwrap();
        assert_eq!(read_journal(&path).as_ref(), Some(&written));
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["from_mode"], "in-process");
        assert_eq!(raw["to_mode"], "session");

        // **The phase on disk is spelled exactly as `identify` reports
        // it**, for every phase and not just the one above. Two readers
        // depend on that being one string: the recovery decodes this
        // file, and a test (or an operator) writing a journal by hand
        // has only the reported name to write. A serde rename that
        // drifted from `wire()` would leave `cleaning-up` undecodable
        // while every other phase kept working.
        for phase in [
            SwitchState::Preparing,
            SwitchState::Replaying,
            SwitchState::Committing,
            SwitchState::CleaningUp,
        ] {
            let mut moved = written.clone();
            moved.phase = phase;
            write_journal(&path, &moved).unwrap();
            let raw: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(raw["phase"], phase.wire().unwrap(), "{phase:?}");
            assert_eq!(read_journal(&path).map(|back| back.phase), Some(phase));
        }
        write_journal(&path, &written).unwrap();

        clear_journal(&path);
        assert_eq!(read_journal(&path), None);
        // Clearing what is not there is the state the caller wanted.
        clear_journal(&path);
    }

    /// A file this build cannot read is left where it is and ignored —
    /// deleting it would destroy the only record of whatever wrote it.
    #[test]
    fn an_undecodable_journal_is_ignored_rather_than_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        std::fs::write(&path, b"{ this is not a journal").unwrap();
        assert_eq!(read_journal(&path), None);
        assert!(path.exists(), "the file the launch could not read stays");
    }

    /// Plan 063 §D8b's three resolutions, one phase at a time.
    ///
    /// The asymmetry is the subject: **the same journal** resolves to
    /// "finish, delete the source" one phase later than it resolves to
    /// "roll back, delete the destination". A recovery keyed on
    /// anything but the phase — the mode on disk, the presence of
    /// created ids — would answer the same for both sides of that line,
    /// and one of the two answers destroys the user's layout.
    #[test]
    fn the_phase_alone_decides_which_way_an_interrupted_switch_resolves() {
        use LocalBackendMode::{InProcess, Session};

        for phase in [SwitchState::Preparing, SwitchState::Replaying] {
            assert_eq!(
                journal_recovery(&journal(phase, SwitchDirection::ToSession, &[4, 9])),
                JournalRecovery::RollBack {
                    mode: InProcess,
                    delete_dest: vec![(4, Some(1)), (9, None)],
                },
                "{phase:?}: the destination is partial, so the source wins"
            );
        }
        for phase in [SwitchState::Committing, SwitchState::CleaningUp] {
            assert_eq!(
                journal_recovery(&journal(phase, SwitchDirection::ToSession, &[4, 9])),
                JournalRecovery::Finish {
                    mode: Session,
                    delete_dest: Vec::new(),
                    // The journal's own list, not "every project": the
                    // running switch kept whatever the replay did not
                    // land whole, and the recovery has to keep it too.
                    delete_source: vec![3],
                },
                "{phase:?}: the destination is whole and the key says so"
            );
        }

        // Reverse is symmetric in the mode and **not** in the teardown:
        // No-Replay copies nothing, so finishing it must not delete the
        // in-process layout it is coming back to.
        assert_eq!(
            journal_recovery(&journal(
                SwitchState::Preparing,
                SwitchDirection::ToInProcess,
                &[]
            )),
            JournalRecovery::RollBack {
                mode: Session,
                delete_dest: Vec::new(),
            }
        );
        assert_eq!(
            journal_recovery(&journal(
                SwitchState::Committing,
                SwitchDirection::ToInProcess,
                &[]
            )),
            JournalRecovery::Finish {
                mode: InProcess,
                delete_dest: Vec::new(),
                delete_source: vec![],
            }
        );
    }

    /// **The resumed teardown keeps what the running one kept.**
    ///
    /// The journal's snapshot names two source projects and its
    /// `deletable_sources` names one: the other is a project whose tab
    /// never landed, which is exactly why the switch left it alone. A
    /// recovery that finished the job by sweeping the workspace — or by
    /// reading the snapshot — would delete it, and the destination does
    /// not hold it.
    #[test]
    fn a_resumed_source_teardown_keeps_what_the_switch_kept() {
        let mut committed = journal(SwitchState::Committing, SwitchDirection::ToSession, &[4, 9]);
        committed.source_snapshot = vec![source(3, &[("a", false)]), source(5, &[("b", false)])];
        committed.deletable_sources = vec![3];
        assert_eq!(
            journal_recovery(&committed),
            JournalRecovery::Finish {
                mode: LocalBackendMode::Session,
                delete_dest: Vec::new(),
                delete_source: vec![3],
            },
            "the snapshot has two; only one is the recovery's to delete"
        );
    }

    /// Plan 063 §D5's launch migration rolls back **without moving the
    /// key**.
    ///
    /// The asymmetry against the verb's own rollback is the subject. A
    /// user's forward switch began from `in-process` and a failure owes
    /// them that back. A launch migration began from a key that already
    /// said `session` — it is the *reason* it ran — so the copy goes and
    /// the mode stays, and the next launch finds the same populated
    /// workspace and tries again. Writing `in-process` here would
    /// silently un-edit the key the user set, and the launch after would
    /// come up on a backend nobody chose.
    ///
    /// Past the commit point there is no asymmetry to have: the
    /// destination is whole and both roads lead to `session`.
    #[test]
    fn a_launch_migration_rolls_its_copy_back_but_never_its_key() {
        for phase in [SwitchState::Preparing, SwitchState::Replaying] {
            let verb = journal(phase, SwitchDirection::ToSession, &[4, 9]);
            let mut launch = verb.clone();
            launch.launch_migration = true;
            assert_eq!(
                journal_recovery(&verb),
                JournalRecovery::RollBack {
                    mode: LocalBackendMode::InProcess,
                    delete_dest: vec![(4, Some(1)), (9, None)],
                },
                "{phase:?}: a verb the user pressed owes them the backend they were on"
            );
            assert_eq!(
                journal_recovery(&launch),
                JournalRecovery::RollBack {
                    mode: LocalBackendMode::Session,
                    delete_dest: vec![(4, Some(1)), (9, None)],
                },
                "{phase:?}: a launch migration has no backend to go back to"
            );
        }
        let mut committed = journal(SwitchState::Committing, SwitchDirection::ToSession, &[4, 9]);
        committed.launch_migration = true;
        assert_eq!(
            journal_recovery(&committed),
            JournalRecovery::Finish {
                mode: LocalBackendMode::Session,
                delete_dest: Vec::new(),
                delete_source: vec![3],
            },
            "past the commit point the two are the same switch"
        );
    }

    /// **The reverse's commit point refuses a journal it cannot write**
    /// (plan 063 §D8).
    ///
    /// The forward commit's rule, and the same consequence read the
    /// other way round: the flip behind this write makes the run report
    /// success, while a journal still on disk at `preparing` is exactly
    /// what the next launch resolves as an *uncommitted* reverse — it
    /// puts `session` back, undoing a completed switch nobody was told
    /// had failed. So it stops, and the key goes back with it.
    #[tokio::test]
    async fn a_reverse_commit_that_cannot_record_itself_refuses_and_puts_the_key_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.conf");
        roost_ui_model::config::set_key(&config, "local-backend", "session").unwrap();
        let (feed, _feed_rx) = crate::engine_feed::channel();
        let writer = ConfigWriter::spawn(
            &tokio::runtime::Handle::current(),
            Some(config.clone()),
            feed,
        );
        let mut reverse = SwitchJournal::new(SwitchDirection::ToInProcess, Vec::new());
        reverse.inherited_dest = vec![InheritedDest {
            project: 7,
            tabs: Some(2),
        }];

        // The control first: with a writable journal this is the commit,
        // and it moves both.
        let ok = journal_path(dir.path());
        assert_eq!(
            reverse_commit_record(&writer, &ok, &mut reverse.clone()).await,
            Ok(())
        );
        assert_eq!(backend_key(&config), Some("in-process".to_string()));
        assert_eq!(
            read_journal(&ok).map(|written| written.phase),
            Some(SwitchState::Committing)
        );

        roost_ui_model::config::set_key(&config, "local-backend", "session").unwrap();
        let blocked = an_unwritable_journal_path(dir.path());
        let error = reverse_commit_record(&writer, &blocked, &mut reverse)
            .await
            .expect_err("the commit journal could not be written");
        assert!(
            error.contains("could not record the switch commit"),
            "{error}"
        );
        assert_eq!(
            backend_key(&config),
            Some("session".to_string()),
            "the key goes back: the run stopped before the commit point, not after it"
        );
        assert_eq!(
            reverse.phase,
            SwitchState::Preparing,
            "and the run's own copy still says what is on disk, \
             so the adopted copy is still named by an uncommitted journal"
        );
    }

    /// **A forward rollback puts the key back before it reaches the
    /// destination** (plan 063 §D8b).
    ///
    /// The ordering is not cosmetic: the journal is cleared the moment
    /// the destination cleanup reports itself complete, so a key restore
    /// running *beside* that cleanup can still be queued when the record
    /// that would redo it is gone — and the next launch comes up on
    /// `session` after a run that said nothing was changed. The
    /// stand-in below answers nothing until it has read the key, so
    /// "before" is observed rather than timed.
    #[tokio::test]
    async fn a_forward_rollback_restores_the_key_before_it_clears_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.conf");
        roost_ui_model::config::set_key(&config, "local-backend", "session").unwrap();
        let (feed, _feed_rx) = crate::engine_feed::channel();
        let writer = ConfigWriter::spawn(
            &tokio::runtime::Handle::current(),
            Some(config.clone()),
            feed,
        );

        let (ops, worker, seen) = a_slot_reading_the_key(&config);
        assert!(roll_back_destination(Some(writer.clone()), &ops, &[(101, Some(1))]).await);
        drop(ops);
        worker.await.unwrap();
        assert_eq!(
            seen.lock().unwrap().first().cloned(),
            Some(Some("in-process".to_string())),
            "the destination cleanup started while the key still said session"
        );
        assert_eq!(backend_key(&config), Some("in-process".to_string()));

        // §D5's launch migration has no key to put back, and the copy
        // still goes.
        roost_ui_model::config::set_key(&config, "local-backend", "session").unwrap();
        let (ops, worker, seen) = a_slot_reading_the_key(&config);
        assert!(roll_back_destination(None, &ops, &[(101, Some(1))]).await);
        drop(ops);
        worker.await.unwrap();
        assert_eq!(
            backend_key(&config),
            Some("session".to_string()),
            "a launch migration started from `session`; there is no key to undo"
        );
        assert!(
            !seen.lock().unwrap().is_empty(),
            "the copy was never cleared"
        );
    }

    /// A stand-in session that records what `local-backend` said on disk
    /// as each op reached it — enough of one to answer the rollback's
    /// `tab.list` and `project.delete`.
    #[allow(clippy::type_complexity)]
    fn a_slot_reading_the_key(
        config: &Path,
    ) -> (
        crate::host_conn::HostOps,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    ) {
        use roost_ipc::messages::ops as wire;

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (ops, mut rx) = crate::host_conn::HostOps::channel();
        let config = config.to_path_buf();
        let recorded = seen.clone();
        let worker = tokio::spawn(async move {
            while let Some(intent) = rx.recv().await {
                recorded.lock().unwrap().push(backend_key(&config));
                let answer = match intent.op.as_ref() {
                    wire::TAB_LIST => serde_json::json!({ "projects": [row(101, &[7])] }),
                    _ => serde_json::json!({}),
                };
                intent.answer(Ok(answer));
            }
        });
        (ops, worker, seen)
    }

    /// **A refused run never drops the journal that names an adopted
    /// copy** (plan 063 §D8b).
    ///
    /// The third of this class in this file, and the reason it is a
    /// property of the ending rather than a check at nine call sites:
    /// eight of them can be reached with the list populated. The file is
    /// the only record a *launch* can read, and the returned list is the
    /// only record this *process* can re-adopt, so a failure has to
    /// leave both.
    #[test]
    fn a_refused_run_keeps_the_journal_naming_an_adopted_copy_and_hands_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        let mut journal = SwitchJournal::new(SwitchDirection::ToInProcess, Vec::new());

        // A run that adopted nothing: the file is its own and goes with
        // it, exactly as before.
        write_journal(&path, &journal).unwrap();
        assert!(retire_failed_journal(&path, &journal).is_empty());
        assert!(
            read_journal(&path).is_none(),
            "a run with nothing adopted still takes its journal with it"
        );

        // A run that adopted one: the file stays, and the list comes
        // back for the next `arm_switch` to re-adopt.
        journal.inherited_dest = vec![InheritedDest {
            project: 7,
            tabs: Some(2),
        }];
        write_journal(&path, &journal).unwrap();
        assert_eq!(retire_failed_journal(&path, &journal), vec![(7, Some(2))]);
        assert_eq!(
            read_journal(&path).map(|kept| kept.inherited_dest),
            Some(journal.inherited_dest.clone()),
            "the copy has no other record: deleting this file orphans it for good"
        );
    }

    /// What the `local-backend` line in a config file says, if any.
    fn backend_key(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("local-backend = "))
            .map(str::to_string)
    }

    /// A journal naming no switch at all — a hand-edited file, or one
    /// from a build that spelled the modes differently — is not acted
    /// on. Both same-mode pairs, because either would otherwise pick a
    /// direction out of a tie.
    #[test]
    fn a_journal_that_names_no_switch_is_ignored() {
        for mode in [LocalBackendMode::InProcess, LocalBackendMode::Session] {
            let mut bad = journal(SwitchState::Committing, SwitchDirection::ToSession, &[1]);
            bad.from_mode = mode;
            bad.to_mode = mode;
            assert_eq!(journal_recovery(&bad), JournalRecovery::Ignore, "{mode}");
        }
    }

    fn tab(id: i64) -> Tab {
        Tab {
            id,
            project_id: 0,
            title: String::new(),
            cwd: "/tmp".into(),
            state: TabState::Idle,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position: 0,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: Default::default(),
            agent_lifecycle: Default::default(),
            ownership: None,
        }
    }

    /// A bare project row, as a `project.create` reply carries one.
    fn made_project(id: i64) -> Project {
        row(id, &[])
    }

    fn row(id: i64, tabs: &[i64]) -> Project {
        Project {
            id,
            name: format!("p{id}"),
            cwd: "/tmp".into(),
            position: 0,
            created_at: 0,
            tabs: tabs.iter().map(|tab_id| tab(*tab_id)).collect(),
        }
    }

    /// Plan 063 §D8's phase 6, one clause at a time from a baseline that
    /// holds.
    ///
    /// The baseline is the state the *replies* alone would already
    /// describe — every project and tab created — and each row below
    /// removes exactly one thing the mirror has not caught up on yet.
    /// That is what the fence is: the difference between what the
    /// control replies said and what the subscription has published.
    /// A created tab list, from the destination ids in source order.
    fn landed(tabs: &[i64]) -> Vec<CreatedTab> {
        tabs.iter()
            .enumerate()
            .map(|(source, tab)| CreatedTab { source, tab: *tab })
            .collect()
    }

    /// **A rollback may not delete a project somebody else has worked
    /// in** (plan 063 §D8b).
    ///
    /// The ids in a journal were minted by a run that is gone, and a
    /// session serves every client at once. If a tab was opened in the
    /// abandoned copy since, `project.delete` would cascade through it.
    /// So the copy is recognised by what it should hold, not by the fact
    /// that this client once made it.
    ///
    /// The counts either side of the line are what matter: **fewer** is
    /// an ordinary crash mid-project and still ours; **more** can only
    /// have come from somewhere else, because a `SwitchDestination`
    /// connect withholds the seed and nothing else on that path opens a
    /// tab.
    #[test]
    fn a_rollback_disowns_a_destination_project_that_has_gained_tabs() {
        // What the replay was going to leave: two tabs.
        assert!(rollback_is_still_ours(Some(2), 2), "exactly what it made");
        assert!(rollback_is_still_ours(Some(2), 1), "a crash mid-project");
        assert!(rollback_is_still_ours(Some(2), 0), "a crash before any tab");
        assert!(
            !rollback_is_still_ours(Some(2), 3),
            "somebody opened a tab in the copy and is working there"
        );
        assert!(rollback_is_still_ours(Some(0), 0));
        assert!(!rollback_is_still_ours(Some(0), 1));
        // A journal that records no source vouches for nothing, so it
        // deletes nothing. Every journal a real replay writes has one.
        assert!(!rollback_is_still_ours(None, 0));
        assert!(!rollback_is_still_ours(None, 7));
    }

    /// The pairing behind it: `created_dest_ids[i]` is the copy of
    /// `source_snapshot[i]`, because the replay appends the id the
    /// moment `project.create` answers — before it opens a single tab.
    /// So a journal shorter in one list than the other is a crash point,
    /// and the ids past the snapshot have no expectation at all.
    #[test]
    fn a_rollback_target_is_paired_with_the_source_it_was_a_copy_of() {
        let mut written = journal(SwitchState::Replaying, SwitchDirection::ToSession, &[4]);
        written.source_snapshot = vec![
            source(11, &[("a", false), ("b", false)]),
            source(22, &[("c", false)]),
        ];
        assert_eq!(rollback_targets(&written), vec![(4, Some(2))]);

        written.created_dest_ids = vec![4, 9, 13];
        assert_eq!(
            rollback_targets(&written),
            vec![(4, Some(2)), (9, Some(1)), (13, None)],
            "the third was created after the snapshot ran out, so nothing vouches for it"
        );
    }

    /// **An adopted copy outlives the journal that recorded it** (plan
    /// 063 §D8b).
    ///
    /// Starting a switch replaces the journal file. Without
    /// `inherited_dest` the copy an earlier, unresolved rollback left
    /// behind would be erased from the only record of it — orphaned for
    /// good — and the replay would then make a second copy of
    /// everything that record described.
    ///
    /// Both arms answer for it, and they answer differently about the
    /// rest: a rollback deletes the adopted copy **and** this run's own,
    /// while a commit deletes only the adopted one — this run's copy is
    /// the work it just committed.
    #[test]
    fn an_adopted_copy_is_deleted_by_both_recovery_arms_and_this_runs_is_not() {
        let mut adopted = journal(SwitchState::Replaying, SwitchDirection::ToSession, &[4]);
        adopted.inherited_dest = vec![
            InheritedDest {
                project: 90,
                tabs: Some(2),
            },
            InheritedDest {
                project: 91,
                tabs: None,
            },
        ];
        assert_eq!(
            journal_recovery(&adopted),
            JournalRecovery::RollBack {
                mode: LocalBackendMode::InProcess,
                delete_dest: vec![(90, Some(2)), (91, None), (4, Some(1))],
            },
            "before the commit point both copies go"
        );

        // A reverse is the arm that can commit while still carrying one:
        // it copies nothing, so it never runs the replay that clears the
        // list.
        let mut committed = journal(SwitchState::Committing, SwitchDirection::ToInProcess, &[]);
        committed.inherited_dest = adopted.inherited_dest.clone();
        assert_eq!(
            journal_recovery(&committed),
            JournalRecovery::Finish {
                mode: LocalBackendMode::InProcess,
                delete_dest: vec![(90, Some(2)), (91, None)],
                delete_source: Vec::new(),
            }
        );

        // And a forward that committed has already cleared it, so the
        // ordinary case stays exactly as it was: nothing to delete on
        // the destination, the source teardown to finish.
        let plain = journal(SwitchState::Committing, SwitchDirection::ToSession, &[4]);
        assert_eq!(
            journal_recovery(&plain),
            JournalRecovery::Finish {
                mode: LocalBackendMode::Session,
                delete_dest: Vec::new(),
                delete_source: vec![3],
            }
        );
    }

    /// **The active tab is mapped by the source position it came from,
    /// not by an index into what landed** (plan 063 §D8 phase 4).
    ///
    /// The fixture puts the failure in the **middle** on purpose. Four
    /// source tabs `A B C D`, `B` refused, so what landed is `[A, C,
    /// D]` — and the source's active tab is `C`, at source index 2,
    /// which is index *1* in the compacted list. A positional read
    /// hands back `D`: the switch reports success and the user is
    /// looking at a tab they did not leave. With the failure at the end
    /// the two readings agree and the bug is invisible, which is why it
    /// is not there.
    #[test]
    fn the_selection_maps_through_the_source_position_a_compacted_list_loses() {
        let created = [CreatedProject {
            source: 101,
            project: 7,
            // A(0) → 70, B(1) refused, C(2) → 72, D(3) → 73.
            tabs: vec![
                CreatedTab { source: 0, tab: 70 },
                CreatedTab { source: 2, tab: 72 },
                CreatedTab { source: 3, tab: 73 },
            ],
            complete: false,
        }];
        assert_eq!(mapped_selection(&created, Some((0, 2))), Some((7, 72)));
        assert_eq!(mapped_selection(&created, Some((0, 3))), Some((7, 73)));
        assert_eq!(mapped_selection(&created, Some((0, 0))), Some((7, 70)));
    }

    /// The three ways the pair names nothing, and the one answer for
    /// all of them: the first tab of the first project that landed
    /// whole. Never `None` while anything landed — a switch that moved
    /// the work and selected nothing leaves an empty pane.
    #[test]
    fn a_selection_that_did_not_land_falls_back_to_a_tab_that_did() {
        let created = [
            CreatedProject {
                source: 101,
                project: 7,
                tabs: vec![CreatedTab { source: 0, tab: 70 }],
                // Its source tab 1 was refused, so the project is not
                // whole — and tab 1 is what the source had selected.
                complete: false,
            },
            CreatedProject {
                source: 102,
                project: 8,
                tabs: landed(&[80, 81]),
                complete: true,
            },
        ];
        // The very tab that was active is the one that did not land.
        assert_eq!(mapped_selection(&created, Some((0, 1))), Some((8, 80)));
        // A project index past what was created at all.
        assert_eq!(mapped_selection(&created, Some((9, 0))), Some((8, 80)));
        // Nothing was selected in the source.
        assert_eq!(mapped_selection(&created, None), Some((8, 80)));
        // And with nothing landed whole there is nothing to fall back
        // to, which the fence reads as "no pair to wait for".
        assert_eq!(mapped_selection(&created[..1], Some((0, 1))), None);
        assert_eq!(mapped_selection(&[], Some((0, 0))), None);
    }

    #[test]
    fn the_fence_waits_for_the_mirror_the_selection_and_a_band_to_show() {
        let created = [
            CreatedProject {
                source: 101,
                project: 1,
                tabs: landed(&[10, 11]),
                complete: true,
            },
            CreatedProject {
                source: 102,
                project: 2,
                tabs: landed(&[20]),
                complete: true,
            },
        ];
        let mirrored = [row(1, &[10, 11]), row(2, &[20])];
        let base = SwitchFence {
            created: &created,
            mirrored: &mirrored,
            selected: Some((2, 20)),
            wanted: Some((2, 20)),
            attached: true,
        };
        assert!(
            fence_holds(base),
            "the baseline must hold, or every row below proves nothing"
        );

        // The mirror has not published the second project yet — the
        // reply that named it arrived first (§2).
        let behind = [row(1, &[10, 11])];
        assert!(!fence_holds(SwitchFence {
            mirrored: &behind,
            ..base
        }));

        // …unless that project is one the replay did **not** land
        // whole. Its source is kept for exactly that reason, and the
        // destination may hold nothing of it at all — the engine closes
        // a tab whose shell would not start, and closing a project's
        // last tab deletes the project. Waiting for that row is waiting
        // for one that is never coming.
        let partial = [
            CreatedProject {
                source: 101,
                project: 1,
                tabs: landed(&[10, 11]),
                complete: true,
            },
            CreatedProject {
                source: 102,
                project: 2,
                tabs: Vec::new(),
                complete: false,
            },
        ];
        assert!(fence_holds(SwitchFence {
            created: &partial,
            mirrored: &behind,
            selected: Some((1, 10)),
            wanted: Some((1, 10)),
            attached: true,
        }));

        // It has the project but not all of its tabs.
        let partial = [row(1, &[10]), row(2, &[20])];
        assert!(!fence_holds(SwitchFence {
            mirrored: &partial,
            ..base
        }));

        // Nothing on screen at all: releasing here re-arms the exit
        // latch against an empty window.
        assert!(!fence_holds(SwitchFence {
            mirrored: &[],
            ..base
        }));
        // **The row the emptiness clause is load-bearing in.** A source
        // with nothing in it creates nothing, so "every created row is
        // mirrored" is vacuously true and "the mapped tab is selected"
        // has no tab to ask about — every other clause says yes, and the
        // guard would come off onto a window with no band at all.
        assert!(!fence_holds(SwitchFence {
            created: &[],
            mirrored: &[],
            selected: None,
            wanted: None,
            attached: false,
        }));

        // The mapped tab is not the one selected.
        assert!(!fence_holds(SwitchFence {
            selected: Some((1, 10)),
            ..base
        }));
        assert!(!fence_holds(SwitchFence {
            selected: None,
            ..base
        }));

        // Selected, listed, mirrored — and the attach has not landed.
        // §D8 phase 6 asks for "selected **and** attached", and an
        // attach can be refused after the selection is made.
        assert!(!fence_holds(SwitchFence {
            attached: false,
            ..base
        }));

        // A source with nothing selected asks for no selection, and the
        // rest of the fence still applies. `attached` is not consulted
        // there — there is no tab for it to be about.
        assert!(fence_holds(SwitchFence {
            selected: None,
            wanted: None,
            attached: false,
            ..base
        }));
        assert!(!fence_holds(SwitchFence {
            mirrored: &[],
            selected: None,
            wanted: None,
            attached: false,
            ..base
        }));

        // A selection at a row the mirror does not carry is not a tab
        // anybody is looking at, however matched the pair is.
        let gone = [row(1, &[10, 11]), row(2, &[])];
        assert!(!fence_holds(SwitchFence {
            created: &[],
            mirrored: &gone,
            ..base
        }));
    }

    /// The quiesced surfaces (plan 063 §D8a), enumerated so a new
    /// mutating action is an omission somebody can see.
    ///
    /// The two lists are asserted to *agree*, because they are the same
    /// four gestures reached two ways — the keybind and the palette row
    /// — and a switch that stopped one but not the other would be a
    /// hole with a passing test over it.
    #[test]
    fn the_four_local_mutations_are_refused_from_both_surfaces() {
        use KeybindAction::*;
        for action in [NewTab, NewProject, CloseTab, CloseProject] {
            assert!(keybind_mutates_local_backend(action), "{action:?}");
        }
        for action in [
            RenameTab,
            RenameProject,
            CycleTabNext,
            CycleTabPrev,
            JumpToUnread,
            Copy,
            Paste,
            ToggleSidebar,
            CommandPalette,
            NewProjectOnHost,
        ] {
            assert!(!keybind_mutates_local_backend(action), "{action:?}");
        }
        for id in ["new_tab", "new_project", "close_tab", "close_project"] {
            assert!(palette_row_mutates_local_backend(id), "{id}");
        }
        for id in [
            "rename_tab",
            "toggle_sidebar",
            "jump_to_unread",
            "cycle_tab_next",
            roost_ui_model::host_verbs::ADD_ID,
            roost_ui_model::host_verbs::USE_SESSION_ID,
        ] {
            assert!(!palette_row_mutates_local_backend(id), "{id}");
        }

        assert_eq!(
            SWITCH_BUSY_PALETTE,
            "busy: a local-backend switch is in progress"
        );
    }

    /// The confirm copy names the counts and the irreversible part —
    /// singular and plural, because "1 projects" is the tell that a
    /// count was pasted rather than written.
    #[test]
    fn the_forward_card_counts_what_it_is_about_to_end() {
        let one = forward_confirm_body(1, 1);
        assert!(one.contains("1 project and 1 tab"), "{one}");
        let many = forward_confirm_body(3, 7);
        assert!(many.contains("3 projects and 7 tabs"), "{many}");
        assert!(many.contains("shells"), "{many}");
        assert!(forward_confirm_body(0, 0).contains("0 projects and 0 tabs"));

        // Reverse says the thing that surprises: nothing comes back.
        let back = reverse_confirm_body("localhost");
        assert!(back.contains("nothing is copied back"), "{back}");
        assert!(back.contains("localhost"), "{back}");
    }

    /// A stand-in session on the far end of a `HostOps` queue.
    ///
    /// Enough of one to run the real [`replay_onto_slot`] against: it
    /// answers `project.create`, `tab.open` and `tab.set_title` the way
    /// a session does, and refuses whichever calls the case names. No
    /// daemon, no socket, no filesystem — which is the only way to
    /// reach the "this one op failed" branches at all, since a *live*
    /// session refuses these ops only for reasons a test cannot
    /// manufacture on demand.
    ///
    /// The worker hands back the params of every `tab.open` it was sent,
    /// once the replay has dropped its `HostOps`.
    fn fake_slot(
        refuse: impl Fn(&str, usize) -> bool + Send + 'static,
    ) -> (
        crate::host_conn::HostOps,
        tokio::task::JoinHandle<Vec<serde_json::Value>>,
    ) {
        use roost_ipc::messages::ops as wire;

        let (ops, mut rx) = crate::host_conn::HostOps::channel();
        let worker = tokio::spawn(async move {
            let mut next_id = 100_i64;
            let mut seen: HashMap<String, usize> = HashMap::new();
            let mut opened = Vec::new();
            while let Some(intent) = rx.recv().await {
                let op = intent.op.to_string();
                if op == wire::TAB_OPEN {
                    opened.push(intent.params.clone());
                }
                let index = {
                    let count = seen.entry(op.clone()).or_insert(0);
                    let index = *count;
                    *count += 1;
                    index
                };
                if refuse(&op, index) {
                    intent.answer(Err(crate::host_conn::HostOpError::Rejected {
                        code: roost_ipc::client::ServerCode::Internal,
                        message: format!("{op} refused by the stand-in"),
                    }));
                    continue;
                }
                next_id += 1;
                let answer = match op.as_str() {
                    wire::PROJECT_CREATE => {
                        let mut made = made_project(next_id);
                        made.cwd = intent.params["cwd"].as_str().unwrap_or("/tmp").to_string();
                        made.name = intent.params["name"].as_str().unwrap_or("").to_string();
                        serde_json::json!({ "project": made })
                    }
                    wire::TAB_OPEN => serde_json::json!({ "tab": tab(next_id) }),
                    _ => serde_json::json!({}),
                };
                intent.answer(Ok(answer));
            }
            opened
        });
        (ops, worker)
    }

    async fn replay(
        snapshot: Vec<SwitchProject>,
        refuse: impl Fn(&str, usize) -> bool + Send + 'static,
    ) -> ReplayOutcome {
        let dir = tempfile::tempdir().unwrap();
        replay_journalled_at(&journal_path(dir.path()), snapshot, refuse)
            .await
            .0
    }

    /// The same replay with the journal somewhere the caller chose —
    /// which is the only way to drive a journal write that *fails* —
    /// beside the params of every `tab.open` it sent.
    async fn replay_journalled_at(
        path: &Path,
        snapshot: Vec<SwitchProject>,
        refuse: impl Fn(&str, usize) -> bool + Send + 'static,
    ) -> (ReplayOutcome, Vec<serde_json::Value>) {
        let home = tempfile::tempdir().unwrap();
        let (ops, worker) = fake_slot(refuse);
        let journal = SwitchJournal::new(SwitchDirection::ToSession, snapshot.clone());
        let outcome = replay_onto_slot(
            &ops,
            snapshot,
            path,
            journal,
            &home.path().to_string_lossy(),
            SPAWN_GRID,
        )
        .await;
        drop(ops);
        (outcome, worker.await.unwrap_or_default())
    }

    /// A journal path nothing can be written to, arranged by **creating**
    /// a file where `write_journal` needs a directory — never by removing
    /// or chmod-ing anything, and inside this test's own temp dir.
    fn an_unwritable_journal_path(dir: &Path) -> PathBuf {
        let blocked = dir.join("not-a-directory");
        std::fs::write(&blocked, b"").unwrap();
        journal_path(&blocked)
    }

    fn source(id: i64, tabs: &[(&str, bool)]) -> SwitchProject {
        SwitchProject {
            source: id,
            name: format!("p{id}"),
            cwd: "/tmp".into(),
            tabs: tabs
                .iter()
                .map(|(title, user_titled)| SwitchTab {
                    cwd: "/tmp".into(),
                    title: (*title).into(),
                    user_titled: *user_titled,
                })
                .collect(),
        }
    }

    /// **A destination copy whose id could not be recorded stops the
    /// replay** (plan 063 §D8 phase 3).
    ///
    /// C6's rule at the other journal write: if we cannot record it, we
    /// cannot recover it. The id lives in memory until the write lands,
    /// so a replay that logged the failure and carried on would leave a
    /// real project on the destination that no later launch can name —
    /// `rollback_targets` reads the list off the file — and the switch
    /// would go on to delete the source it copied.
    #[tokio::test]
    async fn a_project_whose_id_cannot_be_recorded_fails_the_replay() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = vec![source(11, &[("a", false)]), source(22, &[("b", false)])];
        let outcome =
            replay_journalled_at(&an_unwritable_journal_path(dir.path()), snapshot, |_, _| {
                false
            })
            .await
            .0;

        assert!(
            outcome.error.is_some(),
            "an id that could not be recorded is a replay failure, not a warning"
        );
        // **The copy is still handed back.** It exists on the
        // destination, and `forward_roll_back` deletes exactly what is
        // in this list — an id missing here is an orphan for good.
        assert_eq!(
            outcome
                .created
                .iter()
                .map(|made| made.source)
                .collect::<Vec<_>>(),
            vec![11],
            "it stopped at the first project, and carried that project out"
        );
        assert!(
            !outcome.created[0].complete,
            "nothing was opened in it, so it can never cost its source a deletion"
        );
        assert!(outcome.created[0].tabs.is_empty());
        assert!(
            deletable_sources(&outcome.created).is_empty(),
            "and the source of the copy this run abandons is kept"
        );
    }

    /// **A tab that failed to replay must cost its project the source
    /// deletion** (plan 063 §D8 phase 3/5).
    ///
    /// The bug this pins: the replay skipped a refused tab and returned
    /// `error: None`, so phase 5 deleted the source project *including
    /// the tab that never landed*, and the fence passed because
    /// `created.tabs` only ever held the ones that did. A switch that
    /// reported success over a tab that is now nowhere.
    ///
    /// The cwd fallback in phase 3 exists so a **vanished directory**
    /// cannot abort a whole replay. It is not a licence to lose a tab.
    #[tokio::test]
    async fn a_tab_that_does_not_land_keeps_its_source_project() {
        let snapshot = vec![source(11, &[("a", false)]), source(22, &[("b", false)])];

        // Everything lands: both are deletable, in snapshot order.
        let all = replay(snapshot.clone(), |_, _| false).await;
        assert!(all.error.is_none());
        assert_eq!(
            all.created
                .iter()
                .map(|made| made.source)
                .collect::<Vec<_>>(),
            vec![11, 22]
        );
        assert!(all.created.iter().all(|made| made.complete));
        assert_eq!(deletable_sources(&all.created), vec![11, 22]);

        // The **second** `tab.open` is refused. The replay carries on —
        // one tab must not cost the rest of the layout — but the project
        // it belonged to is no longer deletable, and the one that landed
        // whole still is.
        let partial = replay(snapshot.clone(), |op, index| {
            op == roost_ipc::messages::ops::TAB_OPEN && index == 1
        })
        .await;
        assert!(
            partial.error.is_none(),
            "a tab is not a project-level failure"
        );
        assert_eq!(partial.created.len(), 2, "both projects were created");
        assert!(partial.created[0].complete);
        assert!(!partial.created[1].complete);
        assert!(partial.created[1].tabs.is_empty());
        assert_eq!(
            deletable_sources(&partial.created),
            vec![11],
            "the source of the tab that never landed is kept"
        );
    }

    /// The same rule for a title lock, which is part of what the source
    /// held even though losing it costs less.
    #[tokio::test]
    async fn a_title_lock_that_does_not_land_keeps_its_source_project_too() {
        let snapshot = vec![source(11, &[("pinned", true)])];
        let outcome = replay(snapshot, |op, _| {
            op == roost_ipc::messages::ops::TAB_SET_TITLE
        })
        .await;
        assert!(outcome.error.is_none());
        assert_eq!(outcome.created.len(), 1);
        assert_eq!(
            outcome.created[0].tabs.len(),
            1,
            "the tab itself landed; only its lock did not"
        );
        assert!(!outcome.created[0].complete);
        assert!(deletable_sources(&outcome.created).is_empty());
    }

    /// Plan 070 §D2: the tab a replay copies ran on the other backend,
    /// so the snapshot's own cwd is the whole answer.
    #[tokio::test]
    async fn a_replayed_tab_names_no_source_tab() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = vec![source(11, &[("a", false), ("b", true)])];
        let (outcome, opened) =
            replay_journalled_at(&journal_path(dir.path()), snapshot, |_, _| false).await;

        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(opened.len(), 2, "both tabs were replayed: {opened:?}");
        for params in &opened {
            assert!(
                params.get("cwd_from_tab").is_none(),
                "a replayed tab.open must not carry the key at all: {params}"
            );
        }
    }

    #[tokio::test]
    async fn a_replayed_tab_spawns_at_the_grid_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = vec![source(11, &[("a", false)]), source(22, &[("b", true)])];
        let (outcome, opened) =
            replay_journalled_at(&journal_path(dir.path()), snapshot, |_, _| false).await;

        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(opened.len(), 2, "both tabs were replayed: {opened:?}");
        for params in &opened {
            assert_eq!(params["cols"], SPAWN_GRID.0, "{params}");
            assert_eq!(params["rows"], SPAWN_GRID.1, "{params}");
        }
    }

    /// A project-level failure is different in kind: the tabs under it
    /// have nowhere to go, so the replay stops and hands back what it
    /// had made — which is what the rollback deletes.
    #[tokio::test]
    async fn a_project_that_cannot_be_created_stops_the_replay() {
        let snapshot = vec![
            source(11, &[("a", false)]),
            source(22, &[("b", false)]),
            source(33, &[("c", false)]),
        ];
        let outcome = replay(snapshot, |op, index| {
            op == roost_ipc::messages::ops::PROJECT_CREATE && index == 1
        })
        .await;
        assert!(outcome.error.is_some(), "{outcome:?}");
        assert_eq!(
            outcome
                .created
                .iter()
                .map(|made| made.source)
                .collect::<Vec<_>>(),
            vec![11],
            "only what committed before the refusal, for the rollback to undo"
        );
    }

    /// Plan 063 §D8's phase-3 cwd rule: a tab whose directory went away
    /// falls back to the project's, then to `$HOME` — it does not fail
    /// the tab, and it does not abort the replay.
    #[test]
    fn a_replayed_tab_falls_back_from_a_directory_that_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let project = project.to_string_lossy().into_owned();
        let tab = dir.path().join("tab");
        std::fs::create_dir(&tab).unwrap();
        let tab = tab.to_string_lossy().into_owned();
        let home = dir.path().to_string_lossy().into_owned();
        let gone = dir.path().join("gone").to_string_lossy().into_owned();

        assert_eq!(replay_cwd(&tab, &project, &home), tab, "the tab's own wins");
        assert_eq!(replay_cwd(&gone, &project, &home), project);
        assert_eq!(replay_cwd(&gone, &gone, &home), home);
        // An empty cwd is not a directory, and neither is a file.
        assert_eq!(replay_cwd("", &project, &home), project);
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let file = file.to_string_lossy().into_owned();
        assert_eq!(replay_cwd(&file, &project, &home), project);
        // Nothing resolves: the answer is still a path, never an empty
        // string — an empty cwd would spawn wherever the UI started.
        assert_eq!(replay_cwd(&gone, &gone, &gone), gone);
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use roost_engine::{RestoreLayout, RestoreProject, RestoreTab};
    use roost_ipc::messages::{Tab, TabState};

    fn project(id: i64, name: &str, cwd: &str, tabs: &[Spec<'_>]) -> Project {
        Project {
            id,
            name: name.into(),
            cwd: cwd.into(),
            position: 0,
            created_at: 0,
            tabs: tabs
                .iter()
                .enumerate()
                .map(|(index, (cwd, title, user_titled))| Tab {
                    id: id * 100 + index as i64,
                    project_id: id,
                    title: (*title).into(),
                    cwd: (*cwd).into(),
                    state: TabState::Idle,
                    has_notification: false,
                    is_active: false,
                    user_titled: *user_titled,
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

    /// A tab, as both fixtures spell one: `(cwd, title, user_titled)`.
    type Spec<'a> = (&'a str, &'a str, bool);

    fn retained(rows: &[(i64, &[Spec<'_>])], active: (i64, i32)) -> RestoreLayout {
        RestoreLayout {
            projects: rows
                .iter()
                .map(|(project_id, tabs)| RestoreProject {
                    project_id: *project_id,
                    tabs: tabs
                        .iter()
                        .map(|(cwd, title, user_titled)| RestoreTab {
                            cwd: (*cwd).into(),
                            title: (*title).into(),
                            user_titled: *user_titled,
                        })
                        .collect(),
                })
                .collect(),
            active_project_id: active.0,
            active_tab_position: active.1,
        }
    }

    fn shape(source: &MigrationSource) -> Vec<(&str, Vec<Spec<'_>>)> {
        source
            .projects
            .iter()
            .map(|project| {
                (
                    project.name.as_str(),
                    project
                        .tabs
                        .iter()
                        .map(|tab| (tab.cwd.as_str(), tab.title.as_str(), tab.user_titled))
                        .collect(),
                )
            })
            .collect()
    }

    /// The fixture the launch migration actually meets: **three
    /// projects with different tab counts, and not one live tab between
    /// them.**
    ///
    /// That is what a `session`-mode launch leaves — `Workspace::open`
    /// loads the rows and the bootstrap deliberately does not hydrate
    /// them (§D5) — and it is why the snapshot may not be read off the
    /// projects. A migration that counted live tabs would replay three
    /// empty projects and then delete the originals, and the uneven
    /// fixture is what makes that visible rather than merely wrong: the
    /// tab counts, the titles, the one title lock and the active pair
    /// each fail on their own.
    #[test]
    fn the_launch_snapshot_comes_from_the_layout_the_launch_never_opened() {
        let projects = [
            project(7, "alpha", "/home/a", &[]),
            project(8, "beta", "/home/b", &[]),
            project(9, "gamma", "/home/c", &[]),
        ];
        let layout = retained(
            &[
                (
                    7,
                    &[("/home/a", "editor", true), ("/home/a/src", "build", false)],
                ),
                (8, &[("/home/b", "logs", false)]),
                (
                    9,
                    &[
                        ("/home/c", "one", false),
                        ("/home/c/x", "two", false),
                        ("/home/c/y", "three", true),
                    ],
                ),
            ],
            // The third project's last tab: a pair no off-by-one and no
            // "just take the first" can land on by accident.
            (9, 2),
        );

        let migrated = retained_migration(&projects, Some(&layout)).expect("a layout to migrate");
        assert_eq!(
            shape(&migrated),
            vec![
                (
                    "alpha",
                    vec![("/home/a", "editor", true), ("/home/a/src", "build", false)]
                ),
                ("beta", vec![("/home/b", "logs", false)]),
                (
                    "gamma",
                    vec![
                        ("/home/c", "one", false),
                        ("/home/c/x", "two", false),
                        ("/home/c/y", "three", true)
                    ]
                ),
            ]
        );
        assert_eq!(
            migrated
                .projects
                .iter()
                .map(|p| (p.source, p.cwd.as_str()))
                .collect::<Vec<_>>(),
            vec![(7, "/home/a"), (8, "/home/b"), (9, "/home/c")],
            "the source ids are what phase 5 deletes; the cwds are where the copies land"
        );
        assert_eq!(migrated.active_at, Some((2, 2)));
    }

    /// A live tab outranks a descriptor of the same project, because it
    /// is the newer truth — the same precedence
    /// `Workspace::snapshot_for_persist` applies to the same two
    /// sources. Unreachable from a `session` launch, which hydrates
    /// nothing; asserted so a workspace that *has* been hydrated cannot
    /// be replayed from a stale layout.
    #[test]
    fn a_project_that_has_live_tabs_is_snapshotted_from_them() {
        let projects = [
            project(7, "alpha", "/home/a", &[("/live", "live", false)]),
            project(8, "beta", "/home/b", &[]),
        ];
        let layout = retained(
            &[
                (7, &[("/stale", "stale", true)]),
                (8, &[("/home/b", "logs", false)]),
            ],
            (0, 0),
        );

        let migrated = retained_migration(&projects, Some(&layout)).expect("a layout to migrate");
        assert_eq!(
            shape(&migrated),
            vec![
                ("alpha", vec![("/live", "live", false)]),
                ("beta", vec![("/home/b", "logs", false)]),
            ]
        );
    }

    /// The two ways there is nothing to migrate, and the one way a
    /// layout can be short.
    #[test]
    fn an_empty_workspace_owes_no_migration_and_a_missing_row_owes_no_tabs() {
        assert_eq!(retained_migration(&[], None), None);
        assert_eq!(
            retained_migration(&[], Some(&retained(&[(7, &[("/x", "x", false)])], (7, 0)))),
            None,
            "a layout without projects to hang it on is not a migration"
        );

        // A project the layout does not mention — a row written after
        // the file was loaded — migrates as itself with no tabs rather
        // than aborting the rest.
        let migrated = retained_migration(
            &[
                project(7, "alpha", "/home/a", &[]),
                project(8, "beta", "/home/b", &[]),
            ],
            Some(&retained(&[(7, &[("/home/a", "editor", true)])], (7, 0))),
        )
        .expect("a layout to migrate");
        assert_eq!(
            shape(&migrated),
            vec![
                ("alpha", vec![("/home/a", "editor", true)]),
                ("beta", vec![]),
            ]
        );
        assert_eq!(migrated.active_at, Some((0, 0)));
    }

    /// A selection the snapshot cannot name is no selection. Both ways
    /// it can fail to name one: a project that is gone, and an index
    /// past the tabs that project actually has. `forward_commit` falls
    /// back to the first tab it landed, which is a tab the user can
    /// see — a pair pointing at nothing is not.
    #[test]
    fn a_retained_selection_that_does_not_resolve_is_dropped() {
        let projects = [project(7, "alpha", "/home/a", &[])];
        let tabs: &[Spec<'_>] = &[("/home/a", "editor", true)];
        for active in [(99, 0), (7, 4), (7, -1), (0, 0)] {
            assert_eq!(
                retained_migration(&projects, Some(&retained(&[(7, tabs)], active)))
                    .expect("a layout to migrate")
                    .active_at,
                None,
                "{active:?}"
            );
        }
        assert_eq!(
            retained_migration(&projects, None)
                .expect("a layout to migrate")
                .active_at,
            None,
            "no layout at all names no tab"
        );
    }
}
