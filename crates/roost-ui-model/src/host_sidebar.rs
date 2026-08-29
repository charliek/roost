//! The sidebar's per-host sections, and the navigation ring that walks
//! them (plan 037 §3.1).
//!
//! With at least one saved host the single "PROJECTS" band becomes one
//! band per host — LOCAL first, then each saved host in registry order —
//! carrying a connection dot and a right-aligned rollup. Everything here
//! is the render-agnostic half of that: which sections exist and in what
//! order, what each header says, whether its rows respond, and which
//! projects the ring visits. The toolkit adapter paints it.
//!
//! **Zero saved hosts is the zero-change baseline** (roadmap D8):
//! [`sections`] answers empty, and the sidebar keeps exactly the chrome
//! it has today.

use crate::keys::{HostId, ProjectKey};

/// The LOCAL band's label. Reserved as a host label too — a saved host
/// may not be called "local" (`Workspace::add_host` rejects it), so the
/// first band is unambiguous.
pub const LOCAL_LABEL: &str = "LOCAL";

/// A section's connection state, reduced to what its header renders.
/// `roost-iced`'s `HostConnState` maps onto this; the local workspace is
/// its own variant because it is connected by construction and has no
/// reconnect story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionState {
    Local,
    Connected,
    Connecting,
    Disconnected,
    NeedsRestart,
    TakenOver,
    Stopped,
}

/// The header's connection dot. Three states, not six: the amber one
/// means "something is in flight or wants your attention", which is the
/// only distinction a 7px dot can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDot {
    Connected,
    Pending,
    Offline,
}

impl SectionState {
    pub fn dot(self) -> HostDot {
        match self {
            Self::Local | Self::Connected => HostDot::Connected,
            Self::Connecting | Self::NeedsRestart => HostDot::Pending,
            Self::Disconnected | Self::TakenOver | Self::Stopped => HostDot::Offline,
        }
    }

    /// Whether this section's rows respond to a click and take part in
    /// keyboard traversal.
    ///
    /// A disconnected host still *lists* its rows — those shells are
    /// still running over there and the sidebar should not pretend
    /// otherwise — but nothing about them is actionable until the
    /// connection is back, so focusing one cannot be attempted.
    pub fn interactive(self) -> bool {
        matches!(self, Self::Local | Self::Connected)
    }

    /// The header's right-aligned word when the section is not simply
    /// connected. `None` leaves the rollup slot to the agent count.
    pub fn status_text(self) -> Option<&'static str> {
        match self {
            Self::Local | Self::Connected => None,
            Self::Connecting => Some("connecting…"),
            Self::Disconnected => Some("disconnected"),
            Self::NeedsRestart => Some("needs restart"),
            Self::TakenOver => Some("taken over"),
            Self::Stopped => Some("session ended"),
        }
    }

    /// Whether an inline "↻ Reconnect" row sits under the section.
    /// Everything that is not connected offers it — including
    /// `NeedsRestart`, where connecting again is how the upgrade dialog
    /// (C8) gets raised.
    pub fn offers_reconnect(self) -> bool {
        !matches!(self, Self::Local | Self::Connected)
    }
}

/// One saved host, as the sidebar assembly reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostInput<'a> {
    /// `HostSnapshot.id` — the stable saved identity, which is what a
    /// reconnect verb is addressed to.
    pub saved_id: &'a str,
    pub label: &'a str,
    /// The connection incarnation currently serving this host, if any.
    /// `HostId::LOCAL` never appears here.
    pub host: HostId,
    pub state: SectionState,
    /// How many agent rows this host contributes, for the rollup.
    pub agents: usize,
}

/// One rendered section header, in sidebar order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub host: HostId,
    /// `None` for the local workspace's band.
    pub saved_id: Option<String>,
    pub label: String,
    pub state: SectionState,
    /// The right-aligned rollup: an agent count for a connected host, or
    /// the state's own word. `None` renders a bare header.
    pub rollup: Option<String>,
}

impl Section {
    pub fn is_local(&self) -> bool {
        self.saved_id.is_none()
    }
}

/// Every section the sidebar draws, LOCAL first and then the saved hosts
/// in registry order.
///
/// **Empty when there are no saved hosts** — the caller keeps its single
/// "PROJECTS" band and changes nothing, which is the acceptance
/// criterion this whole module is gated behind.
pub fn sections(hosts: &[HostInput<'_>]) -> Vec<Section> {
    if hosts.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(hosts.len() + 1);
    out.push(Section {
        host: HostId::LOCAL,
        saved_id: None,
        label: LOCAL_LABEL.to_string(),
        state: SectionState::Local,
        // Deliberately bare: the approved mockup gives LOCAL a dot and
        // nothing else. A rollup answers "what is going on over there",
        // and for the local workspace there is no "over there".
        rollup: None,
    });
    for host in hosts {
        out.push(Section {
            host: host.host,
            saved_id: Some(host.saved_id.to_string()),
            label: section_label(host.label),
            state: host.state,
            rollup: host
                .state
                .status_text()
                .map(str::to_string)
                .or_else(|| agent_rollup(host.agents)),
        });
    }
    out
}

/// A host's band label: the saved label, uppercased so it reads as the
/// same band LOCAL does.
pub fn section_label(label: &str) -> String {
    label.to_uppercase()
}

/// The connected-host rollup. `None` for a host with no agents — there
/// is nothing to roll up, and the mockup leaves that header bare.
pub fn agent_rollup(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 agent".to_string()),
        n => Some(format!("{n} agents")),
    }
}

/// One section's projects, as the navigation ring sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingSection {
    pub host: HostId,
    /// A disconnected section is listed but never traversed.
    pub navigable: bool,
    /// The section's project ids in sidebar order.
    pub projects: Vec<i64>,
}

/// Every project the sidebar lists top to bottom, skipping the sections
/// whose rows are only there for reference.
///
/// One ring across host boundaries is the whole navigation decision
/// (§3.1): a project's host is not a mode the user has to switch into.
pub fn ring(sections: &[RingSection]) -> Vec<ProjectKey> {
    ring_iter(sections).collect()
}

fn ring_iter(sections: &[RingSection]) -> impl Iterator<Item = ProjectKey> + '_ {
    sections
        .iter()
        .filter(|section| section.navigable)
        .flat_map(|section| {
            section
                .projects
                .iter()
                .map(|project| ProjectKey::new(section.host, *project))
        })
}

/// The `index`-th (1-based) project of the ring — what `switch_project_N`
/// resolves against once the sidebar has more than one section.
pub fn ring_index(sections: &[RingSection], index: u8) -> Option<ProjectKey> {
    let index = usize::from(index).checked_sub(1)?;
    ring_iter(sections).nth(index)
}

/// The project `delta` steps from `current` around the ring, wrapping at
/// both ends. `None` when the ring is empty; a `current` the ring does
/// not contain (its section went away, or is disconnected) starts the
/// walk from the top.
///
/// **No caller yet, by design.** The plan's ring rule — "next/prev
/// project walks the sidebar top-to-bottom across host boundaries" —
/// comes with "no new navigation bindings", and Roost binds no next/prev
/// project action today (`switch_project_N`, which [`ring_index`]
/// answers, and the sidebar rows are the whole of project navigation).
/// This is the step the binding would use the day one is added; it is
/// specified and tested here so the ring has one definition rather than
/// two.
pub fn ring_step(
    sections: &[RingSection],
    current: ProjectKey,
    delta: isize,
) -> Option<ProjectKey> {
    let ring = ring(sections);
    if ring.is_empty() {
        return None;
    }
    let len = ring.len() as isize;
    let at = ring
        .iter()
        .position(|project| *project == current)
        .map(|at| at as isize)
        .unwrap_or(0);
    let next = (at + delta).rem_euclid(len) as usize;
    ring.get(next).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(saved_id: &'static str, state: SectionState, agents: usize) -> HostInput<'static> {
        HostInput {
            saved_id,
            label: saved_id,
            host: HostId::new(1),
            state,
            agents,
        }
    }

    /// Acceptance criterion 1: no saved hosts, no sections — the caller
    /// keeps the single "PROJECTS" band it has today.
    #[test]
    fn zero_saved_hosts_is_zero_sections() {
        assert!(sections(&[]).is_empty());
    }

    #[test]
    fn local_leads_and_hosts_follow_in_registry_order() {
        let hosts = [
            host("pop-os", SectionState::Connected, 2),
            host("box", SectionState::Disconnected, 0),
        ];
        let sections = sections(&hosts);
        assert_eq!(
            sections
                .iter()
                .map(|section| section.label.as_str())
                .collect::<Vec<_>>(),
            vec!["LOCAL", "POP-OS", "BOX"]
        );
        assert!(sections[0].is_local());
        assert_eq!(sections[0].host, HostId::LOCAL);
        assert!(sections[1..].iter().all(|section| !section.is_local()));
        assert_eq!(sections[1].saved_id.as_deref(), Some("pop-os"));
    }

    #[test]
    fn the_rollup_is_the_agent_count_while_connected_and_the_state_otherwise() {
        let sections = sections(&[
            host("a", SectionState::Connected, 2),
            host("b", SectionState::Connected, 1),
            host("c", SectionState::Connected, 0),
            host("d", SectionState::Disconnected, 3),
            host("e", SectionState::TakenOver, 0),
        ]);
        let rollups: Vec<Option<&str>> = sections
            .iter()
            .map(|section| section.rollup.as_deref())
            .collect();
        assert_eq!(
            rollups,
            vec![
                None,
                Some("2 agents"),
                Some("1 agent"),
                None,
                // A disconnected host reports its state, not a count it
                // can no longer vouch for.
                Some("disconnected"),
                Some("taken over"),
            ]
        );
    }

    #[test]
    fn the_dot_is_green_connected_amber_in_flight_and_grey_gone() {
        assert_eq!(SectionState::Local.dot(), HostDot::Connected);
        assert_eq!(SectionState::Connected.dot(), HostDot::Connected);
        assert_eq!(SectionState::Connecting.dot(), HostDot::Pending);
        assert_eq!(SectionState::NeedsRestart.dot(), HostDot::Pending);
        assert_eq!(SectionState::Disconnected.dot(), HostDot::Offline);
        assert_eq!(SectionState::TakenOver.dot(), HostDot::Offline);
        assert_eq!(SectionState::Stopped.dot(), HostDot::Offline);
    }

    #[test]
    fn only_a_live_section_is_interactive_and_only_a_dead_one_offers_reconnect() {
        for state in [SectionState::Local, SectionState::Connected] {
            assert!(state.interactive(), "{state:?}");
            assert!(!state.offers_reconnect(), "{state:?}");
        }
        for state in [
            SectionState::Connecting,
            SectionState::Disconnected,
            SectionState::NeedsRestart,
            SectionState::TakenOver,
            SectionState::Stopped,
        ] {
            assert!(!state.interactive(), "{state:?}");
            assert!(state.offers_reconnect(), "{state:?}");
        }
    }

    fn a_ring() -> Vec<RingSection> {
        vec![
            RingSection {
                host: HostId::LOCAL,
                navigable: true,
                projects: vec![1, 2],
            },
            RingSection {
                host: HostId::new(4),
                navigable: false,
                projects: vec![7, 8],
            },
            RingSection {
                host: HostId::new(5),
                navigable: true,
                projects: vec![3],
            },
        ]
    }

    #[test]
    fn the_ring_walks_top_to_bottom_across_hosts_and_skips_disconnected() {
        assert_eq!(
            ring(&a_ring()),
            vec![
                ProjectKey::local(1),
                ProjectKey::local(2),
                ProjectKey::new(HostId::new(5), 3),
            ]
        );
    }

    #[test]
    fn ring_index_is_one_based_over_the_visited_projects() {
        let sections = a_ring();
        assert_eq!(ring_index(&sections, 1), Some(ProjectKey::local(1)));
        assert_eq!(ring_index(&sections, 2), Some(ProjectKey::local(2)));
        assert_eq!(
            ring_index(&sections, 3),
            Some(ProjectKey::new(HostId::new(5), 3)),
            "the disconnected section's rows never take an index"
        );
        assert_eq!(ring_index(&sections, 4), None);
        assert_eq!(ring_index(&sections, 0), None);
    }

    #[test]
    fn ring_step_wraps_and_crosses_host_boundaries() {
        let sections = a_ring();
        assert_eq!(
            ring_step(&sections, ProjectKey::local(2), 1),
            Some(ProjectKey::new(HostId::new(5), 3)),
            "the step leaves the local section without a mode switch"
        );
        assert_eq!(
            ring_step(&sections, ProjectKey::new(HostId::new(5), 3), 1),
            Some(ProjectKey::local(1)),
            "and wraps back to the top"
        );
        assert_eq!(
            ring_step(&sections, ProjectKey::local(1), -1),
            Some(ProjectKey::new(HostId::new(5), 3))
        );
        // A selection inside the skipped section is not on the ring, so
        // the walk restarts from the top rather than answering nothing.
        assert_eq!(
            ring_step(&sections, ProjectKey::new(HostId::new(4), 7), 1),
            Some(ProjectKey::local(2))
        );
        assert_eq!(ring_step(&[], ProjectKey::local(1), 1), None);
    }
}
