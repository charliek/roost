//! The palette's host-session verb family, and the one policy seam that
//! decides which of them a build offers (plan 037 §3.1/§3.5, plan 041
//! §3.1).
//!
//! Every host action lives in the command palette — there is no host
//! menu and no per-section button beyond the inline ↻ Reconnect. One
//! item per (verb, host) pair, so fuzzy-matching "disc pop" reaches
//! exactly one row, and **verbs appear only when they apply**: you
//! cannot Stop a session you are not attached to, and you cannot Remove
//! a host you are still connected to.
//!
//! Every shipping build offers the localhost surface. The withheld
//! answer stays expressible as a [`VerbPolicy`] value rather than a
//! `cfg!` sprinkled through the adapter, so a build that genuinely
//! cannot reach a local session could hide the whole surface coherently
//! — and both answers stay unit tests that run on any OS.

use crate::host_sidebar::{FidelityAction, HostTransportKind, SectionState};

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

/// One saved host, as the verb builder reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostRow<'a> {
    /// `HostSnapshot.id` — what every verb is addressed to.
    pub saved_id: &'a str,
    pub label: &'a str,
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
/// not connected: an attempt in flight holds no lease, so Stop and
/// Disconnect have nothing to act on yet.
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
/// * `Connect` / `Disconnect` / `Stop` / `Remove` — one per host, gated
///   on whether that host is attached. A connected host offers the two
///   ways to leave it (disconnect keeps its shells, stop ends them); a
///   host that is not offers the way back in, plus Remove once it has
///   settled — removing a host mid-dial would race its own connection.
/// * `Update` / `Restart` — only on a connected host whose `fidelity`
///   names one of them, which is plan 056 §3.4's matrix: an ssh host is
///   sent a matching build, this machine's own session is restarted,
///   and a socket target lists neither because nothing here can reach
///   its binary.
/// * The seeded `localhost` row appears only on a fresh registry, and
///   only where the policy offers the localhost surface at all.
/// * `New Project on…` appears once there is somewhere else to create,
///   because with no hosts it is exactly `new_project`.
pub fn verbs(hosts: &[HostRow<'_>], policy: VerbPolicy) -> Vec<VerbItem> {
    let mut items = vec![VerbItem::new(
        ADD_ID,
        "Add Host…",
        "point Roost at an SSH host or a session socket",
    )];

    if hosts.is_empty() && policy.localhost_surface {
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
            continue;
        }
        if reachable {
            items.push(VerbItem::new(
                format!("{CONNECT_PREFIX}{}", host.saved_id),
                format!("Connect Host: {}", host.label),
                connect_subtitle(host.state),
            ));
        }
        if host.state != SectionState::Connecting {
            items.push(VerbItem::new(
                format!("{REMOVE_PREFIX}{}", host.saved_id),
                format!("Remove Host: {}", host.label),
                "forgets it; never stops the session",
            ));
        }
    }

    if !hosts.is_empty() {
        items.push(VerbItem::new(
            NEW_PROJECT_ON_ID,
            "New Project on…",
            "pick the host to create on",
        ));
    }
    items
}

/// What Connect means from where the host currently is. `NeedsRestart`
/// is the one that has to say something different: connecting again is
/// how the upgrade dialog gets raised, not a plain retry.
fn connect_subtitle(state: SectionState) -> &'static str {
    match state {
        SectionState::NeedsRestart => "build mismatch — offers a restart",
        SectionState::TakenOver => "take the session back",
        SectionState::Stopped => "starts a fresh session",
        _ => "starts it if needed",
    }
}

/// The "New Project on…" picker's rows: the local workspace, then every
/// **connected** host.
///
/// Disconnected hosts are absent rather than disabled, which follows the
/// sidebar's own rule (§3.1: dimmed rows are non-interactive) — you
/// cannot create on a session nothing is attached to, and a row that
/// refuses is worse than a row that is not there.
pub fn create_targets(hosts: &[HostRow<'_>], local_label: &str) -> Vec<VerbItem> {
    let mut items = vec![VerbItem {
        id: CREATE_ON_LOCAL_ID.to_string(),
        title: local_label.to_string(),
        subtitle: None,
    }];
    items.extend(
        hosts
            .iter()
            .filter(|host| is_connected(host.state))
            .map(|host| VerbItem {
                id: format!("{CREATE_ON_PREFIX}{}", host.saved_id),
                title: host.label.to_string(),
                subtitle: None,
            }),
    );
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_sidebar::fidelity_action;

    fn host(saved_id: &str, state: SectionState) -> HostRow<'_> {
        HostRow {
            saved_id,
            label: saved_id,
            state,
            transport: HostTransportKind::Ssh,
            fidelity: None,
        }
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
    #[test]
    fn a_fresh_registry_offers_add_always_and_the_seed_only_with_the_surface() {
        assert_eq!(ids(&verbs(&[], FULL)), vec![ADD_ID, CONNECT_SEED_ID]);
        assert_eq!(ids(&verbs(&[], GATED)), vec![ADD_ID]);
    }

    /// The availability matrix, one host at a time. Connected offers the
    /// two ways out; everything else offers the way back in.
    #[test]
    fn each_state_offers_exactly_the_verbs_that_can_act() {
        let cases = [
            (
                SectionState::Connected,
                vec![ADD_ID, "host:disconnect:h", "host:stop:h"],
            ),
            (
                SectionState::Disconnected,
                vec![ADD_ID, "host:connect:h", "host:remove:h"],
            ),
            (
                SectionState::NeedsRestart,
                vec![ADD_ID, "host:connect:h", "host:remove:h"],
            ),
            (
                SectionState::TakenOver,
                vec![ADD_ID, "host:connect:h", "host:remove:h"],
            ),
            (
                SectionState::Stopped,
                vec![ADD_ID, "host:connect:h", "host:remove:h"],
            ),
            // Mid-dial: reconnecting is fine (it supersedes), removing
            // would race the attempt it is removing.
            (SectionState::Connecting, vec![ADD_ID, "host:connect:h"]),
        ];
        for (state, mut expected) in cases {
            expected.push(NEW_PROJECT_ON_ID);
            assert_eq!(
                ids(&verbs(&[host("h", state)], FULL)),
                expected,
                "{state:?}"
            );
        }
    }

    /// Stop is connected-only and Remove is not-connected-only, stated
    /// as the invariant rather than as a row list: the two must never be
    /// offered together, or the palette would let a user remove the
    /// registry entry for a session it is holding a lease on.
    #[test]
    fn stop_and_remove_are_never_offered_at_the_same_time() {
        for state in [
            SectionState::Connected,
            SectionState::Connecting,
            SectionState::Disconnected,
            SectionState::NeedsRestart,
            SectionState::TakenOver,
            SectionState::Stopped,
        ] {
            let items = verbs(&[host("h", state)], FULL);
            let has = |prefix: &str| items.iter().any(|item| item.id.starts_with(prefix));
            assert!(
                !(has(STOP_PREFIX) && has(REMOVE_PREFIX)),
                "{state:?} offers both Stop and Remove"
            );
            assert_eq!(has(STOP_PREFIX), is_connected(state), "{state:?} stop");
        }
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
            state: SectionState::Disconnected,
            transport: HostTransportKind::Localhost,
            fidelity: None,
        };
        let remote = host("h2", SectionState::Disconnected);

        let gated = verbs(&[local, remote], GATED);
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
        assert!(ids(&verbs(&[local, remote], FULL)).contains(&"host:connect:h1"));
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
            state: SectionState::Connected,
            transport: HostTransportKind::Localhost,
            // Reduced fidelity is a connection verb too: a build that
            // will not offer to leave a local session must not offer to
            // restart one either.
            fidelity: Some(FidelityAction::Restart),
        };
        let offered = verbs(&[connected], GATED);
        let items = ids(&offered);
        assert!(!items.contains(&"host:disconnect:h1"));
        assert!(!items.contains(&"host:stop:h1"));
        assert!(!items.contains(&"host:restart:h1"));
    }

    /// Once a host exists, the picker row does too — and with none it
    /// stays away, because "New Project on…" with a single LOCAL row is
    /// ⌘N with an extra keystroke.
    #[test]
    fn the_picker_row_appears_only_once_there_is_somewhere_else_to_create() {
        assert!(!ids(&verbs(&[], FULL)).contains(&NEW_PROJECT_ON_ID));
        assert!(ids(&verbs(&[host("h", SectionState::Disconnected)], FULL))
            .contains(&NEW_PROJECT_ON_ID));
    }

    /// The picker lists local plus connected hosts only. A disconnected
    /// host is absent rather than disabled — creating on a session
    /// nothing is attached to cannot work.
    #[test]
    fn the_picker_lists_local_and_connected_hosts() {
        let hosts = [
            host("live", SectionState::Connected),
            host("down", SectionState::Disconnected),
            host("dialing", SectionState::Connecting),
        ];
        let targets = create_targets(&hosts, "Local");
        assert_eq!(
            ids(&targets),
            vec![CREATE_ON_LOCAL_ID, "host:create_on:live"]
        );
        assert_eq!(targets[0].title, "Local");
    }

    /// Plan 056 §3.4's matrix, the palette's column — driven through the
    /// same derivation the band's pill reads, so the two can never
    /// disagree about what a host offers.
    #[test]
    fn the_fidelity_matrix_lists_exactly_the_verb_its_transport_can_act_on() {
        use HostTransportKind::{Localhost, Socket, Ssh};
        let connected = vec![ADD_ID, "host:disconnect:h", "host:stop:h"];
        let cases = [
            (
                Ssh,
                SectionState::Connected,
                true,
                vec![ADD_ID, "host:disconnect:h", "host:stop:h", "host:update:h"],
            ),
            (
                Localhost,
                SectionState::Connected,
                true,
                vec![ADD_ID, "host:disconnect:h", "host:stop:h", "host:restart:h"],
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
                vec![ADD_ID, "host:connect:h", "host:remove:h"],
            ),
            (
                Localhost,
                SectionState::TakenOver,
                true,
                vec![ADD_ID, "host:connect:h", "host:remove:h"],
            ),
            (
                Socket,
                SectionState::Connecting,
                true,
                vec![ADD_ID, "host:connect:h"],
            ),
        ];
        for (transport, state, reduced_fidelity, mut expected) in cases {
            let row = HostRow {
                transport,
                fidelity: fidelity_action(reduced_fidelity, transport, state),
                ..host("h", state)
            };
            expected.push(NEW_PROJECT_ON_ID);
            assert_eq!(
                ids(&verbs(&[row], FULL)),
                expected,
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
            SectionState::TakenOver,
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
                let offered = verbs(&[row], FULL);
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
                FULL,
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
        let mut items = verbs(&hosts, FULL);
        items.extend(verbs(&[], FULL));
        items.extend(create_targets(&hosts, "Local"));
        for item in &items {
            assert!(parse(&item.id).is_some(), "{} does not parse", item.id);
        }

        assert_eq!(parse(ADD_ID), Some(HostVerb::Add));
        assert_eq!(parse(CONNECT_SEED_ID), Some(HostVerb::ConnectSeed));
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
            parse("host:create_on:abc"),
            Some(HostVerb::CreateOn(Some("abc".into())))
        );
        // Not a host row: the command frame's own ids must fall through
        // so `run_palette_row` keeps handling them.
        assert_eq!(parse("new_project"), None);
        assert_eq!(parse("hosts"), None);
    }
}
