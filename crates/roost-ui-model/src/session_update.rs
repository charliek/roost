//! What a connected host's session could be updated or restarted onto
//! (plan 076 D3, D4, D5).
//!
//! Pure: the adapter gathers the facts — the running session's build,
//! this client's, the connect gate's verdict, and whatever it has learned
//! about the binaries a restart could run — and these functions answer.

use roost_ipc::bootstrap::ProbeOutcome;
use roost_ipc::messages::{BuildStatus, HostRestartStatus, HostUpdateStatus, RestartTargetStatus};
use roost_ipc::session_version::{order, BuildId, VersionOrder};

use crate::host_sidebar::HostTransportKind;

/// The connect gate's verdict on the running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Ok,
    /// Connected across a libghostty build skew, on the `vt` fallback.
    ReducedFidelity,
    /// Refused. `protocol_newer` says the session speaks a newer session
    /// protocol than this client.
    Failed {
        protocol_newer: bool,
    },
}

/// Where a restart target was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSource {
    /// Shipped beside this client.
    Bundled,
    /// The binary the running session was started from.
    Running,
    /// Named by `ROOST_SESSION_BIN`.
    Override,
    /// On the host's own search path: the ssh exec rung, or a localhost
    /// `PATH` hit.
    Installed,
}

impl TargetSource {
    pub fn wire(self) -> &'static str {
        use roost_ipc::messages::host_restart_source as source;
        match self {
            Self::Bundled => source::BUNDLED,
            Self::Running => source::RUNNING,
            Self::Override => source::OVERRIDE,
            Self::Installed => source::INSTALLED,
        }
    }
}

/// The binary a restart would run, as it identified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartTarget {
    pub path: String,
    pub identity: BuildId,
    pub source: TargetSource,
    /// The running session this was resolved against.
    pub session_id: String,
    /// Which resolution produced it, so a superseded one is ignored.
    pub generation: u64,
}

/// Why no candidate is usable as a restart target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Missing,
    Unreadable,
    Older,
    /// It speaks a different session protocol than this client.
    Incompatible,
    /// `ROOST_SESSION_BIN` is set and is not a file this user can run.
    /// The override is the only candidate, so nothing else is tried.
    Override,
}

impl Why {
    pub fn wire(self) -> &'static str {
        use roost_ipc::messages::host_restart_why as why;
        match self {
            Self::Missing => why::MISSING,
            Self::Unreadable => why::UNREADABLE,
            Self::Older => why::OLDER,
            Self::Incompatible => why::INCOMPATIBLE,
            Self::Override => why::OVERRIDE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKnowledge {
    NotChecked,
    Checking,
    Found(RestartTarget),
    NoneUsable(Why),
}

/// What is known about the binary an ssh host would exec next. Learned
/// only from this client's own installs and from probes a person started
/// (D5): never in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKnowledge {
    NotChecked,
    Staged(BuildId),
    NotNeeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionUpdate {
    UpToDate,
    Available,
    Staged,
    Required,
    SessionNewer {
        blocked: bool,
    },
    Unordered,
    /// The session reported no version at all.
    Unknown,
}

impl SessionUpdate {
    pub fn wire(self) -> &'static str {
        use roost_ipc::messages::host_update_state as state;
        match self {
            Self::UpToDate => state::UP_TO_DATE,
            Self::Available => state::AVAILABLE,
            Self::Staged => state::STAGED,
            Self::Required => state::REQUIRED,
            Self::SessionNewer { .. } => state::SESSION_NEWER,
            Self::Unordered => state::UNORDERED,
            Self::Unknown => state::UNKNOWN,
        }
    }
}

/// Everything [`classify`] reads.
#[derive(Debug, Clone, Copy)]
pub struct UpdateInputs<'a> {
    pub running: &'a BuildId,
    pub client: &'a BuildId,
    pub gate: Gate,
    pub target: &'a TargetKnowledge,
    pub install: &'a InstallKnowledge,
    pub transport: HostTransportKind,
}

/// D3's precedence table; the first matching row wins.
pub fn classify(inputs: UpdateInputs<'_>) -> SessionUpdate {
    let UpdateInputs {
        running,
        client,
        gate,
        target,
        install,
        transport,
    } = inputs;
    let client_vs_running = order(client, running);

    if let Gate::Failed { protocol_newer } = gate {
        return if protocol_newer || client_vs_running == VersionOrder::Older {
            SessionUpdate::SessionNewer { blocked: true }
        } else {
            SessionUpdate::Required
        };
    }
    if client_vs_running == VersionOrder::Older {
        return SessionUpdate::SessionNewer { blocked: false };
    }
    let staged_install = matches!(install,
        InstallKnowledge::Staged(build) if order(build, running) == VersionOrder::Newer);
    let staged_target = transport == HostTransportKind::Localhost
        && matches!(target,
            TargetKnowledge::Found(found) if order(&found.identity, running) == VersionOrder::Newer);
    if staged_install || staged_target {
        return SessionUpdate::Staged;
    }
    if transport == HostTransportKind::Ssh && client_vs_running == VersionOrder::Newer {
        return SessionUpdate::Available;
    }
    if gate == Gate::ReducedFidelity {
        match transport {
            HostTransportKind::Ssh => return SessionUpdate::Available,
            HostTransportKind::Localhost => return SessionUpdate::Staged,
            HostTransportKind::Socket => {}
        }
    }
    // After the rules that need no ordering; 3–5 cannot match an empty
    // version, which orders against nothing.
    if running.version.is_empty() {
        return SessionUpdate::Unknown;
    }
    if client_vs_running == VersionOrder::Unordered {
        return SessionUpdate::Unordered;
    }
    SessionUpdate::UpToDate
}

/// Whether the Restart row is offered, and why not when it is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartOffer {
    pub offered: bool,
    pub why: Option<Why>,
    pub target: Option<RestartTarget>,
}

/// D3's note on target knowledge: it decides only whether Restart is
/// offered, never the state. Not checked yet is offered, because the
/// confirm resolves the target afresh anyway.
///
/// Three states offer no Restart whatever is known: a socket host (it is
/// somebody else's process), a session that is blocked on this client
/// being too old, and a refused ssh session, whose fix is Update.
pub fn restart_offer(
    state: SessionUpdate,
    transport: HostTransportKind,
    target: &TargetKnowledge,
) -> RestartOffer {
    let absent = RestartOffer {
        offered: false,
        why: None,
        target: None,
    };
    if transport == HostTransportKind::Socket
        || state == (SessionUpdate::SessionNewer { blocked: true })
        || (state == SessionUpdate::Required && transport == HostTransportKind::Ssh)
    {
        return absent;
    }
    match target {
        TargetKnowledge::NotChecked | TargetKnowledge::Checking => RestartOffer {
            offered: true,
            ..absent
        },
        TargetKnowledge::Found(found) => RestartOffer {
            offered: true,
            target: Some(found.clone()),
            ..absent
        },
        TargetKnowledge::NoneUsable(why) => RestartOffer {
            why: Some(*why),
            ..absent
        },
    }
}

/// A connected (or refused) host's update facts, as the band, the menus
/// and `host.status` read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFacts {
    pub state: SessionUpdate,
    pub running: BuildId,
    pub client: BuildId,
    pub restart: RestartOffer,
}

impl UpdateFacts {
    pub fn new(inputs: UpdateInputs<'_>) -> Self {
        let state = classify(inputs);
        Self {
            state,
            running: inputs.running.clone(),
            client: inputs.client.clone(),
            restart: restart_offer(state, inputs.transport, inputs.target),
        }
    }

    /// As `host.status` spells it (D7).
    pub fn status(&self) -> HostUpdateStatus {
        let build = |build: &BuildId| BuildStatus {
            version: build.version.clone(),
            dev: build.dev,
            sha: build.sha.clone(),
        };
        HostUpdateStatus {
            state: self.state.wire().to_string(),
            blocked: match self.state {
                SessionUpdate::SessionNewer { blocked } => Some(blocked),
                _ => None,
            },
            session: build(&self.running),
            client: build(&self.client),
            restart: HostRestartStatus {
                offered: self.restart.offered,
                why: self.restart.why.map(|why| why.wire().to_string()),
                target: self
                    .restart
                    .target
                    .as_ref()
                    .map(|target| RestartTargetStatus {
                        version: target.identity.version.clone(),
                        dev: target.identity.dev,
                        sha: target.identity.sha.clone(),
                        source: target.source.wire().to_string(),
                    }),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// D4: choosing the restart target
// ---------------------------------------------------------------------------

/// What running `<candidate> identify` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identified {
    Missing,
    Unreadable,
    Build(BuildId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub source: TargetSource,
    pub identified: Identified,
}

/// D4's usability test for one candidate: it identifies, it is not
/// older than the running session, and this client can talk to it.
pub fn usable(identified: &Identified, running: &BuildId, client: &BuildId) -> Result<(), Why> {
    let build = match identified {
        Identified::Missing => return Err(Why::Missing),
        Identified::Unreadable => return Err(Why::Unreadable),
        Identified::Build(build) => build,
    };
    if order(build, running) == VersionOrder::Older {
        return Err(Why::Older);
    }
    if build.protocol != client.protocol {
        return Err(Why::Incompatible);
    }
    Ok(())
}

/// D4's selection rule, over candidates already identified in its order.
/// Localhost passes the launch ladder's pick first and the running
/// session's own binary second (or `ROOST_SESSION_BIN` alone); ssh passes
/// its exec rung alone.
///
/// The first usable candidate wins unless a later usable one is strictly
/// `Newer` than it, so two `Unordered` usable candidates resolve to the
/// first. With none usable, the answer names the first candidate's
/// reason that is not `Missing` — "it is gone" only when everything is.
pub fn select_target(
    candidates: Vec<Candidate>,
    running: &BuildId,
    client: &BuildId,
) -> Result<Candidate, Why> {
    let mut chosen: Option<Candidate> = None;
    let mut why = None;
    for candidate in candidates {
        match usable(&candidate.identified, running, client) {
            Ok(()) => {
                let newer = match (&chosen, &candidate.identified) {
                    (None, _) => true,
                    (Some(best), Identified::Build(build)) => matches!(
                        &best.identified,
                        Identified::Build(held) if order(build, held) == VersionOrder::Newer
                    ),
                    (Some(_), _) => false,
                };
                if newer {
                    chosen = Some(candidate);
                }
            }
            Err(reason) => {
                if why.is_none_or(|held| held == Why::Missing) {
                    why = Some(reason);
                }
            }
        }
    }
    chosen.ok_or(why.unwrap_or(Why::Missing))
}

/// The knowledge a selection amounts to, for the session it was made
/// against.
pub fn target_knowledge(
    selected: Result<Candidate, Why>,
    session_id: &str,
    generation: u64,
) -> TargetKnowledge {
    match selected {
        Ok(Candidate {
            path,
            source,
            identified: Identified::Build(identity),
        }) => TargetKnowledge::Found(RestartTarget {
            path,
            identity,
            source,
            session_id: session_id.to_string(),
            generation,
        }),
        // `usable` admits only identified candidates.
        Ok(_) => TargetKnowledge::NoneUsable(Why::Unreadable),
        Err(why) => TargetKnowledge::NoneUsable(why),
    }
}

/// What a user-started ssh probe teaches (D4's ssh row, D5): the exec
/// rung is the only restart candidate, and it is also the build an
/// install would have staged.
pub fn ssh_knowledge(
    outcome: &ProbeOutcome,
    running: &BuildId,
    client: &BuildId,
    session_id: &str,
    generation: u64,
) -> (TargetKnowledge, InstallKnowledge) {
    let rung = match outcome {
        ProbeOutcome::Missing => None,
        ProbeOutcome::Compatible { path, identity }
        | ProbeOutcome::Mismatch {
            path,
            identity: Some(identity),
        } => Some((path, Identified::Build(BuildId::from(identity)))),
        ProbeOutcome::Mismatch {
            path,
            identity: None,
        } => Some((path, Identified::Unreadable)),
    }
    .map(|(path, identified)| Candidate {
        path: path.clone(),
        source: TargetSource::Installed,
        identified,
    });
    let install = match rung.as_ref().map(|rung| &rung.identified) {
        Some(Identified::Build(build)) => match order(build, running) {
            VersionOrder::Newer => InstallKnowledge::Staged(build.clone()),
            VersionOrder::Same => InstallKnowledge::NotNeeded,
            VersionOrder::Older | VersionOrder::Unordered => InstallKnowledge::NotChecked,
        },
        _ => InstallKnowledge::NotChecked,
    };
    let selected = select_target(rung.into_iter().collect(), running, client);
    (target_knowledge(selected, session_id, generation), install)
}

#[cfg(test)]
mod tests {
    use super::*;
    use HostTransportKind::{Localhost, Socket, Ssh};

    const PROTOCOL: u32 = 7;

    fn release(version: &str) -> BuildId {
        BuildId {
            version: version.into(),
            protocol: PROTOCOL,
            libghostty_build: "g".into(),
            ..BuildId::default()
        }
    }

    fn dev(version: &str, sha: &str) -> BuildId {
        BuildId {
            dev: true,
            sha: Some(sha.into()),
            ..release(version)
        }
    }

    fn found(identity: BuildId) -> TargetKnowledge {
        TargetKnowledge::Found(RestartTarget {
            path: "/bin/roost-session".into(),
            identity,
            source: TargetSource::Bundled,
            session_id: "s".into(),
            generation: 1,
        })
    }

    struct Row {
        running: BuildId,
        client: BuildId,
        gate: Gate,
        target: TargetKnowledge,
        install: InstallKnowledge,
        transport: HostTransportKind,
    }

    impl Row {
        fn new(running: &str, client: &str) -> Self {
            Self {
                running: release(running),
                client: release(client),
                gate: Gate::Ok,
                target: TargetKnowledge::NotChecked,
                install: InstallKnowledge::NotChecked,
                transport: Ssh,
            }
        }

        fn classify(&self) -> SessionUpdate {
            classify(UpdateInputs {
                running: &self.running,
                client: &self.client,
                gate: self.gate,
                target: &self.target,
                install: &self.install,
                transport: self.transport,
            })
        }
    }

    #[test]
    fn row_1_a_refused_session_newer_than_this_client_is_blocked() {
        let blocked = SessionUpdate::SessionNewer { blocked: true };
        // By protocol, whatever the versions say — even a client that
        // reads newer.
        for client in ["0.0.21", "0.0.22", "0.0.23"] {
            let mut row = Row::new("0.0.22", client);
            row.gate = Gate::Failed {
                protocol_newer: true,
            };
            assert_eq!(row.classify(), blocked, "client {client}");
        }
        // By version, when the protocol is not the newer side.
        let mut row = Row::new("0.0.23", "0.0.22");
        row.gate = Gate::Failed {
            protocol_newer: false,
        };
        assert_eq!(row.classify(), blocked);
        // Ahead of every later row, a staged build included.
        row.install = InstallKnowledge::Staged(release("0.0.24"));
        row.transport = Localhost;
        assert_eq!(row.classify(), blocked);
    }

    #[test]
    fn row_2_any_other_refusal_is_required() {
        for (running, client) in [
            ("0.0.19", "0.0.22"),
            ("0.0.22", "0.0.22"),
            ("0.0.22-rc1", "0.0.22"),
        ] {
            let mut row = Row::new(running, client);
            row.gate = Gate::Failed {
                protocol_newer: false,
            };
            assert_eq!(row.classify(), SessionUpdate::Required, "{running}");
            row.transport = Localhost;
            row.target = found(release("0.0.30"));
            assert_eq!(row.classify(), SessionUpdate::Required, "{running}");
        }
    }

    #[test]
    fn row_3_a_connected_newer_session_is_session_newer_unblocked() {
        let mut row = Row::new("0.0.23", "0.0.22");
        assert_eq!(
            row.classify(),
            SessionUpdate::SessionNewer { blocked: false }
        );
        // Ahead of row 4: a newer staged build does not make it staged.
        row.install = InstallKnowledge::Staged(release("0.0.24"));
        assert_eq!(
            row.classify(),
            SessionUpdate::SessionNewer { blocked: false }
        );
        row.gate = Gate::ReducedFidelity;
        assert_eq!(
            row.classify(),
            SessionUpdate::SessionNewer { blocked: false }
        );
    }

    #[test]
    fn row_4_a_newer_staged_install_or_localhost_target_is_staged() {
        let mut ssh = Row::new("0.0.21", "0.0.22");
        ssh.install = InstallKnowledge::Staged(release("0.0.22"));
        assert_eq!(ssh.classify(), SessionUpdate::Staged);
        // A staged build that is not newer than the session is not.
        ssh.install = InstallKnowledge::Staged(release("0.0.21"));
        assert_eq!(ssh.classify(), SessionUpdate::Available);

        let mut local = Row::new("0.0.21", "0.0.22");
        local.transport = Localhost;
        local.target = found(release("0.0.22"));
        assert_eq!(local.classify(), SessionUpdate::Staged);
        // Even at an equal client, the bundled build is what counts.
        local.client = release("0.0.21");
        assert_eq!(local.classify(), SessionUpdate::Staged);
        // A same-version target is not staged.
        local.target = found(release("0.0.21"));
        assert_eq!(local.classify(), SessionUpdate::UpToDate);

        // Only localhost reads the target: an ssh host's found rung is a
        // restart candidate, and its newness arrives as `install`.
        let mut ssh = Row::new("0.0.21", "0.0.21");
        ssh.target = found(release("0.0.22"));
        assert_eq!(ssh.classify(), SessionUpdate::UpToDate);
    }

    #[test]
    fn row_5_a_newer_client_over_ssh_is_available() {
        let row = Row::new("0.0.21", "0.0.22");
        assert_eq!(row.classify(), SessionUpdate::Available);
        // Localhost never installs: a newer client alone with no newer
        // target found is not an offer.
        let mut local = Row::new("0.0.21", "0.0.22");
        local.transport = Localhost;
        assert_eq!(local.classify(), SessionUpdate::UpToDate);
        let mut socket = Row::new("0.0.21", "0.0.22");
        socket.transport = Socket;
        assert_eq!(socket.classify(), SessionUpdate::UpToDate);
    }

    #[test]
    fn row_6_reduced_fidelity_splits_by_transport() {
        let mut row = Row::new("0.0.22", "0.0.22");
        row.gate = Gate::ReducedFidelity;
        assert_eq!(row.classify(), SessionUpdate::Available);
        row.transport = Localhost;
        assert_eq!(row.classify(), SessionUpdate::Staged);
        // A socket host has no action, so it falls through.
        row.transport = Socket;
        assert_eq!(row.classify(), SessionUpdate::UpToDate);
        row.client = dev("0.0.22", "a1b2c3d");
        assert_eq!(row.classify(), SessionUpdate::Unordered);
    }

    #[test]
    fn row_7_an_unordered_pair_is_unordered() {
        let mut row = Row::new("0.0.22", "0.0.22");
        row.client = dev("0.0.22", "a1b2c3d");
        assert_eq!(row.classify(), SessionUpdate::Unordered);
        row.running = dev("0.0.22", "f00ba12");
        assert_eq!(row.classify(), SessionUpdate::Unordered);
        row.transport = Localhost;
        row.target = found(dev("0.0.22", "0000000"));
        assert_eq!(row.classify(), SessionUpdate::Unordered);
    }

    #[test]
    fn row_8_everything_else_is_up_to_date() {
        assert_eq!(
            Row::new("0.0.22", "0.0.22").classify(),
            SessionUpdate::UpToDate
        );
        let mut row = Row::new("0.0.22", "0.0.22");
        row.running = dev("0.0.22", "a1b2c3d");
        row.client = dev("0.0.22", "a1b2c3d");
        assert_eq!(row.classify(), SessionUpdate::UpToDate);
        row.install = InstallKnowledge::NotNeeded;
        assert_eq!(row.classify(), SessionUpdate::UpToDate);
    }

    #[test]
    fn a_session_with_no_version_is_unknown_unless_refused() {
        let mut row = Row::new("", "0.0.22");
        assert_eq!(row.classify(), SessionUpdate::Unknown);
        row.gate = Gate::Failed {
            protocol_newer: false,
        };
        assert_eq!(row.classify(), SessionUpdate::Required);
    }

    #[test]
    fn reduced_fidelity_outranks_a_missing_version_on_both_transports() {
        let mut row = Row::new("", "0.0.22");
        row.gate = Gate::ReducedFidelity;
        assert_eq!(row.classify(), SessionUpdate::Available);
        row.transport = Localhost;
        assert_eq!(row.classify(), SessionUpdate::Staged);
        row.transport = Socket;
        assert_eq!(row.classify(), SessionUpdate::Unknown);
    }

    #[test]
    fn restart_is_offered_unless_the_state_or_the_target_rules_it_out() {
        let target = found(release("0.0.22"));
        let offer = restart_offer(SessionUpdate::Staged, Localhost, &target);
        assert!(offer.offered);
        assert!(offer.target.is_some());

        for knowledge in [TargetKnowledge::NotChecked, TargetKnowledge::Checking] {
            let offer = restart_offer(SessionUpdate::UpToDate, Ssh, &knowledge);
            assert_eq!(
                offer,
                RestartOffer {
                    offered: true,
                    why: None,
                    target: None
                }
            );
        }

        let offer = restart_offer(
            SessionUpdate::UpToDate,
            Localhost,
            &TargetKnowledge::NoneUsable(Why::Older),
        );
        assert!(!offer.offered);
        assert_eq!(offer.why, Some(Why::Older));

        for (state, transport) in [
            (SessionUpdate::UpToDate, Socket),
            (SessionUpdate::SessionNewer { blocked: true }, Localhost),
            (SessionUpdate::Required, Ssh),
        ] {
            let offer = restart_offer(state, transport, &target);
            assert!(!offer.offered, "{state:?} over {transport:?}");
            assert_eq!(offer.why, None);
        }
        assert!(restart_offer(SessionUpdate::Required, Localhost, &target).offered);
        assert!(
            restart_offer(
                SessionUpdate::SessionNewer { blocked: false },
                Localhost,
                &target
            )
            .offered
        );
    }

    fn candidate(source: TargetSource, identified: Identified) -> Candidate {
        Candidate {
            path: format!("/{}/roost-session", source.wire()),
            source,
            identified,
        }
    }

    fn build(build: BuildId) -> Identified {
        Identified::Build(build)
    }

    #[test]
    fn a_candidate_is_usable_when_it_identifies_is_not_older_and_speaks_the_protocol() {
        let running = release("0.0.22");
        let client = release("0.0.22");
        assert_eq!(
            usable(&Identified::Missing, &running, &client),
            Err(Why::Missing)
        );
        assert_eq!(
            usable(&Identified::Unreadable, &running, &client),
            Err(Why::Unreadable)
        );
        assert_eq!(
            usable(&build(release("0.0.21")), &running, &client),
            Err(Why::Older)
        );
        let other_protocol = BuildId {
            protocol: PROTOCOL + 1,
            ..release("0.0.23")
        };
        assert_eq!(
            usable(&build(other_protocol), &running, &client),
            Err(Why::Incompatible)
        );
        // A libghostty skew is fine: every session from plan 053 on
        // serves `vt`.
        let skewed = BuildId {
            libghostty_build: "other".into(),
            ..release("0.0.22")
        };
        assert_eq!(usable(&build(skewed), &running, &client), Ok(()));
        // Unordered is not a downgrade.
        assert_eq!(
            usable(&build(dev("0.0.22", "a1b2c3d")), &running, &client),
            Ok(())
        );
    }

    #[test]
    fn localhost_prefers_the_launch_pick_unless_the_running_binary_is_strictly_newer() {
        let running = release("0.0.22");
        let client = release("0.0.22");
        let select = |first: Identified, second: Identified| {
            select_target(
                vec![
                    candidate(TargetSource::Bundled, first),
                    candidate(TargetSource::Running, second),
                ],
                &running,
                &client,
            )
            .map(|chosen| chosen.source)
        };

        assert_eq!(
            select(build(release("0.0.23")), build(release("0.0.22"))),
            Ok(TargetSource::Bundled)
        );
        assert_eq!(
            select(build(release("0.0.22")), build(release("0.0.22"))),
            Ok(TargetSource::Bundled)
        );
        assert_eq!(
            select(build(release("0.0.22")), build(release("0.0.24"))),
            Ok(TargetSource::Running)
        );
        // Two usable but unordered: the launch pick.
        assert_eq!(
            select(
                build(dev("0.0.22", "a1b2c3d")),
                build(dev("0.0.22", "f00ba12"))
            ),
            Ok(TargetSource::Bundled)
        );
        // Only one usable: that one, whichever it is.
        assert_eq!(
            select(build(release("0.0.21")), build(release("0.0.22"))),
            Ok(TargetSource::Running)
        );
        assert_eq!(
            select(Identified::Missing, build(release("0.0.22"))),
            Ok(TargetSource::Running)
        );
        assert_eq!(
            select(build(release("0.0.22")), Identified::Unreadable),
            Ok(TargetSource::Bundled)
        );
    }

    #[test]
    fn with_nothing_usable_the_reason_is_the_first_one_that_is_not_missing() {
        let running = release("0.0.22");
        let client = release("0.0.22");
        let select = |candidates: Vec<Candidate>| select_target(candidates, &running, &client);

        assert_eq!(select(vec![]), Err(Why::Missing));
        assert_eq!(
            select(vec![
                candidate(TargetSource::Bundled, Identified::Missing),
                candidate(TargetSource::Running, Identified::Missing),
            ]),
            Err(Why::Missing)
        );
        assert_eq!(
            select(vec![
                candidate(TargetSource::Bundled, Identified::Missing),
                candidate(TargetSource::Running, build(release("0.0.21"))),
            ]),
            Err(Why::Older)
        );
        assert_eq!(
            select(vec![
                candidate(TargetSource::Bundled, Identified::Unreadable),
                candidate(TargetSource::Running, build(release("0.0.21"))),
            ]),
            Err(Why::Unreadable)
        );
        let incompatible = BuildId {
            protocol: PROTOCOL + 1,
            ..release("0.0.22")
        };
        assert_eq!(
            select(vec![candidate(TargetSource::Override, build(incompatible))]),
            Err(Why::Incompatible)
        );
    }

    #[test]
    fn a_selection_becomes_knowledge_about_its_session() {
        let chosen = candidate(TargetSource::Running, build(release("0.0.23")));
        assert_eq!(
            target_knowledge(Ok(chosen), "sess-9", 4),
            TargetKnowledge::Found(RestartTarget {
                path: "/running/roost-session".into(),
                identity: release("0.0.23"),
                source: TargetSource::Running,
                session_id: "sess-9".into(),
                generation: 4,
            })
        );
        assert_eq!(
            target_knowledge(Err(Why::Older), "sess-9", 4),
            TargetKnowledge::NoneUsable(Why::Older)
        );
    }

    #[test]
    fn an_ssh_probe_judges_only_the_exec_rung() {
        let running = release("0.0.21");
        let client = release("0.0.22");
        let probe = |outcome: ProbeOutcome| ssh_knowledge(&outcome, &running, &client, "s", 1);
        let identity = |version: &str| roost_ipc::messages::SessionBinaryIdentity {
            app_version: version.into(),
            session_protocol: PROTOCOL,
            libghostty_build: "g".into(),
            ..Default::default()
        };
        let path = || "/usr/bin/roost-session".to_string();

        let (target, install) = probe(ProbeOutcome::Compatible {
            path: path(),
            identity: identity("0.0.22"),
        });
        assert!(
            matches!(target, TargetKnowledge::Found(ref t) if t.source == TargetSource::Installed)
        );
        assert_eq!(install, InstallKnowledge::Staged(release("0.0.22")));

        let (target, install) = probe(ProbeOutcome::Mismatch {
            path: path(),
            identity: Some(identity("0.0.21")),
        });
        assert!(matches!(target, TargetKnowledge::Found(_)));
        assert_eq!(install, InstallKnowledge::NotNeeded);

        let (target, install) = probe(ProbeOutcome::Mismatch {
            path: path(),
            identity: Some(identity("0.0.20")),
        });
        assert_eq!(target, TargetKnowledge::NoneUsable(Why::Older));
        assert_eq!(install, InstallKnowledge::NotChecked);

        // An unidentifiable first rung never falls through to a later one.
        let (target, install) = probe(ProbeOutcome::Mismatch {
            path: path(),
            identity: None,
        });
        assert_eq!(target, TargetKnowledge::NoneUsable(Why::Unreadable));
        assert_eq!(install, InstallKnowledge::NotChecked);

        let (target, _) = probe(ProbeOutcome::Missing);
        assert_eq!(target, TargetKnowledge::NoneUsable(Why::Missing));
    }

    #[test]
    fn facts_carry_the_state_and_the_offer_together() {
        let running = release("0.0.21");
        let client = release("0.0.22");
        let target = found(release("0.0.22"));
        let facts = UpdateFacts::new(UpdateInputs {
            running: &running,
            client: &client,
            gate: Gate::Ok,
            target: &target,
            install: &InstallKnowledge::NotChecked,
            transport: Localhost,
        });
        assert_eq!(facts.state, SessionUpdate::Staged);
        assert_eq!(facts.running, running);
        assert_eq!(facts.client, client);
        assert!(facts.restart.offered);
        assert_eq!(
            facts.restart.target.as_ref().map(|t| t.source),
            Some(TargetSource::Bundled)
        );
        let status = facts.status();
        assert_eq!(status.state, "staged");
        assert_eq!(status.blocked, None);
        assert_eq!(status.session.version, "0.0.21");
        assert_eq!(status.client.version, "0.0.22");
        assert!(status.restart.offered);
        assert_eq!(
            status.restart.target.map(|t| (t.version, t.source)),
            Some(("0.0.22".to_string(), "bundled".to_string()))
        );
    }
}
