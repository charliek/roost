//! The palette's host-session verb family, and the one policy seam that
//! decides which of them a build offers (plan 037 §3.1/§3.5, plan 041
//! §3.1).
//!
//! Every host action lives in the command palette — there is no host
//! menu and no per-section button beyond the inline ↻ Reconnect. One
//! item per (verb, host) pair, so fuzzy-matching "disc pop" reaches
//! exactly one row, and **verbs appear only when they apply**: you
//! cannot Stop a session you are not attached to.
//!
//! Every shipping build offers the localhost surface. The withheld
//! answer stays expressible as a [`VerbPolicy`] value rather than a
//! `cfg!` sprinkled through the adapter, so a build that genuinely
//! cannot reach a local session could hide the whole surface coherently
//! — and both answers stay unit tests that run on any OS.

use roost_ipc::LocalBackendMode;

use crate::host_sidebar::{
    FidelityAction, HostTransportKind, LocalSlot, SectionState, LOCAL_LABEL,
};

/// Palette item ids. The prefix is what routes an activation back here;
/// everything after the second colon is the saved host's id, which is
/// opaque (hex) and so cannot contain a delimiter.
pub const ADD_ID: &str = "host:add";
pub const NEW_PROJECT_ON_ID: &str = "host:new_project_on";
const CONNECT_PREFIX: &str = "host:connect:";
const DISCONNECT_PREFIX: &str = "host:disconnect:";
const STOP_PREFIX: &str = "host:stop:";
const REMOVE_PREFIX: &str = "host:remove:";
const CREATE_ON_PREFIX: &str = "host:create_on:";
const UPDATE_PREFIX: &str = "host:update:";
const RESTART_PREFIX: &str = "host:restart:";
/// A forgotten host offered back (plan 063 §D7). Keyed by **target**,
/// not by a saved id: the row exists precisely because the host is no
/// longer saved, and the target is what the recents list dedupes on, so
/// it is the only key that is unique by construction. A target may
/// contain colons; the whole remainder of the id is it.
const RECENT_PREFIX: &str = "host:recent:";
/// The same forgotten host in the creation picker, which also opens a
/// project once it is up. A separate id rather than a flag on the wire:
/// a palette row is addressed by its id, and the two rows do different
/// things.
const CREATE_ON_RECENT_PREFIX: &str = "host:create_on_recent:";

/// The id of the seeded-localhost Connect row — the one verb addressed
/// to a host that is not saved yet (plan 037 §3.5). Activating it saves
/// `localhost` and connects in one step, so a fresh install reaches its
/// own session without an Add Host detour.
pub const CONNECT_SEED_ID: &str = "host:connect_seed";

/// The label and target the seeded entry saves under.
pub const SEED_LABEL: &str = "localhost";

/// The picker row for the in-process workspace. Not a host id: the
/// local workspace has none.
pub const CREATE_ON_LOCAL_ID: &str = "host:create_on:local";

/// The picker's `localhost` row when this machine's session is **not**
/// ready to create on (plan 063 §D3): not saved yet, or saved and down.
/// Activating it saves and/or starts the session first and creates once
/// it is up. A slot that *is* connected gets the ordinary
/// `host:create_on:<saved_id>` row instead, so the ready case has one
/// spelling and one dispatch.
///
/// Not a `host:create_on:` id: that prefix is followed by a saved id,
/// and this row exists precisely when there may not be one.
pub const CREATE_ON_LOCALHOST_ID: &str = "host:create_on_localhost";

/// The two local-backend switch rows (plan 063 §D8).
///
/// `local:` rather than `host:` because neither is addressed to a saved
/// host — they move *which backend the local band is*, and only one of
/// the two directions even involves the slot. They live in this module
/// anyway: the palette resolves a command row by handing its id to
/// [`parse`], and a second lookup table beside it would be a second
/// place for a row to go unrouted.
pub const USE_SESSION_ID: &str = "local:use_session";
pub const USE_IN_PROCESS_ID: &str = "local:use_in_process";

/// One saved host, as the verb builder reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostRow<'a> {
    /// `HostSnapshot.id` — what every verb is addressed to.
    pub saved_id: &'a str,
    pub label: &'a str,
    /// The registry's target, verbatim — the key a *recent* is matched
    /// against (plan 063 §D7's [`offerable_recents`]). Not a second
    /// spelling of `transport`: that says how a host is reached, this
    /// says which host it is.
    pub target: &'a str,
    pub state: SectionState,
    /// How this host is reached. Only [`HostTransportKind::localhost`]
    /// rides the policy; a host reached over an `ssh -L` forward keeps
    /// its full verb set under either answer.
    pub transport: HostTransportKind,
    /// What this host's live connection offers about its fidelity, from
    /// [`crate::host_sidebar::fidelity_action`]. `None` for every host
    /// that is connected at exact fidelity or is not connected at all,
    /// which is the ordinary case.
    pub fidelity: Option<FidelityAction>,
}

/// One forgotten host, as the recents rows read it (plan 063 §D7).
///
/// Carries no state: a recent is not a connection, it is a target and
/// the name it last went by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecentRow<'a> {
    pub label: &'a str,
    pub target: &'a str,
}

/// The policy, as one value.
///
/// `localhost_surface` says whether this client offers to reach a
/// session on this machine at all. [`VerbPolicy::current`] answers yes
/// everywhere; `false` is kept expressible so hiding the surface stays a
/// one-value change if some build ever cannot reach one (plan 041 §3.1's
/// named fallback). `Add Host` is deliberately *never* gated — pointing
/// at an `ssh -L` forward to another box is a real destination whatever
/// the local answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerbPolicy {
    pub localhost_surface: bool,
}

impl VerbPolicy {
    /// What this build ships with: every platform packages a
    /// `roost-session` its client can reach (plan 041). A binary that is
    /// genuinely missing gets the spawn ladder's own three-rung error,
    /// which is actionable — a silently absent menu is not.
    pub fn current() -> Self {
        Self {
            localhost_surface: true,
        }
    }
}

/// A verb the palette offered, resolved back from the row id it was
/// activated with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostVerb {
    /// Open the Add Host dialog.
    Add,
    /// Save `localhost` and connect it, in one step.
    ConnectSeed,
    Connect(String),
    Disconnect(String),
    /// Stop the session (confirmed). Connected only — stopping requires
    /// being attached to what you stop.
    Stop(String),
    /// Send this ssh host a matching `roost-session` and restart it, so
    /// the connection leaves the `vt` fallback.
    Update(String),
    /// Restart this machine's own session, for the same reason.
    Restart(String),
    Remove(String),
    /// Drill into the "New Project on…" picker.
    NewProjectOn,
    /// A picker row: create on this host, or `None` for local.
    CreateOn(Option<String>),
    /// The picker's `localhost` row for a session that is not ready:
    /// save it if it is not saved, start it if it is not running, then
    /// create (plan 063 §D3).
    CreateOnLocalhost,
    /// A forgotten host, offered back by target (plan 063 §D7): save it
    /// again and connect. `create` says whether the row was pressed in
    /// the creation picker, which also opens a project on it once it is
    /// up — the two rows differ only in that tail, so they are one verb.
    AddRecent {
        target: String,
        create: bool,
    },
    /// Move the local band onto a `roost-session` on this machine, or
    /// back into this process (plan 063 §D8). Both open a confirm; the
    /// direction is the whole payload.
    UseSession,
    UseInProcess,
}

/// Parse a palette row id back into the verb it names.
///
/// `None` for anything that is not a host row, which is how the
/// adapter's `run_palette_row` tells a host verb from the rest of the
/// command frame without a second lookup table.
pub fn parse(id: &str) -> Option<HostVerb> {
    let saved = |prefix: &str| id.strip_prefix(prefix).map(str::to_string);
    match id {
        ADD_ID => return Some(HostVerb::Add),
        CONNECT_SEED_ID => return Some(HostVerb::ConnectSeed),
        NEW_PROJECT_ON_ID => return Some(HostVerb::NewProjectOn),
        CREATE_ON_LOCAL_ID => return Some(HostVerb::CreateOn(None)),
        CREATE_ON_LOCALHOST_ID => return Some(HostVerb::CreateOnLocalhost),
        USE_SESSION_ID => return Some(HostVerb::UseSession),
        USE_IN_PROCESS_ID => return Some(HostVerb::UseInProcess),
        _ => {}
    }
    if let Some(host) = saved(CONNECT_PREFIX) {
        return Some(HostVerb::Connect(host));
    }
    if let Some(host) = saved(DISCONNECT_PREFIX) {
        return Some(HostVerb::Disconnect(host));
    }
    if let Some(host) = saved(STOP_PREFIX) {
        return Some(HostVerb::Stop(host));
    }
    if let Some(host) = saved(UPDATE_PREFIX) {
        return Some(HostVerb::Update(host));
    }
    if let Some(host) = saved(RESTART_PREFIX) {
        return Some(HostVerb::Restart(host));
    }
    if let Some(host) = saved(REMOVE_PREFIX) {
        return Some(HostVerb::Remove(host));
    }
    // Before `CREATE_ON_PREFIX` for legibility only — the two cannot
    // collide (`host:create_on_recent:` has `_` where the other has `:`).
    if let Some(target) = saved(CREATE_ON_RECENT_PREFIX) {
        return Some(HostVerb::AddRecent {
            target,
            create: true,
        });
    }
    if let Some(target) = saved(RECENT_PREFIX) {
        return Some(HostVerb::AddRecent {
            target,
            create: false,
        });
    }
    // Checked last: `host:create_on:local` is the local sentinel above,
    // and a saved id can never be the word "local" (labels can't be, and
    // ids are hex).
    saved(CREATE_ON_PREFIX).map(|host| HostVerb::CreateOn(Some(host)))
}

/// One palette row: what the adapter turns into a `PaletteItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbItem {
    pub id: String,
    pub title: String,
    /// The one-line "what this does" from the approved mock.
    pub subtitle: Option<String>,
}

impl VerbItem {
    fn new(id: impl Into<String>, title: impl Into<String>, subtitle: &str) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: Some(subtitle.to_string()),
        }
    }
}

/// Whether a host is attached right now. `Connecting` is deliberately
/// not connected: an attempt in flight has reached no session yet, so
/// Stop and Disconnect have nothing to act on.
fn is_connected(state: SectionState) -> bool {
    matches!(state, SectionState::Connected)
}

/// Every host verb this client offers right now, in palette order.
///
/// The rules, all of them:
///
/// * `Add Host…` always — on every platform, with or without saved
///   hosts. It is the only free-text flow and the only entry point that
///   exists before a registry does.
/// * `Connect` / `Disconnect` / `Stop` — one per host, gated on whether
///   that host is attached. A connected host offers the two ways to
///   leave it (disconnect keeps its shells, stop ends them); a host that
///   is not offers the way back in.
/// * `Remove` — on a connected host too (plan 063 §D7: it disconnects
///   first and never stops the session), but never mid-dial, which would
///   race the attempt it is removing, and never on *the slot* under
///   `session` — see [`removable`].
/// * `Update` / `Restart` — only on a connected host whose `fidelity`
///   names one of them, which is plan 056 §3.4's matrix: an ssh host is
///   sent a matching build, this machine's own session is restarted,
///   and a socket target lists neither because nothing here can reach
///   its binary.
/// * The seeded `localhost` row appears while **no saved host has a
///   localhost transport** (plan 063 §D3 — before R9 that could only be
///   an empty registry), and only where the policy offers the localhost
///   surface at all.
/// * One row per **recent** — a host this client forgot — beside
///   `Add Host…`, because that is the same gesture with the typing
///   already done (plan 063 §D7).
/// * `New Project on…` appears once the picker has more than one
///   destination, because with one it is exactly `new_project`.
/// * Exactly one of the two local-backend switch rows (plan 063 §D8),
///   whichever names the backend this client is *not* on — and neither
///   while a switch is in flight (§D8a) or where the policy withholds
///   the localhost surface, since both directions are about a session
///   on this machine.
pub fn verbs(
    hosts: &[HostRow<'_>],
    recents: &[RecentRow<'_>],
    local: LocalSlot<'_>,
    policy: VerbPolicy,
    switching: bool,
) -> Vec<VerbItem> {
    let mut items = vec![VerbItem::new(
        ADD_ID,
        "Add Host…",
        "point Roost at an SSH host or a session socket",
    )];

    items.extend(offerable_recents(hosts, recents).map(|recent| {
        VerbItem::new(
            format!("{RECENT_PREFIX}{}", recent.target),
            format!("Add Host: {}", recent.label),
            "saves it again and connects",
        )
    }));

    if policy.localhost_surface && !hosts.iter().any(|host| host.transport.localhost()) {
        items.push(VerbItem::new(
            CONNECT_SEED_ID,
            format!("Connect Host: {SEED_LABEL}"),
            "starts it if needed",
        ));
    }

    for host in hosts {
        // The policy, applied at exactly one place: where the localhost
        // surface is withheld, a localhost host offers no connection
        // verbs at all. It still lists in the sidebar and can still be
        // removed, which is the only honest thing left to do with it.
        let reachable = policy.localhost_surface || !host.transport.localhost();
        if is_connected(host.state) {
            if reachable {
                items.push(VerbItem::new(
                    format!("{DISCONNECT_PREFIX}{}", host.saved_id),
                    format!("Disconnect Host: {}", host.label),
                    "session keeps running",
                ));
                items.push(VerbItem::new(
                    format!("{STOP_PREFIX}{}", host.saved_id),
                    format!("Stop Session: {}", host.label),
                    "ends shells, keeps layout",
                ));
                match host.fidelity {
                    Some(FidelityAction::Update) => items.push(VerbItem::new(
                        format!("{UPDATE_PREFIX}{}", host.saved_id),
                        format!("Update roost-session on {}", host.label),
                        "installs a matching build and restarts it",
                    )),
                    Some(FidelityAction::Restart) => items.push(VerbItem::new(
                        format!("{RESTART_PREFIX}{}", host.saved_id),
                        format!("Restart session on {}", host.label),
                        "stops and starts it; ends shells, keeps layout",
                    )),
                    Some(FidelityAction::Manual) | None => {}
                }
            }
            if removable(host, local) {
                items.push(VerbItem::new(
                    format!("{REMOVE_PREFIX}{}", host.saved_id),
                    format!("Remove Host: {}", host.label),
                    "disconnects and forgets it; never stops the session",
                ));
            }
            continue;
        }
        if reachable {
            items.push(VerbItem::new(
                format!("{CONNECT_PREFIX}{}", host.saved_id),
                format!("Connect Host: {}", host.label),
                connect_subtitle(host.state),
            ));
        }
        if host.state != SectionState::Connecting && removable(host, local) {
            items.push(VerbItem::new(
                format!("{REMOVE_PREFIX}{}", host.saved_id),
                format!("Remove Host: {}", host.label),
                "forgets it; never stops the session",
            ));
        }
    }

    // Asked of the picker itself rather than recomputed: the row exists
    // exactly when pressing it would offer a choice, and two spellings
    // of that count would drift.
    if create_targets(hosts, recents, local, policy, LOCAL_LABEL).len() > 1 {
        items.push(VerbItem::new(
            NEW_PROJECT_ON_ID,
            "New Project on…",
            "pick the host to create on",
        ));
    }
    // Last: it is the heaviest row in the frame — it ends every running
    // local shell — and the family above is what a person opens this
    // palette for.
    items.extend(switch_row(local.mode, policy, switching));
    items
}

/// The one local-backend switch row this client offers, if any (plan
/// 063 §D8's table).
///
/// A single row rather than a pair with one greyed out: a verb that
/// names the backend you are already on has nothing to do, and the
/// palette's rule is that verbs appear only when they apply.
fn switch_row(mode: LocalBackendMode, policy: VerbPolicy, switching: bool) -> Option<VerbItem> {
    if switching || !policy.localhost_surface {
        return None;
    }
    Some(match mode {
        LocalBackendMode::InProcess => VerbItem::new(
            USE_SESSION_ID,
            "Use a session for local tabs",
            "running local shells end; layout moves; survives quit",
        ),
        LocalBackendMode::Session => VerbItem::new(
            USE_IN_PROCESS_ID,
            "Use in-process local tabs",
            "session keeps running as LOCALHOST; local tabs start fresh",
        ),
    })
}

/// Whether Remove is offered for this host at all (plan 063 §D7).
///
/// Everything is removable except *the slot* under `session`: it is the
/// machine the local band runs on, so forgetting it would leave the
/// window with no local backend and nothing to create in. Under
/// `in-process` the very same saved host is an ordinary `LOCALHOST`
/// band and keeps the verb.
fn removable(host: &HostRow<'_>, local: LocalSlot<'_>) -> bool {
    !(local.mode == LocalBackendMode::Session && Some(host.saved_id) == local.slot_saved_id)
}

/// What Connect means from where the host currently is. `NeedsRestart`
/// is the one that has to say something different: connecting again is
/// how the upgrade dialog gets raised, not a plain retry.
fn connect_subtitle(state: SectionState) -> &'static str {
    match state {
        SectionState::NeedsRestart => "build mismatch — offers a restart",
        SectionState::Stopped => "starts a fresh session",
        _ => "starts it if needed",
    }
}

/// The "New Project on…" picker's rows (plan 063 §D3): the in-process
/// workspace when there is one, then `localhost`, then every other
/// **connected** host.
///
/// Two rules that are not symmetric, deliberately. Ordinary hosts are
/// absent unless connected, which follows the sidebar's own rule (§3.1:
/// dimmed rows are non-interactive) — you cannot create on a session
/// nothing is attached to, and a row that refuses is worse than a row
/// that is not there. `localhost` is different: this client can *make*
/// it ready, so the row is always offered and its subtitle says which of
/// saving, starting or plain creating pressing it will do.
pub fn create_targets(
    hosts: &[HostRow<'_>],
    recents: &[RecentRow<'_>],
    local: LocalSlot<'_>,
    policy: VerbPolicy,
    local_label: &str,
) -> Vec<VerbItem> {
    let slot = slot_row(hosts, local);
    let mut items = Vec::new();
    // Under `session` there is no in-process workspace on screen, so
    // there is nothing for this row to create in.
    if local.mode == LocalBackendMode::InProcess {
        items.push(VerbItem {
            id: CREATE_ON_LOCAL_ID.to_string(),
            title: local_label.to_string(),
            subtitle: None,
        });
    }
    if policy.localhost_surface {
        items.push(localhost_target(slot));
    }
    items.extend(
        hosts
            .iter()
            .filter(|host| is_connected(host.state))
            // The slot already has its row above, in the local band's
            // place; listing it again would be the same session twice.
            .filter(|host| Some(host.saved_id) != slot.map(|slot| slot.saved_id))
            .map(|host| VerbItem {
                id: format!("{CREATE_ON_PREFIX}{}", host.saved_id),
                title: host.label.to_string(),
                subtitle: None,
            }),
    );
    // Last, under the destinations that already exist: a recent is a
    // place to create *after* two round trips (save, connect), so it
    // belongs below every host that is one op away.
    items.extend(offerable_recents(hosts, recents).map(|recent| {
        VerbItem::new(
            format!("{CREATE_ON_RECENT_PREFIX}{}", recent.target),
            recent.label,
            "saves it again, connects, then creates",
        )
    }));
    items
}

/// The recents worth offering: the ones that are not saved again
/// already.
///
/// Nothing prunes the recents list when a host is re-added — a target
/// can be forgotten and saved repeatedly, and a list that rewrote itself
/// on every add would be a second place the registry lives. So the
/// filter is here, where both surfaces read it: a row offering to save a
/// host that is already in the sidebar would fail on the duplicate
/// label, and it would name a band the user can already see.
fn offerable_recents<'a, 'r>(
    hosts: &'a [HostRow<'_>],
    recents: &'a [RecentRow<'r>],
) -> impl Iterator<Item = &'a RecentRow<'r>> {
    recents.iter().filter(|recent| {
        !hosts
            .iter()
            .any(|host| host.target.trim() == recent.target.trim())
    })
}

/// The saved host holding the local-session slot, if one is saved. Read
/// whatever the mode is: under `in-process` it is an ordinary
/// `LOCALHOST` band, and the picker still gives it the `localhost` row.
fn slot_row<'a, 'h>(hosts: &'a [HostRow<'h>], local: LocalSlot<'_>) -> Option<&'a HostRow<'h>> {
    local
        .slot_saved_id
        .and_then(|id| hosts.iter().find(|host| host.saved_id == id))
}

/// The picker's `localhost` row, in whichever of its three states this
/// machine's session is in.
fn localhost_target(slot: Option<&HostRow<'_>>) -> VerbItem {
    match slot {
        // Ready: this is the ordinary create-on-a-host row, sitting in
        // the local band's position. Same id, so the activation is the
        // same one every other host row gets.
        Some(slot) if is_connected(slot.state) => VerbItem {
            id: format!("{CREATE_ON_PREFIX}{}", slot.saved_id),
            title: slot.label.to_string(),
            subtitle: None,
        },
        Some(slot) => VerbItem::new(
            CREATE_ON_LOCALHOST_ID,
            slot.label,
            "starts it if needed, then creates",
        ),
        None => VerbItem::new(
            CREATE_ON_LOCALHOST_ID,
            SEED_LABEL,
            "saves localhost, starts it if needed, then creates",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_sidebar::fidelity_action;

    fn host(saved_id: &str, state: SectionState) -> HostRow<'_> {
        HostRow {
            saved_id,
            label: saved_id,
            // Distinct per host and never a recent's target, so the
            // recents filter neither hides a row by accident nor
            // matches one it should not.
            target: saved_id,
            state,
            transport: HostTransportKind::Ssh,
            fidelity: None,
        }
    }

    /// The registry with nothing forgotten — every pre-§D7 assertion in
    /// this module.
    const NO_RECENTS: &[RecentRow<'static>] = &[];

    fn recent(label: &'static str, target: &'static str) -> RecentRow<'static> {
        RecentRow { label, target }
    }

    fn ids(items: &[VerbItem]) -> Vec<&str> {
        items.iter().map(|item| item.id.as_str()).collect()
    }

    /// What every shipping build answers.
    const FULL: VerbPolicy = VerbPolicy {
        localhost_surface: true,
    };
    /// The withheld answer. No build produces it today; it is here
    /// because the seam has to keep working if one ever does.
    const GATED: VerbPolicy = VerbPolicy {
        localhost_surface: false,
    };

    /// Today's shipping backend, and the one every pre-063 assertion in
    /// this module was written against.
    const IN_PROCESS: LocalSlot<'static> = LocalSlot {
        mode: LocalBackendMode::InProcess,
        slot_saved_id: None,
    };

    /// `session`, with `saved_id` holding the slot.
    fn session(saved_id: &str) -> LocalSlot<'_> {
        LocalSlot {
            mode: LocalBackendMode::Session,
            slot_saved_id: Some(saved_id),
        }
    }

    fn localhost_host(saved_id: &str, state: SectionState) -> HostRow<'_> {
        HostRow {
            transport: HostTransportKind::Localhost,
            ..host(saved_id, state)
        }
    }

    /// Every build ships the localhost surface — asserted on whatever OS
    /// the suite runs, which is how `rust-build`'s two-OS `cargo test`
    /// matrix proves it on both (plan 041 §3.1).
    #[test]
    fn the_shipping_policy_offers_the_localhost_surface() {
        assert!(VerbPolicy::current().localhost_surface);
    }

    /// The zero-host baseline, under both answers. Add Host is
    /// unconditional — it is the only entry point that exists before a
    /// registry does. The seeded localhost Connect rides the surface, so
    /// a withholding build would offer Add Host alone: fewer rows, but
    /// still a real destination rather than a dead end.
    ///
    /// The picker row comes with the surface for the same reason it did
    /// not before plan 063: `localhost` is now a destination that exists
    /// before the registry does, so with the surface there are two
    /// places to create and the picker is a real choice.
    #[test]
    fn a_fresh_registry_offers_add_always_and_the_seed_only_with_the_surface() {
        assert_eq!(
            ids(&verbs(&[], NO_RECENTS, IN_PROCESS, FULL, false)),
            vec![ADD_ID, CONNECT_SEED_ID, NEW_PROJECT_ON_ID, USE_SESSION_ID]
        );
        assert_eq!(
            ids(&verbs(&[], NO_RECENTS, IN_PROCESS, GATED, false)),
            vec![ADD_ID]
        );
    }

    /// The seed row's gate is the *transport*, not the count (plan 063
    /// §D3). An ssh-only registry has no local session saved, so the one
    /// gesture that saves one is still worth offering; a registry that
    /// already holds this machine has nothing left to seed.
    #[test]
    fn the_seed_row_is_gated_on_a_localhost_host_rather_than_on_an_empty_registry() {
        assert!(ids(&verbs(
            &[host("ssh-only", SectionState::Connected)],
            NO_RECENTS,
            IN_PROCESS,
            FULL,
            false
        ))
        .contains(&CONNECT_SEED_ID));
        assert!(!ids(&verbs(
            &[localhost_host("mine", SectionState::Disconnected)],
            NO_RECENTS,
            IN_PROCESS,
            FULL,
            false
        ))
        .contains(&CONNECT_SEED_ID));
    }

    /// The availability matrix, one host at a time. Connected offers the
    /// two ways out; everything else offers the way back in. Remove
    /// rides along in every state but mid-dial (plan 063 §D7).
    #[test]
    fn each_state_offers_exactly_the_verbs_that_can_act() {
        let cases = [
            (
                SectionState::Connected,
                vec![
                    ADD_ID,
                    CONNECT_SEED_ID,
                    "host:disconnect:h",
                    "host:stop:h",
                    "host:remove:h",
                ],
            ),
            (
                SectionState::Disconnected,
                vec![ADD_ID, CONNECT_SEED_ID, "host:connect:h", "host:remove:h"],
            ),
            (
                SectionState::NeedsRestart,
                vec![ADD_ID, CONNECT_SEED_ID, "host:connect:h", "host:remove:h"],
            ),
            (
                SectionState::Stopped,
                vec![ADD_ID, CONNECT_SEED_ID, "host:connect:h", "host:remove:h"],
            ),
            // Mid-dial: reconnecting is fine (it supersedes), removing
            // would race the attempt it is removing.
            (
                SectionState::Connecting,
                vec![ADD_ID, CONNECT_SEED_ID, "host:connect:h"],
            ),
        ];
        for (state, mut expected) in cases {
            expected.push(NEW_PROJECT_ON_ID);
            // The switch row is last in every frame (plan 063 §D8) and
            // is not a per-host verb, so it rides every row of the
            // table rather than any one of them.
            expected.push(USE_SESSION_ID);
            assert_eq!(
                ids(&verbs(
                    &[host("h", state)],
                    NO_RECENTS,
                    IN_PROCESS,
                    FULL,
                    false
                )),
                expected,
                "{state:?}"
            );
        }
    }

    /// Remove on a *connected* host (plan 063 §D7), stated as the
    /// invariant rather than as a row list.
    ///
    /// This reverses the pre-063 rule ("you cannot Remove a host you are
    /// still connected to"): `host_remove_requested` has always
    /// disconnected first and never stopped the session, so the refusal
    /// only forced a two-step. Mid-dial is still withheld — that one
    /// would race the attempt it is removing.
    #[test]
    fn remove_is_offered_in_every_settled_state_including_connected() {
        for state in [
            SectionState::Connected,
            SectionState::Connecting,
            SectionState::Disconnected,
            SectionState::NeedsRestart,
            SectionState::Stopped,
        ] {
            let items = verbs(&[host("h", state)], NO_RECENTS, IN_PROCESS, FULL, false);
            let has = |prefix: &str| items.iter().any(|item| item.id.starts_with(prefix));
            assert_eq!(
                has(REMOVE_PREFIX),
                state != SectionState::Connecting,
                "{state:?} remove"
            );
            assert_eq!(has(STOP_PREFIX), is_connected(state), "{state:?} stop");
        }
        // The subtitle is the promise the reversal rests on, so it is
        // pinned rather than left to the row's presence.
        let connected = verbs(
            &[host("h", SectionState::Connected)],
            NO_RECENTS,
            IN_PROCESS,
            FULL,
            false,
        );
        assert_eq!(
            connected
                .iter()
                .find(|item| item.id.starts_with(REMOVE_PREFIX))
                .and_then(|item| item.subtitle.as_deref()),
            Some("disconnects and forgets it; never stops the session")
        );
    }

    /// "You cannot forget this machine" (plan 063 §D7): under `session`
    /// the slot is the local backend, so Remove is withheld on it — and
    /// on it alone. The identical registry under `in-process` keeps the
    /// verb, because there the same host is an ordinary LOCALHOST band
    /// beside a local workspace that does not depend on it.
    #[test]
    fn remove_is_withheld_on_the_slot_under_session_only() {
        let hosts = [
            localhost_host("slot", SectionState::Connected),
            host("box", SectionState::Connected),
        ];
        let session_items = verbs(&hosts, NO_RECENTS, session("slot"), FULL, false);
        let under_session = ids(&session_items);
        assert!(
            !under_session.contains(&"host:remove:slot"),
            "{under_session:?}"
        );
        assert!(
            under_session.contains(&"host:remove:box"),
            "every other host keeps it: {under_session:?}"
        );

        let in_process_items = verbs(
            &hosts,
            NO_RECENTS,
            LocalSlot {
                mode: LocalBackendMode::InProcess,
                slot_saved_id: Some("slot"),
            },
            FULL,
            false,
        );
        let under_in_process = ids(&in_process_items);
        assert!(
            under_in_process.contains(&"host:remove:slot"),
            "{under_in_process:?}"
        );
    }

    /// What withholding the surface costs, tested on whatever OS this
    /// runs on — which is the reason the policy is a value rather than a
    /// `cfg!`. A localhost host loses its connection verbs; a
    /// socket-path host (the `ssh -L` case) keeps every one of them.
    #[test]
    fn a_gated_policy_hides_the_localhost_surface_and_nothing_else() {
        let local = HostRow {
            saved_id: "h1",
            label: "localhost",
            target: "localhost",
            state: SectionState::Disconnected,
            transport: HostTransportKind::Localhost,
            fidelity: None,
        };
        let remote = host("h2", SectionState::Disconnected);

        let gated = verbs(&[local, remote], NO_RECENTS, IN_PROCESS, GATED, false);
        assert!(
            !ids(&gated).contains(&"host:connect:h1"),
            "a client without the surface must not offer a session it cannot reach"
        );
        assert!(
            ids(&gated).contains(&"host:remove:h1"),
            "but it is still a saved row the user can forget"
        );
        assert!(ids(&gated).contains(&"host:connect:h2"));
        assert!(
            ids(&gated).contains(&ADD_ID),
            "Add Host is the reach-another-box payoff case, never gated"
        );

        // Same inputs, the shipping answer: the localhost host is
        // ordinary.
        assert!(ids(&verbs(
            &[local, remote],
            NO_RECENTS,
            IN_PROCESS,
            FULL,
            false
        ))
        .contains(&"host:connect:h1"));
    }

    /// A connected localhost host under a withholding policy: the gate
    /// applies to leaving too, so such a build lists no connection verbs
    /// at all for it. Launch auto-reconnect reads the same policy so it
    /// never *creates* that state on its own (`app.rs`'s
    /// `reconnect_mode`).
    ///
    /// It is still reachable by hand, and deliberately so: dialing a
    /// session that is already listening was never gated (D11), so the
    /// sidebar's inline ↻ and `roostctl host connect` can still attach
    /// one through `spawn_gate`'s `IfPresent` downgrade. Whichever build
    /// ever ships `localhost_surface: false` has to gate those two
    /// routes as well, or a user can attach with no verb to leave —
    /// noted where the rule lives rather than left to be rediscovered.
    #[test]
    fn the_gate_covers_both_directions() {
        let connected = HostRow {
            saved_id: "h1",
            label: "localhost",
            target: "localhost",
            state: SectionState::Connected,
            transport: HostTransportKind::Localhost,
            // Reduced fidelity is a connection verb too: a build that
            // will not offer to leave a local session must not offer to
            // restart one either.
            fidelity: Some(FidelityAction::Restart),
        };
        let offered = verbs(&[connected], NO_RECENTS, IN_PROCESS, GATED, false);
        let items = ids(&offered);
        assert!(!items.contains(&"host:disconnect:h1"));
        assert!(!items.contains(&"host:stop:h1"));
        assert!(!items.contains(&"host:restart:h1"));
    }

    /// The picker row appears once the picker is a choice, and stays
    /// away when it would be ⌘N with an extra keystroke (plan 063 §D3).
    ///
    /// The one-row cases are the two where the surface is withheld: no
    /// `localhost` row, so `in-process` is alone and `session` has only
    /// the row it cannot offer.
    #[test]
    fn the_picker_row_appears_only_when_the_picker_offers_a_choice() {
        assert!(
            ids(&verbs(&[], NO_RECENTS, IN_PROCESS, FULL, false)).contains(&NEW_PROJECT_ON_ID),
            "LOCAL plus localhost is already two destinations"
        );
        assert!(
            !ids(&verbs(&[], NO_RECENTS, IN_PROCESS, GATED, false)).contains(&NEW_PROJECT_ON_ID),
            "without the surface a fresh registry has LOCAL alone"
        );
        assert!(
            !ids(&verbs(
                &[],
                NO_RECENTS,
                session("nothing-saved"),
                FULL,
                false
            ))
            .contains(&NEW_PROJECT_ON_ID),
            "session mode with only the localhost row is not a choice"
        );
        assert!(ids(&verbs(
            &[host("h", SectionState::Connected)],
            NO_RECENTS,
            session("nothing-saved"),
            FULL,
            false
        ))
        .contains(&NEW_PROJECT_ON_ID));
    }

    /// The picker lists local, then `localhost`, then connected hosts. A
    /// disconnected *ordinary* host is absent rather than disabled —
    /// creating on a session nothing is attached to cannot work.
    #[test]
    fn the_picker_lists_local_localhost_and_connected_hosts() {
        let hosts = [
            host("live", SectionState::Connected),
            host("down", SectionState::Disconnected),
            host("dialing", SectionState::Connecting),
        ];
        let targets = create_targets(&hosts, NO_RECENTS, IN_PROCESS, FULL, "Local");
        assert_eq!(
            ids(&targets),
            vec![
                CREATE_ON_LOCAL_ID,
                CREATE_ON_LOCALHOST_ID,
                "host:create_on:live"
            ]
        );
        assert_eq!(targets[0].title, "Local");
    }

    /// `localhost` is always a row, and its subtitle is what tells the
    /// three cases apart (plan 063 §D3). Connected is the one with no
    /// subtitle: it is the ordinary create-on-a-host row, under the
    /// saved host's own label and its own id, so pressing it is exactly
    /// what pressing any other connected row is.
    #[test]
    fn the_localhost_row_says_which_of_the_three_things_it_will_do() {
        // The row after LOCAL, which
        // `the_picker_lists_local_localhost_and_connected_hosts` pins as
        // the localhost one.
        let row = |hosts: &[HostRow<'_>], local: LocalSlot<'_>| {
            create_targets(hosts, NO_RECENTS, local, FULL, "Local")
                .into_iter()
                .nth(1)
                .expect("the localhost row is always offered")
        };

        let not_saved = row(&[], IN_PROCESS);
        assert_eq!(not_saved.id, CREATE_ON_LOCALHOST_ID);
        assert_eq!(not_saved.title, SEED_LABEL);
        assert_eq!(
            not_saved.subtitle.as_deref(),
            Some("saves localhost, starts it if needed, then creates")
        );

        let saved_slot = LocalSlot {
            mode: LocalBackendMode::InProcess,
            slot_saved_id: Some("mine"),
        };
        let down = row(
            &[localhost_host("mine", SectionState::Disconnected)],
            saved_slot,
        );
        assert_eq!(down.id, CREATE_ON_LOCALHOST_ID);
        assert_eq!(
            down.subtitle.as_deref(),
            Some("starts it if needed, then creates")
        );

        let up = row(
            &[localhost_host("mine", SectionState::Connected)],
            saved_slot,
        );
        assert_eq!(up.id, "host:create_on:mine");
        assert_eq!(up.subtitle, None);
    }

    /// Under `session` there is no in-process workspace on screen, so
    /// the picker has no row that would create in one — and the slot is
    /// listed once, not twice.
    #[test]
    fn the_session_picker_drops_the_local_row_and_lists_the_slot_once() {
        let hosts = [
            localhost_host("slot", SectionState::Connected),
            host("box", SectionState::Connected),
        ];
        assert_eq!(
            ids(&create_targets(
                &hosts,
                NO_RECENTS,
                session("slot"),
                FULL,
                "Local"
            )),
            vec!["host:create_on:slot", "host:create_on:box"]
        );
    }

    /// Plan 056 §3.4's matrix, the palette's column — driven through the
    /// same derivation the band's pill reads, so the two can never
    /// disagree about what a host offers.
    #[test]
    fn the_fidelity_matrix_lists_exactly_the_verb_its_transport_can_act_on() {
        use HostTransportKind::{Localhost, Socket, Ssh};
        // Only the fidelity verb varies here; the surrounding rows are
        // the state matrix's, asserted there. The seed row rides the
        // transport (a Localhost host is the local session already
        // saved), which is why it is spliced rather than hardcoded.
        let expect = |transport: HostTransportKind, rest: Vec<&'static str>| {
            let mut out = vec![ADD_ID];
            if !transport.localhost() {
                out.push(CONNECT_SEED_ID);
            }
            out.extend(rest);
            out.push(NEW_PROJECT_ON_ID);
            out.push(USE_SESSION_ID);
            out
        };
        let connected = vec!["host:disconnect:h", "host:stop:h", "host:remove:h"];
        let cases = [
            (
                Ssh,
                SectionState::Connected,
                true,
                vec![
                    "host:disconnect:h",
                    "host:stop:h",
                    "host:update:h",
                    "host:remove:h",
                ],
            ),
            (
                Localhost,
                SectionState::Connected,
                true,
                vec![
                    "host:disconnect:h",
                    "host:stop:h",
                    "host:restart:h",
                    "host:remove:h",
                ],
            ),
            // A socket target names somebody else's process, and there
            // is no transport this client could reach its binary over.
            (Socket, SectionState::Connected, true, connected.clone()),
            // Connected at exact fidelity: the ordinary two verbs.
            (Ssh, SectionState::Connected, false, connected.clone()),
            (Localhost, SectionState::Connected, false, connected.clone()),
            (Socket, SectionState::Connected, false, connected),
            // Reduced and then dropped: the existing not-connected
            // verbs, and neither fidelity verb — nothing live to act on.
            (
                Ssh,
                SectionState::Disconnected,
                true,
                vec!["host:connect:h", "host:remove:h"],
            ),
            (
                Socket,
                SectionState::Connecting,
                true,
                vec!["host:connect:h"],
            ),
        ];
        for (transport, state, reduced_fidelity, rest) in cases {
            let row = HostRow {
                transport,
                fidelity: fidelity_action(reduced_fidelity, transport, state),
                ..host("h", state)
            };
            assert_eq!(
                ids(&verbs(&[row], NO_RECENTS, IN_PROCESS, FULL, false)),
                expect(transport, rest),
                "{transport:?} {state:?} reduced={reduced_fidelity}"
            );
        }
    }

    /// The two new verbs read the same "is it attached?" gate every
    /// other connection verb does, so a row that arrives claiming an
    /// action its state cannot support still offers nothing to press.
    #[test]
    fn a_fidelity_action_on_a_host_that_is_not_connected_offers_nothing() {
        for state in [
            SectionState::Connecting,
            SectionState::Disconnected,
            SectionState::NeedsRestart,
            SectionState::Stopped,
        ] {
            for action in [
                FidelityAction::Update,
                FidelityAction::Restart,
                FidelityAction::Manual,
            ] {
                let row = HostRow {
                    fidelity: Some(action),
                    ..host("h", state)
                };
                let offered = verbs(&[row], NO_RECENTS, IN_PROCESS, FULL, false);
                let items = ids(&offered);
                assert!(
                    !items
                        .iter()
                        .any(|id| id.starts_with(UPDATE_PREFIX) || id.starts_with(RESTART_PREFIX)),
                    "{state:?} {action:?}: {items:?}"
                );
            }
        }
    }

    /// The titles the matrix names, verbatim — the palette row is one of
    /// three entry points into the same action and they say the same
    /// thing.
    #[test]
    fn the_fidelity_verbs_name_the_host_they_act_on() {
        let title = |transport| {
            verbs(
                &[HostRow {
                    label: "pop-os",
                    transport,
                    fidelity: fidelity_action(true, transport, SectionState::Connected),
                    ..host("h", SectionState::Connected)
                }],
                NO_RECENTS,
                IN_PROCESS,
                FULL,
                false,
            )
            .into_iter()
            .find(|item| item.id.starts_with(UPDATE_PREFIX) || item.id.starts_with(RESTART_PREFIX))
            .map(|item| item.title)
        };
        assert_eq!(
            title(HostTransportKind::Ssh).as_deref(),
            Some("Update roost-session on pop-os")
        );
        assert_eq!(
            title(HostTransportKind::Localhost).as_deref(),
            Some("Restart session on pop-os")
        );
        assert_eq!(title(HostTransportKind::Socket), None);
    }

    /// Plan 063 §D8's table: the row offered is the one naming the
    /// backend this client is **not** on, and a switch in flight offers
    /// neither.
    ///
    /// The pairs are asserted with only the mode moving, then only the
    /// latch moving — a table that varied both at once would pass for a
    /// builder that read neither.
    #[test]
    fn exactly_the_switch_row_for_the_other_backend_is_offered() {
        let row = |local, switching| {
            ids(&verbs(&[], NO_RECENTS, local, FULL, switching))
                .into_iter()
                .find(|id| {
                    parse(id).is_some_and(|verb| {
                        matches!(verb, HostVerb::UseSession | HostVerb::UseInProcess)
                    })
                })
                .map(str::to_string)
        };
        assert_eq!(
            row(IN_PROCESS, false).as_deref(),
            Some(USE_SESSION_ID),
            "in-process offers the way onto a session"
        );
        assert_eq!(
            row(session("mine"), false).as_deref(),
            Some(USE_IN_PROCESS_ID),
            "session offers the way back"
        );
        // The same two inputs with the latch set — the one column that
        // moved.
        assert_eq!(row(IN_PROCESS, true), None);
        assert_eq!(row(session("mine"), true), None);

        // A build that cannot reach a session on this machine offers
        // neither direction: both are about one.
        assert_eq!(
            ids(&verbs(&[], NO_RECENTS, IN_PROCESS, GATED, false)),
            vec![ADD_ID]
        );
    }

    /// The copy is the contract with the user, and both lines say the
    /// irreversible part out loud (plan 063 §D8's table).
    #[test]
    fn each_switch_row_says_what_it_ends_and_what_it_keeps() {
        let subtitle = |local| {
            verbs(&[], NO_RECENTS, local, FULL, false)
                .into_iter()
                .find(|item| item.id == USE_SESSION_ID || item.id == USE_IN_PROCESS_ID)
                .and_then(|item| item.subtitle)
                .expect("a switch row")
        };
        assert_eq!(
            subtitle(IN_PROCESS),
            "running local shells end; layout moves; survives quit"
        );
        assert_eq!(
            subtitle(session("mine")),
            "session keeps running as LOCALHOST; local tabs start fresh"
        );
    }

    /// Plan 063 §D7's recents, in both surfaces: beside `Add Host…` in
    /// the command frame, and last in the creation picker.
    #[test]
    fn a_forgotten_host_is_offered_back_in_both_surfaces() {
        let recents = [recent("old-box", "user@old-box")];
        let items = verbs(&[], &recents, IN_PROCESS, FULL, false);
        assert_eq!(
            ids(&items),
            vec![
                ADD_ID,
                "host:recent:user@old-box",
                CONNECT_SEED_ID,
                NEW_PROJECT_ON_ID,
                USE_SESSION_ID
            ],
            "the recent sits with Add Host, which is the gesture it saves"
        );
        let row = items
            .iter()
            .find(|item| item.id.starts_with(RECENT_PREFIX))
            .expect("the recent row");
        assert_eq!(row.title, "Add Host: old-box");
        assert_eq!(row.subtitle.as_deref(), Some("saves it again and connects"));

        let targets = create_targets(
            &[host("live", SectionState::Connected)],
            &recents,
            IN_PROCESS,
            FULL,
            "Local",
        );
        assert_eq!(
            ids(&targets),
            vec![
                CREATE_ON_LOCAL_ID,
                CREATE_ON_LOCALHOST_ID,
                "host:create_on:live",
                "host:create_on_recent:user@old-box"
            ],
            "and last in the picker: it is two round trips from being a destination"
        );
        assert_eq!(targets[3].title, "old-box");
        assert_eq!(
            targets[3].subtitle.as_deref(),
            Some("saves it again, connects, then creates")
        );
    }

    /// A recent whose target is saved again is not offered: the row
    /// would fail on the duplicate label, and it would name a band that
    /// is already on screen. Nothing prunes the recents list on an add,
    /// so this filter is what keeps the two from disagreeing.
    #[test]
    fn a_recent_that_is_saved_again_is_not_offered_back() {
        let recents = [recent("old-box", "user@old-box"), recent("shed", "shed")];
        let saved = HostRow {
            target: "user@old-box",
            ..host("back", SectionState::Connected)
        };

        let offered_items = verbs(&[saved], &recents, IN_PROCESS, FULL, false);
        let offered = ids(&offered_items);
        assert!(
            !offered.contains(&"host:recent:user@old-box"),
            "the saved one is gone: {offered:?}"
        );
        assert!(
            offered.contains(&"host:recent:shed"),
            "the one still forgotten stays: {offered:?}"
        );

        let picker_items = create_targets(&[saved], &recents, IN_PROCESS, FULL, "Local");
        let picker = ids(&picker_items);
        assert!(!picker.contains(&"host:create_on_recent:user@old-box"));
        assert!(picker.contains(&"host:create_on_recent:shed"));

        // The control: with that host not saved, the row is there.
        let control = verbs(&[], &recents, IN_PROCESS, FULL, false);
        assert!(ids(&control).contains(&"host:recent:user@old-box"));
    }

    /// A target with colons in it — an `ssh -L` socket path, a
    /// `host:port` — round-trips through the row id, which is why the
    /// remainder of the id is taken whole rather than split.
    #[test]
    fn a_recents_row_id_round_trips_a_target_with_colons() {
        let target = "/run/user/1000/roost:2/roost.sock";
        let recents = [recent("forwarded", target)];
        for item in verbs(&[], &recents, IN_PROCESS, FULL, false)
            .into_iter()
            .chain(create_targets(&[], &recents, IN_PROCESS, FULL, "Local"))
            .filter(|item| item.id.contains("recent"))
        {
            let create = item.id.starts_with(CREATE_ON_RECENT_PREFIX);
            assert_eq!(
                parse(&item.id),
                Some(HostVerb::AddRecent {
                    target: target.to_string(),
                    create
                }),
                "{}",
                item.id
            );
        }
    }

    /// Every id the builders emit parses back to the verb that produced
    /// it — the round trip the adapter's activation depends on.
    #[test]
    fn every_emitted_id_round_trips_through_parse() {
        let hosts = [
            host("live", SectionState::Connected),
            host("down", SectionState::Disconnected),
            HostRow {
                transport: HostTransportKind::Ssh,
                fidelity: Some(FidelityAction::Update),
                ..host("skewed", SectionState::Connected)
            },
            HostRow {
                transport: HostTransportKind::Localhost,
                fidelity: Some(FidelityAction::Restart),
                ..host("mine", SectionState::Connected)
            },
        ];
        let mut items = verbs(&hosts, NO_RECENTS, IN_PROCESS, FULL, false);
        items.extend(verbs(&[], NO_RECENTS, IN_PROCESS, FULL, false));
        items.extend(verbs(&hosts, NO_RECENTS, session("mine"), FULL, false));
        items.extend(create_targets(
            &hosts, NO_RECENTS, IN_PROCESS, FULL, "Local",
        ));
        // The registry with no localhost host is what emits the picker's
        // not-yet-saved `localhost` row.
        items.extend(create_targets(&[], NO_RECENTS, IN_PROCESS, FULL, "Local"));
        let recents = [recent("old-box", "user@old-box")];
        items.extend(verbs(&hosts, &recents, IN_PROCESS, FULL, false));
        items.extend(create_targets(&hosts, &recents, IN_PROCESS, FULL, "Local"));
        for item in &items {
            assert!(parse(&item.id).is_some(), "{} does not parse", item.id);
        }

        assert_eq!(parse(ADD_ID), Some(HostVerb::Add));
        assert_eq!(parse(CONNECT_SEED_ID), Some(HostVerb::ConnectSeed));
        assert_eq!(parse(USE_SESSION_ID), Some(HostVerb::UseSession));
        assert_eq!(parse(USE_IN_PROCESS_ID), Some(HostVerb::UseInProcess));
        assert_eq!(
            parse("host:connect:abc"),
            Some(HostVerb::Connect("abc".into()))
        );
        assert_eq!(parse("host:stop:abc"), Some(HostVerb::Stop("abc".into())));
        assert_eq!(
            parse("host:update:abc"),
            Some(HostVerb::Update("abc".into()))
        );
        assert_eq!(
            parse("host:restart:abc"),
            Some(HostVerb::Restart("abc".into()))
        );
        assert_eq!(
            parse("host:remove:abc"),
            Some(HostVerb::Remove("abc".into()))
        );
        assert_eq!(parse(CREATE_ON_LOCAL_ID), Some(HostVerb::CreateOn(None)));
        assert_eq!(
            parse(CREATE_ON_LOCALHOST_ID),
            Some(HostVerb::CreateOnLocalhost)
        );
        assert_eq!(
            parse("host:create_on:abc"),
            Some(HostVerb::CreateOn(Some("abc".into())))
        );
        assert_eq!(
            parse("host:recent:user@box"),
            Some(HostVerb::AddRecent {
                target: "user@box".into(),
                create: false
            })
        );
        assert_eq!(
            parse("host:create_on_recent:user@box"),
            Some(HostVerb::AddRecent {
                target: "user@box".into(),
                create: true
            })
        );
        // Not a host row: the command frame's own ids must fall through
        // so `run_palette_row` keeps handling them.
        assert_eq!(parse("new_project"), None);
        assert_eq!(parse("hosts"), None);
    }
}
