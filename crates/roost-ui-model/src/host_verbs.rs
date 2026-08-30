//! The palette's host-session verb family, and the one platform gate
//! that decides which of them a build offers (plan 037 §3.1/§3.5).
//!
//! Every host action lives in the command palette — there is no host
//! menu and no per-section button beyond the inline ↻ Reconnect. One
//! item per (verb, host) pair, so fuzzy-matching "disc pop" reaches
//! exactly one row, and **verbs appear only when they apply**: you
//! cannot Stop a session you are not attached to, and you cannot Remove
//! a host you are still connected to.
//!
//! Pure and toolkit-free, which is the point: the macOS gate below is a
//! field on [`VerbPolicy`] rather than a `cfg!` sprinkled through the
//! adapter, so the Mac policy is a unit test that runs on Linux and the
//! Linux policy is a unit test that runs on a Mac.

use crate::host_sidebar::SectionState;

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

/// The id of the seeded-localhost Connect row — the one verb addressed
/// to a host that is not saved yet (plan 037 §3.5). Activating it saves
/// `localhost` and connects in one step, so a fresh Linux install
/// reaches its own session without an Add Host detour.
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
    /// Whether this host's target is this machine's own session. Only
    /// this flag is gated on macOS; a host reached over an `ssh -L`
    /// forward keeps its full verb set there.
    pub localhost: bool,
}

/// The platform policy, as one value.
///
/// `localhost_surface` is plan 037 §3.1's Mac gate: macOS packages no
/// `roost-session`, so offering to connect to one would be a visible
/// dead end (the roadmap's no-dead-end rule). `Add Host` is deliberately
/// *not* gated — pointing a Mac at an `ssh -L` forward to a Linux box is
/// the whole Mac→Linux payoff case, not a dead end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerbPolicy {
    pub localhost_surface: bool,
}

impl VerbPolicy {
    /// What this build ships with. The single `cfg!` in the host-verb
    /// path; everything else takes the answer as a value.
    pub fn current() -> Self {
        Self {
            localhost_surface: !cfg!(target_os = "macos"),
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
/// * The seeded `localhost` row appears only on a fresh registry, and
///   only where this client could talk to a local session at all.
/// * `New Project on…` appears once there is somewhere else to create,
///   because with no hosts it is exactly `new_project`.
pub fn verbs(hosts: &[HostRow<'_>], policy: VerbPolicy) -> Vec<VerbItem> {
    let mut items = vec![VerbItem::new(
        ADD_ID,
        "Add Host…",
        "register a session socket",
    )];

    if hosts.is_empty() && policy.localhost_surface {
        items.push(VerbItem::new(
            CONNECT_SEED_ID,
            format!("Connect Host: {SEED_LABEL}"),
            "starts it if needed",
        ));
    }

    for host in hosts {
        // The Mac gate, applied at exactly one place: a localhost host
        // on macOS has no session to reach, so it offers no connection
        // verbs at all. It still lists in the sidebar and can still be
        // removed, which is the only honest thing left to do with it.
        let reachable = policy.localhost_surface || !host.localhost;
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

    fn host(saved_id: &str, state: SectionState) -> HostRow<'_> {
        HostRow {
            saved_id,
            label: saved_id,
            state,
            localhost: false,
        }
    }

    fn ids(items: &[VerbItem]) -> Vec<&str> {
        items.iter().map(|item| item.id.as_str()).collect()
    }

    const LINUX: VerbPolicy = VerbPolicy {
        localhost_surface: true,
    };
    const MAC: VerbPolicy = VerbPolicy {
        localhost_surface: false,
    };

    /// The zero-host baseline, both platforms. Linux seeds a localhost
    /// Connect so a fresh install can reach its own session; macOS has
    /// no session to reach and offers only Add Host — which is a real
    /// destination there, not a dead end.
    #[test]
    fn a_fresh_registry_offers_add_everywhere_and_the_seed_only_on_linux() {
        assert_eq!(ids(&verbs(&[], LINUX)), vec![ADD_ID, CONNECT_SEED_ID]);
        assert_eq!(ids(&verbs(&[], MAC)), vec![ADD_ID]);
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
                ids(&verbs(&[host("h", state)], LINUX)),
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
            let items = verbs(&[host("h", state)], LINUX);
            let has = |prefix: &str| items.iter().any(|item| item.id.starts_with(prefix));
            assert!(
                !(has(STOP_PREFIX) && has(REMOVE_PREFIX)),
                "{state:?} offers both Stop and Remove"
            );
            assert_eq!(has(STOP_PREFIX), is_connected(state), "{state:?} stop");
        }
    }

    /// The Mac gate, tested on whatever OS this runs on — which is the
    /// reason the policy is a value. A localhost host loses its
    /// connection verbs there; a socket-path host (the `ssh -L` case)
    /// keeps every one of them.
    #[test]
    fn macos_hides_the_localhost_surface_and_nothing_else() {
        let local = HostRow {
            saved_id: "h1",
            label: "localhost",
            state: SectionState::Disconnected,
            localhost: true,
        };
        let remote = host("h2", SectionState::Disconnected);

        let mac = verbs(&[local, remote], MAC);
        assert!(
            !ids(&mac).contains(&"host:connect:h1"),
            "macOS must not offer to connect a localhost session it cannot run"
        );
        assert!(
            ids(&mac).contains(&"host:remove:h1"),
            "but it is still a saved row the user can forget"
        );
        assert!(ids(&mac).contains(&"host:connect:h2"));
        assert!(
            ids(&mac).contains(&ADD_ID),
            "Add Host is the Mac→Linux payoff case, never gated"
        );

        // Same inputs, Linux policy: the localhost host is ordinary.
        assert!(ids(&verbs(&[local, remote], LINUX)).contains(&"host:connect:h1"));
    }

    /// A connected localhost host on macOS: the gate applies to leaving
    /// too, or a Mac build could hold a connection it has no verb to
    /// drop. (It cannot get one — no launch auto-reconnect, no Connect —
    /// but the rule is stated where it is enforced, not inferred.)
    #[test]
    fn the_gate_covers_both_directions() {
        let connected = HostRow {
            saved_id: "h1",
            label: "localhost",
            state: SectionState::Connected,
            localhost: true,
        };
        let offered = verbs(&[connected], MAC);
        let items = ids(&offered);
        assert!(!items.contains(&"host:disconnect:h1"));
        assert!(!items.contains(&"host:stop:h1"));
    }

    /// Once a host exists, the picker row does too — and with none it
    /// stays away, because "New Project on…" with a single LOCAL row is
    /// ⌘N with an extra keystroke.
    #[test]
    fn the_picker_row_appears_only_once_there_is_somewhere_else_to_create() {
        assert!(!ids(&verbs(&[], LINUX)).contains(&NEW_PROJECT_ON_ID));
        assert!(ids(&verbs(&[host("h", SectionState::Disconnected)], LINUX))
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

    /// Every id the builders emit parses back to the verb that produced
    /// it — the round trip the adapter's activation depends on.
    #[test]
    fn every_emitted_id_round_trips_through_parse() {
        let hosts = [
            host("live", SectionState::Connected),
            host("down", SectionState::Disconnected),
        ];
        let mut items = verbs(&hosts, LINUX);
        items.extend(verbs(&[], LINUX));
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
